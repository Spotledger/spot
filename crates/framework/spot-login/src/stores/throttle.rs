//! [`StorageThrottleStore`]: sign-in throttle counters over records, not over memory.
//!
//! # The two counters
//!
//! - **Per account.** The platform app's `user` record already carries `failed_attempts`,
//!   `last_failed_at` and `locked_until` (read-only, no permission opens them), so the account
//!   counter is written where an operator and the audit already look for it: one short
//!   transaction that loads the row under a write lock, replaces the three counters and commits.
//!   The account is resolved through the **same name-or-email lookup** the user directory signs
//!   in with, so lockout and backoff engage whichever identifier is presented.
//!   A name that belongs to no user has no row to hold a counter, so each failure counted on it
//!   is appended as a `login_attempt` row of its own kind (`account` = the presented name, which
//!   "need not belong to a user", result `login.failure_counted`, source digest
//!   [`ACCOUNT_COUNTER_SOURCE`], which no source counter ever reads), and `get` replays those
//!   rows with the same window, backoff threshold and lockout as a real account's row
//!   ([`derive_unknown_account`]). The service's own attempt rows are not read: they also
//!   record refusals that count nothing, and the host's event sink may not store them. Without this an unknown name never backs off or locks while a
//!   real one does, and the answer tells an attacker which names exist. The admission port
//!   (`CoordinatorAdmission`, a bucket per account name on the shared coordinator) counts both
//!   alike as well. There is no in-process map to fill or evict.
//!   The two limits meet like this: the coordinator's account bucket is a **rate** that refills,
//!   the user-row counters are a **state** (backoff, then a lock) an operator can read and clear.
//!   Neither is set from source evidence alone: only failures on this account write the row.
//! - **Per source.** There is no source-counter record, and inventing one is not in an owned
//!   path. The counter is therefore **derived** from the append-only `login_attempt` rows the
//!   service already writes for every attempt: the failures of one source digest inside the
//!   throttle window. `put` and `clear` for the source kind write nothing, because the attempts
//!   *are* the state; a success never washes out a source's failures (ADR-070 §7), so there is
//!   nothing to clear either.
//!
//! The window comes from [`ThrottleConfig`](spot_sessions::ThrottleConfig) at construction: the
//! derived counter counts only the failures of the last `window` seconds, so a quiet source
//! slides back to zero exactly as an in-memory counter would.
//!
//! No value here is a credential: the source is already a salted digest and the counters are
//! integers and instants.

use std::sync::Arc;
use std::time::Duration;

use spot_platform_app::doctype::{LOGIN_ATTEMPT, USER};
use spot_sessions::throttle::{FailureRecord, ThrottleError, ThrottleStore};
use spot_storage_contract::{
    DocumentData, DocumentStore, Expr, Fields, Name, Nulls, Order, OrderDir, Query, QueryExecutor,
    QueryPath, RowLock, Selection, StorageBackend, Transaction, Value, Wait,
};

use super::{LoginStores, instant, name, seconds, text};

/// The most attempt rows one derived source counter reads (a bound on the read, not on the
/// counter: a source with more failures than this inside a window is throttled anyway).
pub const SOURCE_WINDOW_ROWS: u32 = 512;

/// The `source_digest` of the rows that hold a counted failure of a name that belongs to nobody.
/// It is not a digest: no source counter's key renders to it, so those rows are never read as a
/// source's failures.
pub const ACCOUNT_COUNTER_SOURCE: &str = "account-counter";

/// The result code of those rows.
pub const RESULT_FAILURE_COUNTED: &str = "login.failure_counted";

/// Throttle counters over the platform app's records.
pub struct StorageThrottleStore<B: StorageBackend> {
    stores: Arc<LoginStores<B>>,
    window: Duration,
    /// Failures inside the window that lock an account, and for how long (the sign-in
    /// throttle's `lockout_after` and `lockout_duration`): a name that belongs to nobody is
    /// locked by the same rule as a real one. `None` derives backoff only.
    lockout: Option<(u32, Duration)>,
}

impl<B: StorageBackend> StorageThrottleStore<B> {
    /// A store over one site's engine and capability, reading source failures inside `window`.
    #[must_use]
    pub fn new(stores: Arc<LoginStores<B>>, window: Duration) -> Self {
        Self {
            stores,
            window,
            lockout: None,
        }
    }

    /// Applies the sign-in throttle's lockout (`after` failures inside the window lock the
    /// account for `duration`) to a name that belongs to nobody, as it applies to a real
    /// account's row.
    #[must_use]
    pub fn with_lockout(mut self, after: u32, duration: Duration) -> Self {
        self.lockout = Some((after, duration));
        self
    }

    fn column(text: &str) -> Result<Name, ThrottleError> {
        name(text).map_err(|error| ThrottleError::StoreFailed(error.store_text()))
    }

    fn entity(text: &str) -> Result<Name, ThrottleError> {
        name(text).map_err(|error| ThrottleError::StoreFailed(error.store_text()))
    }

    fn label(suffix: &str) -> String {
        format!("spot_login_{suffix}")
    }

    fn int_at(fields: &Fields, field: &str) -> i64 {
        fields
            .iter()
            .find(|(name, _)| name.as_str() == field)
            .and_then(|(_, value)| match value {
                Value::Int(number) => Some(*number),
                _ => None,
            })
            .unwrap_or(0)
    }

    fn seconds_at(fields: &Fields, field: &str) -> i64 {
        fields
            .iter()
            .find(|(name, _)| name.as_str() == field)
            .and_then(|(_, value)| seconds(value))
            .unwrap_or(0)
    }

    /// The id of the user whose sign-in name **or e-mail** is `account`, case-insensitively:
    /// the same lookup the user directory signs in through, so an account is throttled under
    /// whichever identifier it is presented with.
    async fn user_id(
        &self,
        account: &str,
    ) -> Result<Option<spot_storage_contract::RecordId>, ThrottleError> {
        super::user::find_user_id(&self.stores, "throttle_user", account)
            .await
            .map_err(|error| ThrottleError::StoreFailed(error.store_text()))
    }

    /// The account counter, from the user's own fields.
    async fn account_record(&self, account: &str) -> Result<FailureRecord, ThrottleError> {
        let Some(id) = self.user_id(account).await? else {
            // No row to hold a counter: derive it from the attempts of the name.
            return self.unknown_account_record(account).await;
        };
        let mut tx = self
            .stores
            .begin(&Self::label("throttle_account"), false)
            .await
            .map_err(|error| ThrottleError::StoreFailed(error.store_text()))?;
        let entity = Self::entity(USER)?;
        let loaded = tx.load(&entity, id, RowLock::None).await;
        let _ = tx.rollback().await;
        let Some(loaded) = loaded.map_err(|error| {
            ThrottleError::StoreFailed(super::failed_text("throttle load", &error))
        })?
        else {
            return Ok(FailureRecord::default());
        };
        let fields = &loaded.data.fields;
        Ok(FailureRecord {
            failures: u32::try_from(Self::int_at(fields, "failed_attempts")).unwrap_or(u32::MAX),
            window_start: Self::seconds_at(fields, "last_failed_at"),
            locked_until: Self::seconds_at(fields, "locked_until"),
            last_failure: Self::seconds_at(fields, "last_failed_at"),
        })
    }

    /// Writes the account counter back onto the user record.
    async fn write_account(
        &self,
        account: &str,
        record: FailureRecord,
    ) -> Result<(), ThrottleError> {
        let Some(id) = self.user_id(account).await? else {
            // No user record: a counted failure is appended as a counter row (see the module
            // docs); clearing (nothing counts for a name that cannot sign in) writes nothing.
            if record.failures > 0 {
                self.append_unknown_failure(account, record.last_failure)
                    .await?;
            }
            return Ok(());
        };
        let mut tx = self
            .stores
            .begin(&Self::label("throttle_write"), true)
            .await
            .map_err(|error| ThrottleError::StoreFailed(error.store_text()))?;
        let entity = Self::entity(USER)?;
        let loaded = tx.load(&entity, id, RowLock::Update(Wait::Block)).await;
        let Some(mut loaded) = loaded.map_err(|error| {
            ThrottleError::StoreFailed(super::failed_text("throttle load", &error))
        })?
        else {
            let _ = tx.rollback().await;
            return Ok(());
        };
        let date = |seconds: i64| -> Result<Value, ThrottleError> {
            if seconds == 0 {
                return Ok(Value::Null);
            }
            instant(seconds)
                .map(Value::DateTime)
                .map_err(|error| ThrottleError::StoreFailed(error.store_text()))
        };
        loaded.data.fields.insert(
            Self::column("failed_attempts")?,
            Value::Int(i64::from(record.failures)),
        );
        loaded
            .data
            .fields
            .insert(Self::column("last_failed_at")?, date(record.last_failure)?);
        loaded
            .data
            .fields
            .insert(Self::column("locked_until")?, date(record.locked_until)?);
        let updated = tx.update(&loaded.data, loaded.version).await;
        match updated {
            Ok(_) => tx.commit().await.map_err(|error| {
                ThrottleError::StoreFailed(super::failed_text("commit throttle", &error))
            }),
            Err(error) => {
                let _ = tx.rollback().await;
                Err(ThrottleError::StoreFailed(super::failed_text(
                    "throttle write",
                    &error,
                )))
            }
        }
    }

    /// Appends the counter row of one failure counted on a name that belongs to nobody.
    async fn append_unknown_failure(&self, account: &str, at: i64) -> Result<(), ThrottleError> {
        let instant =
            instant(at).map_err(|error| ThrottleError::StoreFailed(error.store_text()))?;
        let mut fields = Fields::new();
        fields.insert(Self::column("account")?, Value::Text(account.to_owned()));
        fields.insert(
            Self::column("source_digest")?,
            Value::Text(ACCOUNT_COUNTER_SOURCE.to_owned()),
        );
        fields.insert(
            Self::column("result_code")?,
            Value::Text(RESULT_FAILURE_COUNTED.to_owned()),
        );
        fields.insert(Self::column("at")?, Value::DateTime(instant));
        let doc = DocumentData {
            entity: Self::entity(LOGIN_ATTEMPT)?,
            id: spot_storage_contract::RecordId::new_v7(),
            fields,
            children: Default::default(),
        };
        let mut tx = self
            .stores
            .begin(&Self::label("throttle_unknown_write"), true)
            .await
            .map_err(|error| ThrottleError::StoreFailed(error.store_text()))?;
        if let Err(error) = tx.insert(&doc).await {
            let _ = tx.rollback().await;
            return Err(ThrottleError::StoreFailed(super::failed_text(
                "throttle unknown account",
                &error,
            )));
        }
        tx.commit().await.map_err(|error| {
            ThrottleError::StoreFailed(super::failed_text("commit throttle", &error))
        })?;
        Ok(())
    }

    /// The counter of a name that belongs to nobody, replayed from its counter rows.
    async fn unknown_account_record(&self, account: &str) -> Result<FailureRecord, ThrottleError> {
        let entity = Self::entity(LOGIN_ATTEMPT)?;
        let alias = Self::column("a")?;
        let mut query = Query::from(&entity, &alias, QueryPath::Report);
        query.select = vec![Selection {
            expr: Expr::field(&alias, &Self::column("at")?),
            alias: Self::column("at")?,
        }];
        query.filter = Some(Expr::And(vec![
            Expr::field(&alias, &Self::column("account")?)
                .eq(Expr::value(Value::Text(account.to_owned()))),
            Expr::field(&alias, &Self::column("result_code")?)
                .eq(Expr::value(Value::Text(RESULT_FAILURE_COUNTED.to_owned()))),
        ]));
        query.order_by = vec![Order {
            expr: Expr::field(&alias, &Self::column("at")?),
            dir: OrderDir::Desc,
            nulls: Nulls::Last,
        }];
        query.limit = Some(SOURCE_WINDOW_ROWS);
        let mut tx = self
            .stores
            .begin(&Self::label("throttle_unknown"), false)
            .await
            .map_err(|error| ThrottleError::StoreFailed(error.store_text()))?;
        let rows = tx.query(&query).await;
        let _ = tx.rollback().await;
        let rows = rows
            .map_err(|error| {
                ThrottleError::StoreFailed(super::failed_text("throttle unknown account", &error))
            })?
            .rows;
        let mut failures: Vec<i64> = rows
            .iter()
            .filter_map(|row| row.first().and_then(seconds).filter(|at| *at != 0))
            .collect();
        failures.reverse();
        Ok(derive_unknown_account(&failures, self.window, self.lockout))
    }

    /// The source counter, derived from the `login_attempt` rows inside the window.
    async fn source_record(&self, key: &[u8]) -> Result<FailureRecord, ThrottleError> {
        let digest = source_digest_text(key);
        let entity = Self::entity(LOGIN_ATTEMPT)?;
        let alias = Self::column("a")?;
        let mut query = Query::from(&entity, &alias, QueryPath::Report);
        query.select = vec![
            Selection {
                expr: Expr::field(&alias, &Self::column("at")?),
                alias: Self::column("at")?,
            },
            Selection {
                expr: Expr::field(&alias, &Self::column("result_code")?),
                alias: Self::column("result")?,
            },
        ];
        query.filter = Some(
            Expr::field(&alias, &Self::column("source_digest")?)
                .eq(Expr::value(Value::Text(digest))),
        );
        query.order_by = vec![Order {
            expr: Expr::field(&alias, &Self::column("at")?),
            dir: OrderDir::Desc,
            nulls: Nulls::Last,
        }];
        query.limit = Some(SOURCE_WINDOW_ROWS);
        let mut tx = self
            .stores
            .begin(&Self::label("throttle_source"), false)
            .await
            .map_err(|error| ThrottleError::StoreFailed(error.store_text()))?;
        let rows = tx.query(&query).await;
        let _ = tx.rollback().await;
        let rows = rows
            .map_err(|error| {
                ThrottleError::StoreFailed(super::failed_text("throttle source", &error))
            })?
            .rows;
        let mut failures = 0u32;
        let mut newest = 0i64;
        let mut oldest = 0i64;
        let window = i64::try_from(self.window.as_secs()).unwrap_or(i64::MAX);
        for row in rows {
            let at = row.first().and_then(seconds).unwrap_or(0);
            let result = row.get(1).and_then(text).unwrap_or_default();
            if at == 0 || result == crate::service::RESULT_OK {
                continue;
            }
            if newest == 0 {
                newest = at;
            }
            if newest - at > window {
                // Ordered newest first: everything after this is older still.
                break;
            }
            failures = failures.saturating_add(1);
            oldest = at;
        }
        Ok(FailureRecord {
            failures,
            // Measured from the newest failure, so an active source cannot age its failures
            // out while it keeps trying.
            window_start: newest,
            locked_until: 0,
            last_failure: if oldest == 0 { newest } else { oldest },
        })
    }
}

/// The counter a user row would hold for a name that belongs to nobody: replays the failures
/// (instants in ascending order) with the service's own rules. The first failure of a window
/// opens it (`window_start`), a failure after the window has passed starts a new one, the
/// `lockout.0`-th failure inside a window locks until its instant plus `lockout.1`, and a lock
/// that has passed is cleared by the next failure before it counts, as `check` clears it.
#[must_use]
pub fn derive_unknown_account(
    failures: &[i64],
    window: Duration,
    lockout: Option<(u32, Duration)>,
) -> FailureRecord {
    let window = i64::try_from(window.as_secs()).unwrap_or(i64::MAX);
    let mut record = FailureRecord::default();
    for &at in failures {
        if record.locked_until != 0 && at >= record.locked_until {
            record = FailureRecord::default();
        }
        let live = record.failures > 0 && at - record.window_start < window;
        let count = (if live { record.failures } else { 0 }) + 1;
        record.window_start = if count == 1 { at } else { record.window_start };
        record.failures = count;
        record.last_failure = at;
        if let Some((after, duration)) = lockout
            && count >= after
        {
            record.locked_until =
                at.saturating_add(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX));
        }
    }
    record
}

/// The `source_digest` text of a source key: the digest's first eight bytes in base16, exactly
/// as [`spot_sessions::SourceHash`]'s `Display` renders it for the audit row.
fn source_digest_text(key: &[u8]) -> String {
    super::hex(&key[..key.len().min(8)])
}

impl<B: StorageBackend> ThrottleStore for StorageThrottleStore<B> {
    fn get<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<FailureRecord, ThrottleError>> + Send + 'a>,
    > {
        Box::pin(async move {
            if account {
                let name = account_text(key);
                self.account_record(&name).await
            } else {
                self.source_record(key).await
            }
        })
    }

    fn put<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
        record: FailureRecord,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ThrottleError>> + Send + 'a>>
    {
        Box::pin(async move {
            if account {
                let name = account_text(key);
                self.write_account(&name, record).await
            } else {
                // The attempt rows are the source counter (see the module docs).
                Ok(())
            }
        })
    }

    fn clear<'a>(
        &'a self,
        account: bool,
        key: &'a [u8],
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), ThrottleError>> + Send + 'a>>
    {
        Box::pin(async move {
            if account {
                let name = account_text(key);
                self.write_account(&name, FailureRecord::default()).await
            } else {
                Ok(())
            }
        })
    }
}

/// An account key's text (the normalised sign-in name the throttle keys on).
fn account_text(key: &[u8]) -> String {
    String::from_utf8_lossy(key).into_owned()
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{derive_unknown_account, source_digest_text};

    const WINDOW: Duration = Duration::from_secs(60);
    const LOCK: Option<(u32, Duration)> = Some((3, Duration::from_secs(60)));

    #[test]
    fn an_unknown_name_backs_off_and_locks_like_a_real_row() {
        // Two failures: the counter the backoff threshold (2) reads.
        let two = derive_unknown_account(&[100, 101], WINDOW, LOCK);
        assert_eq!(
            (two.failures, two.window_start, two.locked_until),
            (2, 100, 0)
        );
        // The third inside the window locks until its instant plus the duration.
        let three = derive_unknown_account(&[100, 101, 102], WINDOW, LOCK);
        assert_eq!((three.failures, three.locked_until), (3, 162));
        // Without a lockout setting only the counter exists.
        let none = derive_unknown_account(&[100, 101, 102], WINDOW, None);
        assert_eq!((none.failures, none.locked_until), (3, 0));
    }

    #[test]
    fn a_passed_window_or_lock_starts_the_counter_afresh() {
        // The second failure is after the window: a new window of one.
        let late = derive_unknown_account(&[100, 200], WINDOW, LOCK);
        assert_eq!((late.failures, late.window_start), (1, 200));
        // A failure after the lock has passed is the first of a clean counter.
        let after = derive_unknown_account(&[100, 101, 102, 170], WINDOW, LOCK);
        assert_eq!(
            (after.failures, after.locked_until, after.window_start),
            (1, 0, 170)
        );
        assert_eq!(
            derive_unknown_account(&[], WINDOW, LOCK),
            Default::default()
        );
    }

    #[test]
    fn the_source_key_renders_the_audit_digest() {
        let key = [0xab, 0xcd, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0xff, 0xfe];
        assert_eq!(source_digest_text(&key), "abcd010203040506");
        assert_eq!(source_digest_text(&[1, 2]), "0102");
    }
}
