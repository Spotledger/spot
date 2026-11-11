//! Hub-side lease grants for leased naming sequences (WP-EDG-03.T4/T6, R-065; ADR-062
//! section 7).
//!
//! A branch that is offline names its documents from a leased range of numbers
//! (`spot-partition`'s `RangeLease`, used through `spot-leased-sequences`). The bare
//! `RangeAllocator` grants a range to whatever `NodeId` it is given. [`GrantService`] is the
//! service in front of it: it knows **who** asks (the node the transport authenticated), whether
//! that node is still **entitled** (standing, and the series the hub allows it), that numbers are
//! **never granted twice** (a log written before the range is returned, and a cursor rebuilt from
//! it after a restart), that a lost answer costs nothing (a re-request under the same envelope id
//! gets the same range), and that each outcome is **audited**.
//!
//! - [`TransportNode`] / [`LeaseRequest`] / [`RequestId`]: the authenticated node, what it asks
//!   for, and the envelope id that makes a retry idempotent.
//! - [`StandingPort`], [`LiveRegistryStanding`], [`RegistryStanding`]: whether a node is
//!   `Active`. Anything else, and an unreadable answer, is refused. The live view is the one the
//!   hub wires; the snapshot is for tests.
//! - [`GrantPolicy`]: the series a node may lease, the cap on distinct series, the series
//!   maximum, the minimum interval between grants, the per-node memory of request ids and the
//!   window in which refusals are counted.
//! - [`GrantLog`], [`MemoryGrantLog`], [`contract`]: the durable record of grants, and the suite
//!   any store must pass. The traits are synchronous like `RangeAllocator`; whether a durable
//!   store is sync or async is WP-CON-01.T4's decision and is not made here.
//! - [`RebuildableAllocator`]: a `RangeAllocator` that resumes above the log.
//! - [`GrantService`]: the rules, in order.
//! - [`ReplayGuard`], [`LedgerStore`], [`ReenrolmentStanding`]: the hub half of offline-identity
//!   replay detection (see the `replay` module's notes on the exported items): a strictly
//!   increasing nonce per node, the node's report compared with what the hub issued, a snapshot
//!   paired with signed hub time, and re-enrolment of a node that reported less.
//!
//! The crate reads no clock (the instant is an argument), keeps no key material and writes only
//! ids and numbers to the audit sink.

#![warn(missing_docs)]

mod allocator;
mod dedupe;
mod error;
mod identity;
mod ledger;
mod log;
mod policy;
mod refusals;
mod replay;
mod service;
mod standing;

pub use allocator::RebuildableAllocator;
pub use error::GrantError;
pub use identity::{LeaseRequest, MAX_REQUEST_ID_LEN, RequestId, TransportNode};
pub use ledger::{
    LedgerError, LedgerStore, MemoryLedgerStore, NodeLedger, contract as ledger_contract,
};
pub use log::{GrantLog, GrantLogError, GrantRecord, MemoryGrantLog, contract};
pub use policy::{
    DEFAULT_MAX_REMEMBERED_REQUESTS, DEFAULT_MAX_SERIES, DEFAULT_MAX_SIZE, DEFAULT_MIN_INTERVAL,
    DEFAULT_REFUSAL_WINDOW, GrantPolicy,
};
pub use replay::{
    Admitted, KIND_NODE_REENROLLED, KIND_NODE_REGRESSED, KIND_NODE_REPORT_AHEAD,
    ReenrolmentStanding, ReplayGuard, ReplayRefusal, SnapshotReply,
};
pub use service::{GrantService, KIND_GRANTED, KIND_REFUSED, KIND_REPLAYED};
pub use standing::{
    LiveRegistryStanding, RegistryStanding, Standing, StandingPort, StandingUnavailable,
};
