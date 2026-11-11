//! The stable `quarantine.*` codes of the quarantine queue.
//!
//! Every code here has one entry in `spot-errors`' `catalogue/quarantine.toml` with its class, a
//! one-sentence summary and a remedy. The codes are a **library-internal vocabulary**: the queue
//! is a platform facility, so nothing here rides an `AppError` and every class is `none` (see
//! `crates/framework/spot-errors/README.md`).
//!
//! The **rejection** reasons a quarantined item carries are `replica_apply.*` codes, not these:
//! they come from the apply path (WP-EDG-06.T1) and are catalogued in `replica_apply.toml`. This
//! module only names what the queue itself can fail with, plus the one reason a record is parked
//! behind another without having been applied at all.

/// The quarantine item a caller named does not exist.
pub const NOT_FOUND: &str = "quarantine.not_found";
/// The acting principal may not perform the requested operation on the item.
pub const DENIED: &str = "quarantine.denied";
/// A discard was requested without the reason it requires.
pub const REASON_REQUIRED: &str = "quarantine.reason_required";
/// The item is released or discarded, so the requested operation does not apply.
pub const NOT_OPEN: &str = "quarantine.not_open";
/// The quarantine store itself failed.
pub const STORE_FAILED: &str = "quarantine.store_failed";
/// Recording a quarantine audit entry failed.
pub const AUDIT_FAILED: &str = "quarantine.audit_failed";
/// A later version of a document waits behind a quarantined earlier version (parked, not applied).
pub const DEPENDENT_HELD: &str = "quarantine.dependent_held";
/// The item is released or discarded, and a closed item never changes again.
pub const ITEM_CLOSED: &str = "quarantine.item_closed";
/// A payload, detail, reason code or source name is longer than the queue accepts.
pub const TOO_LARGE: &str = "quarantine.too_large";
/// A received record's image is larger than the queue stores; a short marker item records the
/// refusal instead (the item's reason code, not an error).
pub const PAYLOAD_TOO_LARGE: &str = "quarantine.payload_too_large";
/// The item marks a record too large to store, so it cannot be retried or released.
pub const MARKER_ONLY: &str = "quarantine.marker_only";
