//! The PostgreSQL [`SentLog`](spot_mail_queue::SentLog) (WP-PLT-11.T6).
//!
//! The sent log is what stops one message from being sent twice. `spot-mail-queue` ships
//! [`InMemorySentLog`](spot_mail_queue::InMemorySentLog), which is per process: a restart forgets
//! every receipt, and a second node never saw them, so a job retried after the SMTP server
//! accepted the message but before the job finished sends it again. [`PgSentLog`] makes the log
//! durable and shared. It keeps one table, created by [`PgSentLog::prepare`] from the DDL in
//! [`schema`]:
//!
//! | Column | Holds |
//! | --- | --- |
//! | `site` | the site that owns the message; with `message_id` it is the whole key |
//! | `message_id` | the message id the transport reported, without angle brackets |
//! | `recorded_at` | when the database recorded the receipt (Unix microseconds, the database clock) |
//! | `accepted` | the recipients the server accepted, in the transport's order |
//! | `rejected` | the recipients the server refused, in the transport's order |
//! | `rejected_code` | the SMTP reply code of each rejected recipient; position for position with `rejected` |
//!
//! The primary key is `(site, message_id)`, so two sites never share a receipt and two writes of
//! one message never both win.
//!
//! Rules it keeps:
//!
//! - **`record_sent` is one statement and first write wins.** `INSERT … ON CONFLICT (site,
//!   message_id) DO NOTHING`: the insert that really wrote a row answers `true`, every later one
//!   for the same key answers `false` and leaves the stored receipt untouched. The decision is
//!   made by the primary key inside the database, not by anything this process reads first, so
//!   two nodes recording one message at the same moment produce exactly one receipt.
//! - **`is_sent` is an indexed point read.** It holds no address and no reply code; it answers
//!   `true` only for a receipt the key really has, so a message nothing recorded never reads as
//!   sent.
//! - **An outage reads as unavailable, never as "not sent".** Every database or pool failure is
//!   [`Unavailable`](spot_mail_queue::Unavailable), a value with no fields, and no driver message,
//!   SQLSTATE, host or message id is carried or logged. That is what makes the queue fail closed:
//!   [`MailSender`] reads the log before each attempt and turns an unreadable log into a retry
//!   (`mail_queue.sent_log_unavailable`), so a database that is down delays the message instead of
//!   sending it unrecorded. The store keeps the same rule in the other direction: a send whose
//!   receipt cannot be written is **not** retried by the sender (it could carry the message to a
//!   second delivery), and the receipt is what a later `is_sent` will be asked about. The one
//!   window neither side can close is the moment between the server's acceptance and the insert;
//!   [`MailSender`]'s module docs describe it and the queue's contract is at-least-once because
//!   of it.
//! - **Time is an argument where the caller decides it.** The port's `record_sent` takes no
//!   instant, so a row is stamped with the **database clock** (`now()` of the server whose
//!   transaction wrote it) — one clock for every node, which is what a shared log needs. The
//!   retention cutoff is the caller's argument, never a clock this store reads.
//! - **[`PgSentLog::purge_older_than`] keeps the log small without opening that window.** It
//!   removes receipts recorded before an instant the caller passes, in bounded batches served by
//!   an index. The instant must leave a margin larger than the longest path a retry can take —
//!   read the method's docs before choosing it, because a receipt removed too early is a
//!   message that can be sent again.
//! - **Every value is a bound parameter.** The site is always one, no SQL is built from data, and
//!   every statement is a constant in the crate's `sql` module.
//! - **The receipt keeps what the port says it keeps.** Addresses and SMTP reply codes are
//!   stored; the server's own reply text is not, because the port documents the receipt as "the
//!   addresses and reply codes the transport already reported". `recorded` therefore rebuilds a
//!   rejected entry with an empty description (`SmtpReply::new(code, "")`). The queue never reads
//!   `recorded`; the contract suite does, and compares the accepted and rejected recipients it
//!   wrote.
//!
//! Tenancy: like the other `*-pg` stores, the table lives in the tenant's own database and the
//! pool the caller passes is that tenant's, and every row also carries its [`SiteId`], so
//! `is_sent`, `recorded` and `purge_older_than` are scoped by site inside a shared database too.
//! ADR-61 is kept by the row, not by the deployment.
//!
//! # Tests and the database
//!
//! The integration tests need a PostgreSQL server and skip cleanly (printing `SKIPPED:`) when
//! `SPOT_TEST_PG_URL` is unset, as the other `*-pg` crates do; CI's Postgres service sets it, and
//! with `SPOT_REQUIRE_PG=1` a missing server fails instead of skipping. They run the
//! `spot-mail-queue` `SentLog` contract suite against this store, plus restart persistence across
//! a new pool, two concurrent writers of one message, site isolation, the receipt round trip,
//! retention, and the failure mapping on a pool that cannot connect.
//!
//! [`MailSender`]: spot_mail_queue::MailSender
//! [`SiteId`]: spot_core::SiteId

#![warn(missing_docs)]

mod convert;
pub mod error;
pub mod schema;
mod sql;
mod store;

pub use error::MIGRATION_FAILED;
pub use store::PgSentLog;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-mail-queue-pg";
