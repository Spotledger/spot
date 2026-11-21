//! `spot-sync-reconcile` — drift reconciliation and the sync lag measure (WS10, WP-EDG-07.T1;
//! ADR-062, ADR-053, ADR-23).
//!
//! Sync is a consumer of the stream (ADR-062): a hub owns a record and an edge holds a mirror
//! of it. At-least-once delivery with idempotent apply (ADR-053) keeps the two equal in the
//! normal case, but a lost, delayed or corrupted batch leaves **drift**, and nobody can say
//! whether a branch is current without a measure. This crate is that measure.
//!
//! # What a reconciliation cycle does
//!
//! 1. Read the **owner** and the **mirror** each as one consistent snapshot at a safe bookmark
//!    ([`DigestSource`], ADR-053): in-flight writes after the bookmark never cause false drift.
//! 2. Summarise each snapshot as a [`Digest`] of its scope: per-DocType counts, sums of the
//!    declared integer total fields, GL balance by account, stock quantity by item, and a
//!    **Merkle-style range digest** over the `(name, version)` pairs.
//! 3. Compare the digests ([`reconcile`]). When the range digest differs, the difference is
//!    narrowed by **bisection** over the two range trees, so the cost grows with the number of
//!    differences, not with the number of documents.
//! 4. Turn the [`Finding`]s into **repair suggestions** only ([`plan_repairs`]). Applying them is
//!    the self-healing layer's job (ADR-067); this crate never repairs an owner from a mirror.
//! 5. Measure the **lag** ([`measure_lag`]): `newest produced seq - last applied seq` and the
//!    age of the oldest unapplied record, per node and per scope.
//! 6. Run on a daily [`cycle_schedule`] through `spot-scheduler`, unless settings say otherwise.
//!
//! # Metric names
//!
//! The numbers this crate produces are exported under these names, served by
//! [`spot_telemetry::render_prometheus`]. Labels are bounded: `node` is a source id
//! (`<site>#<partition>`, ADR-056), `scope` is the scope key's value, and `kind` is one of
//! `missing`, `extra`, `version` or `total`.
//!
//! | Constant | Metric | Type |
//! | --- | --- | --- |
//! | [`metrics::FINDINGS_TOTAL`] | `spot_sync_reconcile_findings_total` | counter |
//! | [`metrics::LAG_SEQ`] | `spot_sync_reconcile_lag_seq` | gauge |
//! | [`metrics::OLDEST_UNAPPLIED_AGE_SECONDS`] | `spot_sync_reconcile_oldest_unapplied_age_seconds` | gauge |
//! | [`metrics::ROUNDS`] | `spot_sync_reconcile_rounds` | gauge |
//! | [`metrics::DIGEST_BYTES_TOTAL`] | `spot_sync_reconcile_digest_bytes_total` | counter |
//! | [`metrics::CYCLES_TOTAL`] | `spot_sync_reconcile_cycles_total` | counter |
//!
//! # Example
//!
//! ```
//! use spot_storage_contract::{FeedPosition, Name, Version};
//! use spot_sync_reconcile::{
//!     DigestSource, MemorySource, RecordFacts, RecordKey, Scope, ScopeSnapshot, reconcile,
//! };
//!
//! # fn key(entity: &str, name: &str, version: u64) -> RecordKey {
//! #     RecordKey { entity: Name::new(entity).unwrap(), name: name.into(), version: Version(version) }
//! # }
//! # fn facts(k: RecordKey) -> RecordFacts { RecordFacts { key: k, totals: vec![], gl: vec![], stock: vec![] } }
//! let scope = Scope::new("company", "C1");
//! let owner = MemorySource::new(
//!     scope.clone(),
//!     FeedPosition::new(1, 7),
//!     vec![facts(key("item", "A", 1)), facts(key("item", "B", 2))],
//! );
//! // The mirror is missing B.
//! let mirror = MemorySource::new(
//!     scope.clone(),
//!     FeedPosition::new(1, 7),
//!     vec![facts(key("item", "A", 1))],
//! );
//! let owner_snapshot: ScopeSnapshot = owner.snapshot_now(&scope);
//! let mirror_snapshot = mirror.snapshot_now(&scope);
//! let report = reconcile(&owner_snapshot, &mirror_snapshot, &scope);
//! // B is missing at the mirror (and the count digest differs with it).
//! assert_eq!(report.of_kind("missing").len(), 1);
//! ```

#![forbid(unsafe_code)]

pub mod digest;
pub mod error;
pub mod lag;
pub mod metrics;
pub mod reconcile;
pub mod repair;
pub mod schedule;
pub mod scope;
pub mod source;

pub use digest::{Digest, RecordFacts, RecordKey, ScopeSnapshot};
pub use error::{ReconcileError, ReconcileResult};
pub use lag::{Lag, LagSample, ProducedRecord, measure_lag};
pub use reconcile::{Finding, Measure, ReconcileReport, reconcile, reconcile_sources};
pub use repair::{NodeRole, Repair, plan_repairs};
pub use schedule::{CycleSettings, cycle_schedule, due_cycles};
pub use scope::{DigestPlan, GroupSpec, Scope};
pub use source::{DigestSource, MemorySource};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-reconcile";
