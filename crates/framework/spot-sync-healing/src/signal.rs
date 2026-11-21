//! The condition the driver raises, as data the caller applies to its conditions engine.

use spot_conditions::{
    AuditSink, ConditionDef, ConditionEngine, ConditionParams, ConditionsError, Millis,
    RaiseOutcome, Registry, Severity,
};

use crate::JournalEntry;

/// The condition raised when a repair gives up or its direction is not certain (ADR-067
/// sections 2 and 3). The host registers it by calling [`register_conditions`] on the registry
/// it builds its conditions engine from (kind [`SUBJECT_KIND`]: a node and scope,
/// `node/scope`). The definition lives in this crate ([`condition_def`]) until the ADR-067 row
/// exists (WP-SYN-09.T5), when it moves to the `spot-sync-conditions` catalogue.
pub const CONDITION_ID: &str = "RECONCILE_DIVERGENCE";

/// The subject kind of [`CONDITION_ID`]: the subject is `node/scope`, a node and one of its
/// scopes.
pub const SUBJECT_KIND: &str = "node_scope";

/// The definition of [`CONDITION_ID`]: amber, subject `node/scope`, parameters `node`, `scope`,
/// `cause` and `attempts`, no automatic response step (the repair loop has stopped), and the
/// operator command `re_verify`.
#[must_use]
pub fn condition_def() -> ConditionDef {
    ConditionDef::new(
        CONDITION_ID,
        Severity::Amber,
        SUBJECT_KIND,
        concat!(
            "sync",
            ".condition",
            ".reconcile_divergence",
            ".explanation"
        ),
    )
    .with_param("node")
    .with_param("scope")
    .with_param("cause")
    .with_param("attempts")
    .with_action("re_verify")
}

/// Registers [`condition_def`]. The host calls this once on the registry it builds the
/// [`ConditionEngine`] from; without it [`apply_signals`] fails with `UnknownId`.
///
/// # Errors
///
/// [`ConditionsError::DuplicateId`] when the id is already registered.
pub fn register_conditions(registry: &mut Registry) -> Result<(), ConditionsError> {
    registry.register(condition_def())
}

/// Why the condition was raised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// A repair was tried `max_attempts` times without the finding disappearing.
    AttemptsExhausted,
    /// No repair was made; the code is the refusal's `sync_healing.*` code.
    Refused(&'static str),
}

impl Cause {
    /// A stable text for the condition's parameters.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::AttemptsExhausted => "attempts_exhausted",
            Self::Refused(code) => code,
        }
    }
}

/// A change the driver asks the conditions engine to make.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConditionSignal {
    /// Raise [`CONDITION_ID`] on `subject`, with the journal as evidence.
    Raise {
        /// The subject, `node/scope`.
        subject: String,
        /// The values the explanation fills in: `node`, `scope`, `cause`, `attempts`.
        params: ConditionParams,
        /// Why.
        cause: Cause,
        /// The journal entries of that node and scope, oldest first.
        evidence: Vec<JournalEntry>,
    },
    /// A comparison since showed the scope clean (or repairable again): clear the condition.
    Clear {
        /// The subject, `node/scope`.
        subject: String,
    },
}

/// Applies signals to an engine, in order.
///
/// # Errors
///
/// [`ConditionsError::UnknownId`] when [`CONDITION_ID`] is not registered in the engine's
/// registry.
pub fn apply_signals<S: AuditSink>(
    engine: &mut ConditionEngine<S>,
    signals: &[ConditionSignal],
    now: Millis,
) -> Result<(), ConditionsError> {
    for signal in signals {
        match signal {
            ConditionSignal::Raise {
                subject, params, ..
            } => {
                let _: RaiseOutcome = engine.raise(CONDITION_ID, subject, params.clone(), now)?;
            }
            ConditionSignal::Clear { subject } => {
                engine.observe_cleared(CONDITION_ID, subject, now)?;
            }
        }
    }
    Ok(())
}
