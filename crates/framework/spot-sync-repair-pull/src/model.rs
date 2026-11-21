//! Plain data: the loop's inputs, outputs and seams. Everything is per tenant (ADR-061
//! section 1): a request, a re-send and a condition each name exactly one tenant.

use serde::{Deserialize, Serialize};
use spot_sync_node::{Position, RecordKey, RepairItem};

/// The condition code raised while a key stays missing.
pub const CLOSURE_UNRESOLVED: &str = "CLOSURE_UNRESOLVED";

/// What the loop is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Input {
    /// A bundle of `tenant` at `bundle_position` still lacks `keys`.
    Missed {
        /// The tenant whose journal the bundle belongs to.
        tenant: String,
        /// The referenced records the node lacks.
        keys: Vec<RecordKey>,
        /// The bundle's first position (where it holds the acknowledged prefix).
        bundle_position: Position,
        /// Monotonic milliseconds.
        now_ms: u64,
    },
    /// These records are now applied for `tenant` (only that tenant's tracking is cleared).
    Arrived {
        /// The tenant that applied the records.
        tenant: String,
        /// The applied records.
        keys: Vec<RecordKey>,
    },
    /// The hub answered that these records of `tenant` exist no more (`Gone`: they were deleted).
    /// They are resolved, not missing: the loop stops asking for them and reports the keys it was
    /// tracking in [`Output::Gone`], so the caller can hand them to `NodeIntake::resolve_gone`.
    Gone {
        /// The tenant the answer is for (only that tenant's tracking is cleared).
        tenant: String,
        /// The records that exist no more.
        keys: Vec<RecordKey>,
    },
    /// The hub finished the scoped re-send of `scope` for `tenant`: keys still missing are
    /// asked for again one by one, and the tenant's escalation state resets. A `scope` that is
    /// not the tenant's current scope is ignored (counted).
    ResendComplete {
        /// The tenant the re-send was for.
        tenant: String,
        /// The scope name the re-send carried ([`ScopeCheck::scope`]).
        scope: String,
    },
    /// Time passed (monotonic milliseconds): the only input that emits asks.
    Tick(u64),
}

impl Input {
    /// The `Missed` input for one item of the node's repair list.
    #[must_use]
    pub fn missed(item: &RepairItem, now_ms: u64) -> Self {
        Self::Missed {
            tenant: item.tenant.clone(),
            keys: item.missing.clone(),
            bundle_position: item.first_position,
            now_ms,
        }
    }
}

/// A request for exactly these records of one tenant. It names the tenant and keys only; the
/// hub re-checks scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    /// The tenant the keys are missing for (never mixed within one request).
    pub tenant: String,
    /// At most `max_keys`.
    pub keys: Vec<RecordKey>,
}

/// A request to re-send a tenant's whole scope after a position.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopedResend {
    /// The tenant to re-send for.
    pub tenant: String,
    /// The destination's subscription scope, as the [`ScopeCheck`] names it.
    pub scope: String,
    /// The tenant's applied prefix: everything after it is sent again (`None`: from the start).
    pub from_position: Option<Position>,
}

/// A condition to raise on the conditions engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionRaise {
    /// Always [`CLOSURE_UNRESOLVED`].
    pub code: String,
    /// `node:<id>`.
    pub subject: String,
    /// The tenant the unresolved keys belong to (one condition per node and tenant).
    pub tenant: String,
    /// Keys of this tenant still missing when it was raised.
    pub missing: usize,
}

/// A condition to clear on the conditions engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionClear {
    /// `node:<id>`.
    pub subject: String,
    /// The tenant whose condition clears; other tenants' conditions are untouched.
    pub tenant: String,
}

/// What the loop asks its caller to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Send this pull request to the hub.
    Ask(PullRequest),
    /// Ask the hub to re-send the scope; the tenant's single asks are suppressed until the keys
    /// arrive or [`Input::ResendComplete`] is fed.
    Escalate(ScopedResend),
    /// Raise this condition.
    Raise(ConditionRaise),
    /// Clear this condition.
    Clear(ConditionClear),
    /// Keys of `tenant` outside the node's scope were never asked (the hub would refuse and the
    /// loop would spin): hand them to the lead under [`crate::OUT_OF_SCOPE`]. Each key is
    /// reported once until it is seen applied (or forgotten by the `max_out_of_scope` bound).
    OutOfScope {
        /// The tenant the keys are missing for.
        tenant: String,
        /// The keys, not reported before.
        keys: Vec<RecordKey>,
    },
    /// A miss of `tenant` was not tracked: the loop already keeps `max_tenants` tenants. Hand it
    /// to the lead under [`crate::TOO_MANY_TENANTS`].
    TenantRefused(String),
    /// Keys of `tenant` the hub answered `Gone` for, that the loop was tracking as missing: they
    /// are resolved. Pass them to `NodeIntake::resolve_gone` so the bundles parked for them go
    /// on. The tenant's condition clears when no other key is missing.
    Gone {
        /// The tenant the keys belong to.
        tenant: String,
        /// The tracked keys that are resolved by the answer.
        keys: Vec<RecordKey>,
    },
    /// Nothing to do.
    Nothing,
}

/// The node's subscription scope, supplied by the caller. Scope and prefix are per tenant.
pub trait ScopeCheck {
    /// Whether the hub would serve `key` of `tenant` to this node.
    fn in_scope(&self, tenant: &str, key: &RecordKey) -> bool;
    /// A stable name of the tenant's whole scope, for the scoped re-send.
    fn scope(&self, tenant: &str) -> String;
    /// The tenant's acknowledged prefix (`NodeIntake::applied_prefix`).
    fn applied_prefix(&self, tenant: &str) -> Option<Position>;
}

/// Where conditions are raised (the conditions engine implements it).
pub trait ConditionPort {
    /// Raise (idempotent per subject and tenant).
    fn raise(&mut self, raise: &ConditionRaise);
    /// Clear the condition of the subject and tenant.
    fn clear(&mut self, clear: &ConditionClear);
}

/// Sends the `Raise` and `Clear` outputs to the port.
pub fn forward_conditions(outputs: &[Output], port: &mut impl ConditionPort) {
    for out in outputs {
        match out {
            Output::Raise(r) => port.raise(r),
            Output::Clear(c) => port.clear(c),
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
    /// Keys reported as out of scope (each report counts once).
    pub out_of_scope: u64,
    /// Misses of an out-of-scope key that was already reported (not reported again).
    pub out_of_scope_repeats: u64,
    /// Remembered out-of-scope keys forgotten because the per-tenant set was full.
    pub out_of_scope_forgotten: u64,
    /// `ResendComplete` inputs that reset a tenant's escalation.
    pub resends_completed: u64,
    /// `ResendComplete` inputs ignored: no re-send outstanding, or another scope.
    pub resends_ignored: u64,
    /// Misses refused because the tenant bound was reached.
    pub tenants_refused: u64,
    /// Tracked keys resolved by a `Gone` answer.
    pub gone: u64,
}
