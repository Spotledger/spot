//! The side-effect hold (WP-MIG-08.T1, ADR-22).
//!
//! A migration follows plan, approve, rehearse, apply, verify. While it is unverified the site
//! stays in maintenance, and **no external call may leave**. This crate is the gate every external
//! dispatcher passes through, the durable ledger of what was held, and the release and discard
//! operations:
//!
//! - [`class`]: [`EffectClass`] and the [`PolicyTable`]. Unknown classes are **held**; fiscal
//!   transmissions of issued documents are **never held** (ADR-43), and a configuration cannot
//!   make them holdable.
//! - [`hold`]: the per-site state machine `Open -> Held -> Released | Discarded -> Open`.
//! - [`Gate`]: [`Gate::admit`] is what a dispatcher calls immediately before the irrevocable act.
//!   It returns [`Admission::Proceed`] or [`Admission::Held`]; it errs towards `Held` whenever the
//!   state is unknown, unreadable or possibly stale.
//! - [`store`]: the [`LedgerStore`] trait, the in-memory reference [`InMemoryLedger`] and the
//!   [`contract`] suite every backend must pass. The ledger lives **outside** the database the
//!   migration restores.
//! - [`service::HoldService`]: [`begin`](HoldService::begin), [`release`](HoldService::release)
//!   and [`discard`](HoldService::discard), with release re-dispatching in the original order per
//!   source and every transition recorded in the audit chain.
//!
//! - [`scope`] and [`request`]: the same machinery scoped to a **request** (ADR-080 section 6.7).
//!   A request hold has no unverified state, several requests may hold the same class at once,
//!   and a held effect is released at a commit point or discarded when the request ends first.
//!   While a migration hold is live, a request release dispatches nothing.
//!
//! # Failing closed
//!
//! The crate never lets a held-class effect reach `Proceed` while the state is `Held`, unknown or
//! unreadable, never holds a fiscal transmission, and never replays a discarded effect. It reads
//! no clock and no randomness: time and ids are arguments.
//!
//! # Not here
//!
//! The migration lifecycle (WP-MIG-04, WP-MIG-07) calls this; this crate does not build it, and it
//! does not edit any dispatcher. The hook the job worker and edge sync need is documented in
//! [`gate::QueueGate`] and [`gate::EdgeSyncGate`].

#![warn(missing_docs)]

pub mod audit;
pub mod class;
pub mod codes;
pub mod dispatch;
pub mod effect;
pub mod error;
pub mod gate;
pub mod hold;
pub mod request;
pub mod scope;
pub mod service;
pub mod store;

#[cfg(feature = "testing")]
#[allow(clippy::unwrap_used, clippy::expect_used)]
pub mod contract;

mod serde_site;

pub use audit::{AuditEvent, AuditKind, AuditSink, ChainAudit, RecordingAudit};
pub use class::{ClassPolicy, EffectClass, OnDiscard, OnRelease, PolicyConfig, PolicyTable};
pub use dispatch::{Dispatcher, Notice, Notifier, RecordingDispatcher, RecordingNotifier};
pub use effect::{
    EffectId, EffectRecord, EffectRef, EffectState, NewEffect, PayloadRef, STALE_OUTCOME, Sequence,
    Transition,
};
pub use error::{Error, Result};
pub use gate::{Admission, EdgeSyncGate, Gate, HoldSnapshot, HoldTicket, QueueGate};
pub use hold::{HoldState, MigrationId};
pub use request::{
    MAX_HELD_PER_REQUEST, RequestAdmission, RequestDiscardReport, RequestReleaseReport,
    RequestTicket,
};
pub use scope::{HoldScope, RequestId};
pub use service::{DiscardReport, HoldService, ReleasePlan, ReleaseReport};
pub use store::{InMemoryLedger, LedgerStore, ReleaseCursor, UnavailableLedger};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-effect-hold";
