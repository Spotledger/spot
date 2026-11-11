#![forbid(unsafe_code)]
//! Sandboxes cloned from production with deterministic data masking (WP-ALM-05.T1, WS19,
//! ADR-22, ADR-070 section 12).
//!
//! A sandbox is a real tenant restored from a production snapshot, masked **before** it is
//! reachable, with secrets and integrations stripped and outbound effects locked off. The
//! acceptance criterion is one sentence: **no unmasked personal or financial data in a
//! sandbox.** This crate proves it before the gate opens.
//!
//! # The workflow
//!
//! [`workflow::Sandbox`] is a checked state machine; every step takes time as an argument:
//!
//! | State | What happened | Reachable |
//! | --- | --- | --- |
//! | [`SandboxState::Planned`] | the plan is validated | no |
//! | [`SandboxState::SnapshotTaken`] | production was snapshotted ([`SnapshotPort`]) | no |
//! | [`SandboxState::Restored`] | restored into a **closed** new tenant ([`TenantPort`]) | no |
//! | [`SandboxState::Masking`] | masking is running | no |
//! | [`SandboxState::Masked`] | masked; a leak scan has not run | no |
//! | [`SandboxState::Scanning`] | the leak scan is running | no |
//! | [`SandboxState::Verified`] | the scan was clean | no |
//! | [`SandboxState::Quarantined`] | the scan found a leak | no |
//! | [`SandboxState::Active`] | activated: the tenant opens | **yes** |
//!
//! The tenant is opened in exactly one place, [`workflow::Sandbox::activate`], and only when a
//! clean [`scan::ScanReport`] is recorded and the outbound lock is installed. A failed or
//! missing scan cannot activate, so no sandbox is exposed before its scan passes.
//!
//! # Masking is driven by metadata, never by a customer list
//!
//! [`classify::MetadataClasses`] reads the effective metadata: every field tagged `pii`,
//! `financial`, `secret` or `health`, every field in a protection group and every field with a
//! `masking` spec is closed. When an [`spot_field_guard::ProtectionSource`] is supplied the
//! guard's facts (own group plus inherited groups, classification, mask format) are the
//! authority ([`classify::MetadataClasses::with_protection`]).
//!
//! Denormalised copies are protected too: a field with `fetch_from = "customer.name"` inherits
//! the strictest tags and groups of its source, so the copy is masked with the same
//! pseudonym. A derived copy is therefore equal to the masked source, which is what
//! referential integrity means for a value copied across documents.
//!
//! **Fail closed.** A field with no classification at all (no tag, no group, no
//! mask) is masked too. Only an explicit Keep decision of the administrator,
//! [`plan::KeepField`] in the [`plan::MaskingPolicy`] (so it is part of the plan), leaves a
//! field in clear. [`plan::FieldPlan`] lists every field and its class before a run and refuses
//! to start when a field has a classification the policy neither closes nor keeps.
//!
//! # A failed run is never reachable
//!
//! Any failure after the copy started marks the tenant `Unusable` in the registry first, then
//! destroys it (best effort); a destroy that fails leaves it `Unusable` and visible
//! ([`workflow::Cleanup`]). The scan opens the sandbox tenant's own store through
//! [`ports::StorePort`] and refuses an unbound or foreign store and a report naming another
//! tenant.
//!
//! # What the maskers do
//!
//! - **Identifiers** (names, e-mail, phone, IBAN, card numbers, national ids) become
//!   deterministic HMAC-SHA256 pseudonyms keyed per sandbox. The same source value maps to the
//!   same fake in every document, journal row, audit diff and denormalised copy.
//! - **Shapes** are preserved for the formats that need them: an e-mail keeps its shape and
//!   moves to a reserved domain, a phone keeps its leading `+` and digit count, and an IBAN
//!   keeps its country, length and a valid mod-97 checksum. Only the payload changes.
//! - **Free text** is scrubbed word by word to hash tokens (`h:<12 hex>`); no original word
//!   survives.
//! - **Attachments** are replaced with placeholders: a hashed name, no content, size zero.
//! - **Secrets and integrations** are stripped; **caches** and the **search index and its
//!   embeddings** are purged (they hold copies of values and cannot be shown to be clean).
//! - **Amounts stay.** Numeric fields *classified* `financial` are ledger amounts and are never
//!   touched: masking adds no noise, so trial balances and journal totals stay balanced. Only
//!   identifiers and text are masked. This is a deliberate rule, not an omission.
//! - **Link fields stay.** A link holds a record id, not personal data; the identity fields of
//!   the target document are masked instead, so references still resolve.
//!
//! The key is [`mask::SandboxKey`] material: HMAC-based, supplied by the caller (never read
//! from a clock or a random source here), and wiped when the [`mask::Masker`] drops. Nothing
//! keeps it after masking and it never appears in a record or a report.
//!
//! # The leak scanner
//!
//! While masking, every original value that was masked or stripped is reduced to two
//! normalised digests (lower-cased whitespace-collapsed text and its punctuation-free compact
//! form) and inserted into a [`scan::SourceIndex`]: a Bloom filter for a cheap negative answer
//! plus the exact digest set that confirms a hit. A Bloom false positive can therefore never
//! quarantine a clean sandbox.
//!
//! [`scan::scan`] then reads every surface of the sandbox — documents, journal, audit rows,
//! attachment metadata, the search index and caches — and reports a [`scan::Leak`] for each
//! candidate that matches an original value exactly or as a word inside a longer text. A
//! `Leak` carries the surface, the record and a digest prefix, **never the value**, so a
//! report can be logged.
//!
//! Values shorter than the policy's minimum (four characters by default) are not indexed:
//! a three-letter string cannot be told apart from ordinary words, and amounts are kept by
//! design. The trade-off is documented and configurable ([`plan::MaskingPolicy`]).
//!
//! # Ports and tests
//!
//! Everything external is a trait with an in-memory implementation:
//! [`data::SandboxStore`] for the tenant's data, [`SnapshotPort`], [`TenantPort`] and
//! [`EffectPort`]. The tests' shared fixture builds a small tenant with documents, a journal,
//! audit rows, an attachment, a search row, caches, secrets and integrations, so the whole
//! workflow runs with no database and no network. A real PostgreSQL-backed store plugs in
//! behind the same trait; that run is the generic `*-pg` gating of a later task.
//!
//! Time is always an argument (`i64` Unix microseconds, or `u64` milliseconds for the tenant
//! id), nothing here reads a wall clock, and no error carries a value.

pub mod classify;
pub mod data;
pub mod engine;
pub mod error;
pub mod mask;
pub mod plan;
pub mod ports;
pub mod scan;
pub mod workflow;

pub use classify::{ClassificationSource, FieldClass, MetadataClasses};
pub use data::{
    Attachment, AuditRow, CacheEntry, Document, Integration, JournalRow, SandboxData, SandboxStore,
    SearchRow, SecretRow,
};
pub use engine::{MaskOutcome, MaskingEngine};
pub use error::{Error, Result, codes};
pub use mask::{MaskKind, MaskReport, Masker, SandboxKey};
pub use plan::{FieldPlan, KeepField, MaskingPolicy, PlannedField, SandboxPlan, SnapshotRef};
pub use ports::{
    EffectPort, LOCKED_CLASSES, MemoryEffects, MemorySnapshots, MemoryStores, MemoryTenants,
    OutboundLock, SnapshotPort, StorePort, TenantPort, TenantRecord, TenantStatus,
};
pub use scan::{Leak, ScanReport, SourceIndex, Surface, scan, scan_leaks};
pub use workflow::{Cleanup, Sandbox, SandboxState, Step, StepRecord};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-sandbox";

/// The HMAC domain-separation prefix of every pseudonym. Changing it changes every fake, so
/// it is versioned.
pub const MASK_DOMAIN: &[u8] = b"spot-sandbox/mask/v1";
