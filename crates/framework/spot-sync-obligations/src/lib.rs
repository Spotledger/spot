//! The sync obligation ledger (ADR-065 sections 1 to 3 and 6) as pure functions.
//!
//! Obligations are **derived** by a query over the stream, never stored per record:
//! [`outstanding`] lists the mandatory events a destination has not applied, [`due_by`] gives
//! each its deadline, [`ladder`] turns the clock into an escalation level and the notifications
//! to send on a transition, [`lane_order`] sorts mandatory work before bulk, [`evidence`] builds
//! the one range event written when the applied prefix crosses mandatory events, and [`report`]
//! answers "show me that this change reached every branch, and when".
//!
//! Nothing here reads a clock, a random source, the filesystem or the network: every instant is
//! an argument in milliseconds since the Unix epoch, and every threshold is a field of
//! [`ObligationSettings`].

mod deadline;
mod error;
mod evidence;
mod lane;
mod model;
mod outstanding;
mod report;

pub use deadline::{due_by, ladder, level_of};
pub use error::{HOLD_UNKNOWN, ObligationError, PREFIX_REGRESSED, SETTINGS_INVALID};
pub use evidence::evidence;
pub use lane::{Lane, Pending, lane_order};
pub use model::{
    LadderOutcome, Level, MandatoryEvent, Notify, Obligation, ObligationSatisfied,
    ObligationSettings, Recipient,
};
pub use outstanding::outstanding;
pub use report::{Crossing, NodeState, ReportRow, ReportStatus, report};
