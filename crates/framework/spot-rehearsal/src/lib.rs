//! `spot-rehearsal` — rehearse a release on a clone before it touches the live site
//! (blueprint WS12, WP-MIG-06.T1, ADR-22).
//!
//! Migration runs **plan → approve → rehearse → apply → verify**. This crate is the *rehearse*
//! step: it takes a snapshot of a site, restores it into a scratch database, measures the site
//! before and after the plan is applied, runs the invariant sets, and reports every difference
//! the plan does not explain.
//!
//! # What a rehearsal does
//!
//! 1. [`Rehearsal::run`] checks the approved [`Ticket`] against the plan's `plan_digest`.
//! 2. It obtains a clone: a [`spot_snapshot::SnapshotProvider`] takes a snapshot of the site and
//!    restores it into the scratch database (see [`CloneRequest`]). The clone is owned by an RAII
//!    guard ([`RehearsalClone`]) that destroys it on **every** exit path, including a panic.
//! 3. It computes the **pre fingerprint** over the clone.
//! 4. It applies the plan to the clone through an [`Applier`] (the real one is
//!    [`PgMigrateApplier`], which calls `spot-pg-migrate::apply`).
//! 5. It computes the **post fingerprint** and runs every [`InvariantSet`].
//! 6. It diffs the two fingerprints and classifies each difference as **expected** (a section a
//!    plan change declares it moves, through a [`ReleaseDeclaration`]) or **unexplained**.
//!
//! The result is a [`RehearsalReport`], structured (JSON) and rendered ([`RehearsalReport::render_text`]).
//!
//! # The fingerprint
//!
//! The ERP does not exist yet, so this crate owns the *engine*, not the *contents*. A
//! [`FingerprintSpec`] declares one [`FingerprintQuery`] per section (trial balance by account and
//! company, stock balance by item/warehouse/valuation, AR and AP ageing buckets). The engine runs
//! each declared query through a [`FingerprintSource`], reduces the rows to a deterministic
//! [`Fingerprint`] (integers only, ADR-12), and digests its canonical form. Foundation and ERP
//! register the actual queries later; the engine carries no ERP DocType or account names.
//!
//! # Wire form of a measure
//!
//! Every `i128` measure of a [`Fingerprint`] and of its diffs is written as a JSON string of
//! decimal digits (`"-12345678901234567890123"`), always. serde cannot buffer an `i128`, so a JSON
//! integer would serialise but could not be read back from inside an internally tagged enum such
//! as a journal record. A JSON integer that fits `i64` or `u64` is still read, so older data and
//! hand-written specs keep working; anything else is refused. The fingerprint's digest is computed
//! from the numbers, not from this form, and has not changed.
//!
//! # Replay policy
//!
//! [`ReplayPolicy`] (`off`, `auto`, `always`) is a per-site setting. [`decide_replay`] resolves
//! `auto` against the plan and the release declaration: it replays when the plan contains a
//! behaviour-changing or high-sensitivity entry. Behavioural replay itself is WP-MIG-06.T2.
//!
//! # Honest limits
//!
//! - The engine never reads a clock or the environment: time and identity are arguments.
//! - No floating-point arithmetic for money or quantities; a measure that is not an integer is
//!   refused ([`Error::FingerprintFailed`]).
//! - Nothing here touches the live site database: only the clone is read and written.
//! - The engine contains no ERP DocType, account or party names.

pub mod applier;
pub mod clone;
pub mod codes;
pub mod declaration;
pub mod engine;
pub mod error;
pub mod fingerprint;
pub mod invariants;
pub mod replay;
pub mod report;
pub mod spec;
mod wire;

pub use applier::{Applier, ApplyOutcome, PgMigrateApplier};
pub use clone::{
    CleanupOutcome, CloneRequest, CloneTarget, RehearsalClone, Scratch, SnapshotScratch,
    obtain_clone,
};
pub use declaration::{NoDeclaration, PlanDeclaration, ReleaseDeclaration};
pub use engine::Rehearsal;
pub use error::{Error, Result};
pub use fingerprint::{
    Dimension, Fingerprint, FingerprintDiff, FingerprintRow, FingerprintSection,
};
pub use invariants::{
    InvariantContext, InvariantReport, InvariantSet, InvariantSetResult, InvariantViolation,
    StaticInvariantSet,
};
pub use replay::{ReplayDecision, ReplayPolicy, decide_replay};
pub use report::{ClassifiedDiff, CloneRecord, DiffClass, RehearsalOutcome, RehearsalReport};
pub use spec::{
    BucketSpec, FingerprintQuery, FingerprintSource, FingerprintSpec, StorageFingerprintSource,
};

/// A boxed future: what an object-safe async method returns.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
