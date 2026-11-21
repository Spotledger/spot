//! The PostgreSQL stores of incoming mail (WP-PLT-11.T12).
//!
//! `spot-mail-intake` polls a mailbox; what it must not lose lives here, in the tenant's own
//! database, one table each:
//!
//! | Table | Key | Holds |
//! | --- | --- | --- |
//! | `spot_mail_intake_cursor` | `(site, account)` | `uid_validity`, `last_uid`, the highest fencing `fence` that wrote, and `updated_at` (database clock, Unix microseconds) |
//! | `spot_mail_intake_seen` | `(site, account, digest)` | `seen_at` (Unix microseconds, the instant the engine passed) |
//!
//! Rules they keep:
//!
//! - **The fence check is in the write.** [`PgCursorStore::store`](spot_mail_intake::CursorStore)
//!   is one `INSERT ... ON CONFLICT DO UPDATE ... WHERE fence <= $new` statement: a writer whose
//!   fencing token is older than the stored one changes nothing and is told so
//!   (`StoreError::StaleFence`). The check and the write cannot be separated by another node.
//! - **First record wins.** [`PgSeenStore`] inserts with `ON CONFLICT DO NOTHING`; the statement
//!   that really wrote the row answers `true`, so two nodes handling one message agree on who
//!   was first.
//! - **A digest, never a Message-ID.** The key is the hex SHA-256 the engine computed. No
//!   address, subject or id reaches this crate.
//! - **An outage is `Unavailable`, and carries nothing.** No driver message, SQLSTATE, host or
//!   statement is kept or logged.
//! - **Every value is a bound parameter**, the site and the account included; every statement is
//!   a constant. The `spot_` prefix is what `spot-pg-migrate` accepts.
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, as the other `*-pg` crates do.

#![warn(missing_docs)]

mod cursor;
pub mod schema;
mod seen;
mod sql;

pub use cursor::PgCursorStore;
pub use schema::prepare;
pub use seen::PgSeenStore;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-mail-intake-pg";

use deadpool_postgres::{Object, Pool};
use spot_mail_intake::StoreError;

/// A checked-out connection, or `Unavailable` with nothing carried.
pub(crate) async fn conn(pool: &Pool) -> Result<Object, StoreError> {
    pool.get().await.map_err(|_| StoreError::Unavailable)
}

/// A statement failure as the port's error: nothing of the driver's message is kept.
pub(crate) fn unavailable(_error: &tokio_postgres::Error) -> StoreError {
    StoreError::Unavailable
}
