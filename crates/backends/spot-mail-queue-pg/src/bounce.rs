//! [`PgBounceSink`]: the durable, shared [`BounceSink`] (WP-PLT-11.T11).
//!
//! `spot-mail-queue` ships [`InMemoryBounceSink`](spot_mail_queue::InMemoryBounceSink), which is
//! per process: a restart forgets every bounce and a second node never saw them. This sink keeps
//! them in one table, `spot_mail_bounce_log`, created by [`PgBounceSink::prepare`] (the `spot_`
//! prefix is the rule `spot_pg_migrate` and the table-prefix lint require, the same as
//! `spot_mail_sent_log`):
//!
//! | Column | Holds |
//! | --- | --- |
//! | `site`, `job_id`, `recipient`, `class` | the record's key (`BounceRecord::key`); the primary key |
//! | `message_id` | the message id |
//! | `code` | the stable catalogue code (`mail_queue.*` or `egress.*`) |
//! | `reply_code` | the SMTP reply code, when the server produced one |
//! | `attempts` | how many attempts ran out, for `retries_exhausted` |
//! | `recorded_at` | the record's own instant (the job's clock), Unix microseconds |
//!
//! Rules it keeps:
//!
//! - **`record` is one statement and first write wins.** `INSERT ... ON CONFLICT (site, job_id,
//!   recipient, class) DO NOTHING` answers `Ok(())` whether or not it inserted; the first record
//!   stays exactly as written, so a retry with another instant or attempt count changes nothing.
//!   There is no read before the write.
//! - **Nothing but the record's facts is stored**: never the message body, subject or the
//!   server's reply text.
//! - **An outage is `Unavailable`** (a value with no fields): no driver text, SQL, host or
//!   address is carried, and this store writes no log line, so no recipient address reaches a log.
//! - **`records_for`** is a bounded read-only listing of one site's one job (at most 1,000 rows).
//!
//! The job id is stored as its decimal text so no `u64` is ever refused; the table is in the
//! tenant's own database and every row carries its site, as the sent log does.

use deadpool_postgres::{Object, Pool};
use spot_app_api::{AppError, AppResult, ErrorClass};
use spot_core::SiteId;
use spot_mail_queue::{BounceClass, BounceRecord, BounceSink, Unavailable};

use crate::convert::micros;
use crate::error;

/// The most rows `records_for` returns.
pub const MAX_LISTED: i64 = 1_000;

/// The lock key that serialises concurrent `prepare` calls (transaction scoped).
const PREPARE_LOCK: i64 = 0x4D41_494C_0007;

/// The idempotent DDL. Every statement can run again.
pub const BOUNCE_DDL: &str = "CREATE TABLE IF NOT EXISTS spot_mail_bounce_log (\
    site text NOT NULL, \
    job_id text NOT NULL, \
    recipient text NOT NULL, \
    class text NOT NULL, \
    message_id text NOT NULL, \
    code text NOT NULL, \
    reply_code integer, \
    attempts integer, \
    recorded_at bigint NOT NULL, \
    PRIMARY KEY (site, job_id, recipient, class))";

/// First write wins; the conflict is resolved by the primary key inside the database.
const RECORD: &str = "INSERT INTO spot_mail_bounce_log \
    (site, job_id, recipient, class, message_id, code, reply_code, attempts, recorded_at) \
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
    ON CONFLICT (site, job_id, recipient, class) DO NOTHING";

/// One site's one job, in key order, bounded.
const LIST: &str = "SELECT recipient, class, message_id, code, reply_code, attempts, recorded_at \
    FROM spot_mail_bounce_log WHERE site = $1 AND job_id = $2 \
    ORDER BY recipient, class LIMIT $3";

/// The stored text of a class.
pub(crate) fn class_text(class: BounceClass) -> &'static str {
    match class {
        BounceClass::Rejected => "rejected",
        BounceClass::Deferred => "deferred",
        BounceClass::Authentication => "authentication",
        BounceClass::InvalidMessage => "invalid_message",
        BounceClass::EgressDenied => "egress_denied",
        BounceClass::Attachment => "attachment",
        BounceClass::Configuration => "configuration",
        BounceClass::RetriesExhausted => "retries_exhausted",
    }
}

/// The class a stored text names, if this version knows it.
fn class_of(text: &str) -> Option<BounceClass> {
    [
        BounceClass::Rejected,
        BounceClass::Deferred,
        BounceClass::Authentication,
        BounceClass::InvalidMessage,
        BounceClass::EgressDenied,
        BounceClass::Attachment,
        BounceClass::Configuration,
        BounceClass::RetriesExhausted,
    ]
    .into_iter()
    .find(|c| class_text(*c) == text)
}

/// One stored bounce, as `records_for` returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBounce {
    /// The recipient address that did not get the message.
    pub recipient: String,
    /// The failure class.
    pub class: BounceClass,
    /// The message id.
    pub message_id: String,
    /// The stable catalogue code.
    pub code: String,
    /// The SMTP reply code, when there was one.
    pub reply_code: Option<u16>,
    /// The attempts that ran out, for `RetriesExhausted`.
    pub attempts: Option<u32>,
    /// The record's instant, Unix microseconds.
    pub recorded_at: i64,
}

/// The bounce log in PostgreSQL. Call [`PgBounceSink::prepare`] once at start-up; clones share
/// the pool, and a second node needs nothing but the same database.
#[derive(Clone)]
pub struct PgBounceSink {
    pool: Pool,
}

impl std::fmt::Debug for PgBounceSink {
    /// Shows no connection detail.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgBounceSink").finish_non_exhaustive()
    }
}

impl PgBounceSink {
    /// A sink on `pool`. It does not touch the database until used.
    #[must_use]
    pub fn new(pool: Pool) -> Self {
        Self { pool }
    }

    /// Creates the table if it is missing. Idempotent and safe to run from several nodes at once.
    ///
    /// # Errors
    /// `mail_queue_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> AppResult<()> {
        let fail = |why: &str| {
            AppError::new(
                ErrorClass::Internal,
                crate::MIGRATION_FAILED,
                format!("the bounce log's table could not be created: {why}"),
            )
        };
        let mut conn = self
            .pool
            .get()
            .await
            .map_err(|_| fail("no database connection is available"))?;
        let tx = conn
            .transaction()
            .await
            .map_err(|_| fail("the database refused the transaction"))?;
        tx.execute("SELECT pg_advisory_xact_lock($1)", &[&PREPARE_LOCK])
            .await
            .map_err(|_| fail("the database refused the lock"))?;
        tx.batch_execute(BOUNCE_DDL)
            .await
            .map_err(|_| fail("the database refused the DDL"))?;
        tx.commit()
            .await
            .map_err(|_| fail("the database refused the commit"))
    }

    /// The bounces of one job of `site`, in recipient and class order, at most [`MAX_LISTED`].
    /// Read only; another site's rows are never returned.
    ///
    /// # Errors
    /// `Unavailable` when the database cannot be reached, answers with an error, or holds a row
    /// this version cannot read.
    pub async fn records_for(
        &self,
        site: &SiteId,
        job_id: u64,
    ) -> Result<Vec<StoredBounce>, Unavailable> {
        let conn = self.conn().await?;
        let job = job_id.to_string();
        let rows = conn
            .query(LIST, &[&site.as_str(), &job, &MAX_LISTED])
            .await
            .map_err(|e| error::unavailable(&e))?;
        rows.iter()
            .map(|row| {
                let class_name: String = row.get(1);
                let reply: Option<i32> = row.get(4);
                let attempts: Option<i32> = row.get(5);
                Ok(StoredBounce {
                    recipient: row.get(0),
                    class: class_of(&class_name).ok_or_else(error::unreadable)?,
                    message_id: row.get(2),
                    code: row.get(3),
                    reply_code: reply
                        .map(|c| u16::try_from(c).map_err(|_| error::unreadable()))
                        .transpose()?,
                    attempts: attempts
                        .map(|a| u32::try_from(a).map_err(|_| error::unreadable()))
                        .transpose()?,
                    recorded_at: row.get(6),
                })
            })
            .collect()
    }

    async fn conn(&self) -> Result<Object, Unavailable> {
        self.pool
            .get()
            .await
            .map_err(|e| error::unavailable_pool(&e))
    }
}

#[async_trait::async_trait]
impl BounceSink for PgBounceSink {
    async fn record(&self, record: BounceRecord) -> Result<(), Unavailable> {
        let at = micros(record.at)?;
        let reply = record.reply_code.map(i32::from);
        let attempts = record
            .attempts
            .map(|a| i32::try_from(a).map_err(|_| error::unreadable()))
            .transpose()?;
        let job = record.job_id.0.to_string();
        let conn = self.conn().await?;
        // One statement: the primary key decides. A key the table holds changes nothing.
        conn.execute(
            RECORD,
            &[
                &record.site.as_str(),
                &job,
                &record.recipient,
                &class_text(record.class),
                &record.message_id,
                &record.code,
                &reply,
                &attempts,
                &at,
            ],
        )
        .await
        .map_err(|e| error::unavailable(&e))?;
        Ok(())
    }
}
