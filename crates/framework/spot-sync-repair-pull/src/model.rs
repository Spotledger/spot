//! Plain data: the loop's inputs, outputs and seams.

use serde::{Deserialize, Serialize};
use spot_sync_closure::{Position, RecordKey};
use spot_sync_node::RepairItem;

/// The condition code raised while a key stays missing.
pub const CLOSURE_UNRESOLVED: &str = "CLOSURE_UNRESOLVED";

/// What the loop is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A bundle at `bundle_position` still lacks `keys`.
    Missed {
        /// The referenced records the node lacks.
        keys: Vec<RecordKey>,
        /// The bundle's first position (where it holds the acknowledged prefix).
        bundle_position: Position,
        /// Monotonic milliseconds.
        now_ms: u64,
    },
    /// These records are now applied.
    Arrived(Vec<RecordKey>),
    /// Time passed (monotonic milliseconds): the only input that emits asks.
    Tick(u64),
}

impl Input {
    /// The `Missed` input for one item of the node's repair list.
    #[must_use]
    pub fn missed(item: &RepairItem, now_ms: u64) -> Self {
        Self::Missed {
            keys: item.missing.clone(),
            bundle_position: item.first_position,
            now_ms,
        }
    }
}

/// A request for exactly these records. It names keys only; the hub re-checks scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// At most `max_keys`.
    pub keys: Vec<RecordKey>,
}

/// A request to re-send the destination's whole scope after a position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedResend {
    /// The destination's subscription scope, as the [`ScopeCheck`] names it.
    pub scope: String,
    /// The node's applied prefix: everything after it is sent again (`None`: from the start).
    pub from_position: Option<Position>,
}

/// A condition to raise on the conditions engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionRaise {
    /// Always [`CLOSURE_UNRESOLVED`].
    pub code: String,
    /// `node:<id>`.
    pub subject: String,
    /// Keys still missing when it was raised.
    pub missing: usize,
}

/// What the loop asks its caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Send this pull request to the hub.
    Ask(PullRequest),
    /// Ask the hub to re-send the scope; single asks are suppressed until the keys arrive.
    Escalate(ScopedResend),
    /// Raise this condition.
    Raise(ConditionRaise),
    /// Clear the condition of this subject.
    Clear(String),
    /// Keys outside the node's scope were never asked (the hub would refuse and the loop would
    /// spin): hand them to the lead under [`crate::OUT_OF_SCOPE`].
    OutOfScope(Vec<RecordKey>),
    /// Nothing to do.
    Nothing,
}

/// The node's subscription scope, supplied by the caller.
pub trait ScopeCheck {
    /// Whether the hub would serve `key` to this node.
    fn in_scope(&self, key: &RecordKey) -> bool;
    /// A stable name of the whole scope, for the scoped re-send.
    fn scope(&self) -> String;
    /// The node's acknowledged prefix (`NodeIntake::applied_prefix`).
    fn applied_prefix(&self) -> Option<Position>;
}

/// Where conditions are raised (the conditions engine implements it).
pub trait ConditionPort {
    /// Raise (idempotent per subject).
    fn raise(&mut self, raise: &ConditionRaise);
    /// Clear the subject's condition.
    fn clear(&mut self, subject: &str);
}

/// Sends the `Raise` and `Clear` outputs to the port.
pub fn forward_conditions(outputs: &[Output], port: &mut impl ConditionPort) {
    for out in outputs {
        match out {
            Output::Raise(r) => port.raise(r),
            Output::Clear(s) => port.clear(s),
            _ => {}
        }
    }
}

/// Counters for the health view.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counters {
    /// Keys named in `Ask` outputs (a key asked twice counts twice).
    pub keys_asked: u64,
    /// Scoped re-sends emitted.
    pub escalations: u64,
    /// Tracked keys dropped because the tracked set was full.
    pub dropped_by_bound: u64,
    /// Misses of an already tracked key.
    pub duplicate_misses: u64,
    /// Keys refused for being out of scope.
    pub out_of_scope: u64,
}
