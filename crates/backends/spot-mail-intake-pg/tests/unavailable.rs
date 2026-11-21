//! What the stores report when the database is gone: `Unavailable`, carrying nothing. Needs no
//! database: the pool points at a closed local port.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{PoolConfig, Runtime};
use spot_coordinator::FencingToken;
use spot_core::SiteId;
use spot_mail::MailboxCursor;
use spot_mail_intake::{CursorStore, SeenStore, StoreError};
use spot_mail_intake_pg::{PgCursorStore, PgSeenStore};
use time::OffsetDateTime;
use tokio_postgres::NoTls;

fn dead_pool() -> deadpool_postgres::Pool {
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some("127.0.0.1".to_owned());
    cfg.port = Some(1);
    cfg.dbname = Some("spot_dead_database".to_owned());
    cfg.user = Some("spot_dead_user".to_owned());
    cfg.pool = Some(PoolConfig::new(1));
    cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap()
}

fn site() -> SiteId {
    SiteId::new("head.office").unwrap()
}

#[tokio::test]
async fn every_method_reports_a_bare_unavailable() {
    let cursors = PgCursorStore::new(dead_pool(), site());
    let seen = PgSeenStore::new(dead_pool(), site());
    let cursor = MailboxCursor {
        uid_validity: 1,
        last_uid: 1,
    };
    assert_eq!(cursors.load("a").await, Err(StoreError::Unavailable));
    assert_eq!(
        cursors.store("a", cursor, FencingToken(1)).await,
        Err(StoreError::Unavailable)
    );
    assert_eq!(seen.contains("a", "d").await, Err(StoreError::Unavailable));
    assert_eq!(
        seen.first_time("a", "d", OffsetDateTime::UNIX_EPOCH).await,
        Err(StoreError::Unavailable)
    );
    assert_eq!(
        seen.purge_older_than(OffsetDateTime::UNIX_EPOCH).await,
        Err(StoreError::Unavailable)
    );
    assert_eq!(cursors.prepare().await, Err(StoreError::Unavailable));

    let text = StoreError::Unavailable.to_string();
    for forbidden in [
        "127.0.0.1",
        "spot_dead_database",
        "spot_dead_user",
        "refused",
    ] {
        assert!(!text.contains(forbidden), "{text} leaks {forbidden}");
    }
    assert!(!format!("{cursors:?}{seen:?}").contains("127.0.0.1"));
}
