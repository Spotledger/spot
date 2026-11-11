//! What the store reports when the database is gone — the state that decides whether the queue
//! sends at all.
//!
//! The `SentLog` port fails with `Unavailable`, a value that carries nothing. That is the whole
//! point of these tests: `MailSender` reads the log before every attempt and turns an error into a
//! retry, so a database outage must arrive as an error and never as "not sent" (which would send
//! an unrecorded message) and never carry the host, the database name or a bound value into a log
//! line. The sender's side of that — a job that retries with `mail_queue.sent_log_unavailable`
//! and a transport that is never asked to send — is
//! `spot-mail-queue`'s `backing_outages_retry_before_anything_is_sent`; what this file proves is
//! that this store hands the sender exactly that error, for every method of the port, when the
//! database is gone. Needs no database: the pool points at a closed local port.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use deadpool_postgres::{PoolConfig, Runtime};
use spot_app_api::ErrorClass;
use spot_core::SiteId;
use spot_mail_queue::{SentLog, Unavailable};
use spot_mail_queue_pg::{MIGRATION_FAILED, PgSentLog};
use tokio_postgres::NoTls;

/// A log whose pool can never connect.
fn dead_log() -> PgSentLog {
    let mut cfg = deadpool_postgres::Config::new();
    cfg.host = Some("127.0.0.1".to_owned());
    cfg.port = Some(1);
    cfg.dbname = Some("spot_dead_database".to_owned());
    cfg.user = Some("spot_dead_user".to_owned());
    cfg.pool = Some(PoolConfig::new(1));
    PgSentLog::new(cfg.create_pool(Some(Runtime::Tokio1), NoTls).unwrap())
}

fn site() -> SiteId {
    SiteId::new("head.office").unwrap_or_else(|e| panic!("{e}"))
}

fn receipt() -> spot_mail::SendReceipt {
    spot_mail::SendReceipt {
        message_id: "outage@head.office".to_owned(),
        accepted: vec!["buyer@customer.test".parse().unwrap()],
        rejected: Vec::new(),
    }
}

/// The error carries nothing: no host, no database name, no SQLSTATE, no statement.
fn assert_bare(error: &Unavailable) {
    let text = error.to_string();
    assert_eq!(text, "backing service unavailable");
    for forbidden in [
        "127.0.0.1",
        "spot_dead_database",
        "spot_dead_user",
        "refused",
        "connect",
        "SQLSTATE",
        "spot_mail_sent_log",
        "outage@head.office",
    ] {
        assert!(!text.contains(forbidden), "{text} leaks {forbidden}");
    }
    assert_eq!(format!("{error:?}"), "Unavailable");
}

#[tokio::test]
async fn reading_the_log_on_a_dead_pool_is_unavailable_never_not_sent() {
    let error = dead_log()
        .is_sent(&site(), "outage@head.office")
        .await
        .expect_err("a database that is gone cannot answer `not sent`");
    assert_bare(&error);
}

#[tokio::test]
async fn recording_on_a_dead_pool_is_unavailable() {
    let error = dead_log()
        .record_sent(&site(), "outage@head.office", &receipt())
        .await
        .expect_err("a send that cannot be recorded must not report success");
    assert_bare(&error);
}

#[tokio::test]
async fn reading_a_receipt_on_a_dead_pool_is_unavailable() {
    let error = dead_log()
        .recorded(&site(), "outage@head.office")
        .await
        .expect_err("a database that is gone cannot answer `no receipt`");
    assert_bare(&error);
}

#[tokio::test]
async fn preparing_a_dead_pool_reports_the_catalogue_code_without_driver_text() {
    let error = dead_log()
        .prepare()
        .await
        .expect_err("a schema that cannot be created must fail loudly");
    assert_eq!(error.code, MIGRATION_FAILED);
    assert_eq!(error.class, ErrorClass::Internal);
    assert!(!error.retryable());
    let text = error.to_string();
    for forbidden in [
        "127.0.0.1",
        "spot_dead_database",
        "spot_dead_user",
        "refused",
    ] {
        assert!(!text.contains(forbidden), "{text} leaks {forbidden}");
    }
}

#[tokio::test]
async fn the_store_never_shows_its_pool() {
    let shown = format!("{:?}", dead_log());
    assert_eq!(shown, "PgSentLog { .. }");
    for forbidden in ["127.0.0.1", "spot_dead_database", "spot_dead_user"] {
        assert!(!shown.contains(forbidden), "{shown} leaks {forbidden}");
    }
}
