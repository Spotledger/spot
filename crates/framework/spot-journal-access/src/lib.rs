//! The journal access surface (WP-JRN-09.T4, ADR-061 section 7 surface 2).
//!
//! A warehouse, a lakehouse or an auditor reads the tenant's whole history without loading the
//! platform. The data is **unfiltered by design**, so the controls are the grant and the audit,
//! and both fail closed:
//!
//! * [`Access`]: the `journal_reader` grant model: off by default, named principals, expiring,
//!   every grant action written as `sys_journal_grant` in the same transaction as the grant row
//!   ([`pg::PgGrantStore`]), a grant void when its grantor loses the administrator role, a
//!   revocation effective at the next request.
//! * [`JournalReader`]: the three operations over one tenant's store: `info`, `manifest`,
//!   `segment`. A segment is served only when the signed manifest lists it, verified against its
//!   recorded hash before the first byte, by byte range, in bounded chunks, under a per-principal
//!   concurrency limit and a daily byte budget.
//! * Every call, denied and failed ones included, writes one `sys_journal_access` record
//!   ([`AccessAudit`]); if that write fails the call is refused.
//!
//! No HTTP here (`spot-http` owns the routes over its own port trait), no clock, no randomness:
//! time, ids and the administrator directory are injected.

#![forbid(unsafe_code)]

mod access;
mod error;
mod limits;
mod model;
pub mod pg;
mod range;
mod reader;
mod store;
mod timefmt;

pub use access::{
    Access, AdminDirectory, AuthorizeError, AuthorizedReader, DenyReason, GrantRef, IdSource,
};
pub use error::{AccessError, ReadError};
pub use limits::{Limits, OverLimit, Permit};
pub use model::{
    ACCESS_TOPIC, ADMIN_GRANT, AccessObject, AccessOutcome, AccessRecord, CACHE_MILLIS,
    CHUNK_BYTES, DAY_MILLIS, DEFAULT_MAX_GRANT_DAYS, EXTERNAL_MAX_DAYS, Fut, GRANT_TOPIC, Grant,
    GrantAction, GrantEvent, GrantState, HARD_CAP_DAYS, MAX_SEGMENT_BYTES, PERMISSION, ROLE,
    Settings, valid_id,
};
pub use range::{RangeRequest, canonical_number, canonical_range, parse_range};
pub use reader::{
    AccessAudit, GenerationInfo, InfoOut, JournalMeta, JournalReader, ReaderParts, SegmentRead,
    SegmentStatus, SegmentStream, TimeSource,
};
pub use store::{GrantStore, MemoryGrantStore};
pub use timefmt::rfc3339;
