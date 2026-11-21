//! Master-data and price propagation as events (WP-EDG-04.T1, ADR-062, ADR-063, ADR-065).
//!
//! Master data flows **down** from its owner, the hub (ADR-23's table: items, price lists, taxes,
//! promotions, employees are owned by Head Office and sent to branches). This crate is the
//! sender-side and receiver-side model for that direction:
//!
//! * [`Subscription`] and [`Router`] — the hub's registry of what each node holds and receives,
//!   and a **pure** routing function that turns a master-data version into the nodes it reaches
//!   (ADR-063 section 3). Scope keys are opaque `(key type, value)` pairs; nothing here decides
//!   what a key means.
//! * [`MasterChange`] and [`EffectiveChange`] — a version, and the instant it becomes effective.
//! * [`ActiveView`] — what a receiver holds and which version is **in force**. A receiver stores a
//!   version on arrival but activates it by the clock, never because a message arrived
//!   (ADR-065 section 6). A version that arrived after its effective instant is
//!   [`late`](ArrivedChange::is_late) with its delay exposed for ADR-065 freshness.
//! * [`FanoutPlanner`] — batches per destination, deterministic order, idempotent re-emit.
//! * [`harness`] — the 100-branch convergence harness over [`spot_sync_protocol::sim`].
//!
//! # What this crate deliberately does not build
//!
//! ADR-062 leaves **cross-node stock visibility at the till** open (live lookup or an "as of"
//! view; see the platform blueprint, WS10). It is a read-model decision, not a sync one, so this
//! crate leaves no `StockViewPolicy` trait or placeholder: when that decision is made it gets its
//! own shape.
//!
//! # Time and policy
//!
//! No function reads the wall clock: every time is an [`Instant`] argument (see [`time`]). The
//! policy ADRs (063 to 067) are still *Proposed*, so this crate fixes only what ADR-062 and the
//! blueprint rows fix and keeps the rest behind data, not behaviour.

#![warn(missing_docs)]

pub mod change;
pub mod error;
pub mod fanout;
pub mod harness;
pub mod scope;
pub mod subscription;
pub mod time;

pub use change::{ActiveView, ArrivedChange, EffectiveChange, MasterChange, StoreOutcome};
pub use error::{Error, Result};
pub use fanout::{DestinationBatch, FanoutPlan, FanoutPlanner};
pub use harness::{
    BranchOutcome, ConvergenceReport, DEFAULT_BRANCHES, HarnessConfig, NEW_PRICE, PRICE_ENTITY,
    PRICE_NAME, PRIOR_PRICE, UPDATE_VERSION,
};
pub use scope::{Doctype, MAX_OPAQUE_BYTES, ScopeKey};
pub use subscription::{Router, Subscription};
pub use time::Instant;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-master";
