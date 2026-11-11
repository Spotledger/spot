//! `spot-field-guard`: field-level protection on every channel except the document read
//! path (WP-SEC-04.T1, ADR-070 section 12 rule 6).
//!
//! *The control is applied in one channel and the data leaves through another* is the
//! failure every surveyed product has. After WP-PLT-03.T2 a protected field is safe on the
//! document read, write and list paths. This crate closes the other doors with **one**
//! component, so a new channel cannot be added without choosing how it is protected
//! ([`Channel`] is exhaustive on purpose) and one test can prove that no channel leaks
//! ([`leakcheck`]).
//!
//! It is a library over `spot-permissions` (every decision and the one `mask`) and the
//! protection facts of `spot-meta` ([`ProtectionSource`]). It decides no right itself and
//! caches nothing.
//!
//! - [`FieldGuard`]: `redact` a document for a channel, `redact_diff` a version-history
//!   diff, `columns` for list, export, print and report, `check_query_fields_for` for the
//!   no-inference rule, `system_access` for jobs, sync and migrations.
//! - [`RevealService`]: the audited reveal, one document and one field per call.
//! - [`FieldAccessRecord`] / [`TraceSink`]: the field access trace of `trace` groups.
//! - [`leakcheck`]: the harness later channels use to prove they do not leak.
//! - [`lint`]: a derived field must carry every group of the fields it derives from.
//!
//! No value ever appears in a record, an error, a log line or a metric label.

pub mod channel;
mod diff;
pub mod error;
pub mod guard;
pub mod leakcheck;
pub mod lint;
pub mod records;
pub mod reveal;
pub mod source;

pub use channel::{Channel, Need};
pub use diff::RedactedDiff;
pub use error::GuardError;
pub use guard::{ActorManifest, Columns, FieldGuard, QueryField, QueryUse};
pub use records::{
    AuditRecord, AuditSink, BoundedQueue, FieldAccessRecord, FullSink, Offer, TraceSink, Via,
};
pub use reveal::{Loaded, RevealService, ValueSource};
#[cfg(feature = "meta")]
pub use source::MetaProtection;
pub use source::{DocProtection, ProtectionSource, StaticProtection};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-field-guard";
