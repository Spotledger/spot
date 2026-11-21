//! `spot-capture-invariant-pg`: the PostgreSQL readers of the journal capture invariant
//! (WP-JRN-02.T3, ADR-061 s3 and s8).
//!
//! `spot-capture-invariant` compares the change feed with the `doc_state` events in a single pass
//! and leaves the reading to its host. This crate reads them:
//!
//! * [`Snapshot`] is one read-only `REPEATABLE READ` transaction on the site's own pool; both
//!   readers read inside it, so the two streams are compared at one instant. A connection that
//!   is not read-only is refused, and nothing here can write to the feed or the outbox.
//! * [`PgFeedReader`] reads the published change-feed entries (`spot_change` by
//!   `spot_change_pub`); [`PgStateReader`] reads the outbox's `doc_state` events
//!   (`spot_outbox` by `spot_outbox_pub`). Both page by position.
//! * [`scan`] runs one pass from a persisted [`Watermarks`] and says what is settled, what is
//!   deferred (younger than the lag) and where to resume; its module documentation states the
//!   rules. [`CaptureGate::run_once`] is the CI mode.
//! * [`schema::DDL`] is the one table the watermark lives in; `spot migrate` creates it.
//!
//! One database is one site (ADR-34): nothing here names a tenant, crosses sites or returns a
//! record's content, only `entity/id`, version and position. No clock is read in Rust: the lag is
//! judged by the database's own instant inside the snapshot.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod error;
mod scan;
pub mod schema;
mod snapshot;
mod sql;
mod watermark;

pub use error::CapturePgError;
pub use scan::{
    CaptureGate, DEFAULT_LAG_SECS, DEFAULT_PAGE_SIZE, DEFAULT_WINDOW, Scan, ScanConfig, scan,
};
pub use snapshot::{Heads, PgFeedReader, PgStateReader, Snapshot};
pub use watermark::Watermarks;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-capture-invariant-pg";
