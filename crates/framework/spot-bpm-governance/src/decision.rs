//! What the guard decided about one create attempt.

use crate::mode::GovernanceMode;

/// How a create attempt was allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// The record type is not governed (mode `free`): the default.
    Free,
    /// The attempt carried a valid case context whose current node permits the record type.
    InCase,
    /// The principal held the outside-process permission and supplied an acceptable reason; the
    /// exception is audited.
    OutsideProcess,
}

/// Where enforcement was handed over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeferredTo {
    /// The entry crate (WP-BPM-13) enforces `process_only`, with no bypass.
    Entry,
}

/// Why a create attempt was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefusalReason {
    /// The record type is governed and the attempt carried no valid case context.
    NotInCase,
    /// The attempt was outside a case and the principal does not hold the outside-process
    /// permission.
    NoPermission,
    /// The principal holds the permission, but the reason was blank or shorter than the floor.
    ReasonRequired {
        /// The configured floor, in characters.
        min_chars: usize,
    },
}

/// What the guard decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GovernanceDecision {
    /// The create may proceed, by the named route.
    Allow {
        /// The route it was allowed by.
        route: Route,
    },
    /// The mode is `process_only`: this crate offers no route and defers to the entry crate.
    Defer {
        /// Where enforcement was handed over.
        to: DeferredTo,
    },
    /// The create is refused.
    Refuse {
        /// Why.
        reason: RefusalReason,
    },
}

impl GovernanceDecision {
    /// An allow by `route`.
    #[must_use]
    pub fn allow(route: Route) -> Self {
        Self::Allow { route }
    }

    /// Whether the create may proceed.
    #[must_use]
    pub fn is_allowed(self) -> bool {
        matches!(self, Self::Allow { .. })
    }

    /// Whether enforcement was deferred to the entry crate.
    #[must_use]
    pub fn is_deferred(self) -> bool {
        matches!(self, Self::Defer { .. })
    }

    /// The route, when allowed.
    #[must_use]
    pub fn route(self) -> Option<Route> {
        match self {
            Self::Allow { route } => Some(route),
            _ => None,
        }
    }

    /// The mode this decision was reached under, when the decision is a refusal or defer.
    #[must_use]
    pub fn enforced_mode(self) -> Option<GovernanceMode> {
        match self {
            Self::Defer { .. } => Some(GovernanceMode::ProcessOnly),
            Self::Refuse { .. } => Some(GovernanceMode::Governed),
            Self::Allow { .. } => None,
        }
    }
}
