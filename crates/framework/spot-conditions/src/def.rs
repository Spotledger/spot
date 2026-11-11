//! The fixed definition of a condition (ADR-067 section 1).

use serde::{Deserialize, Serialize};

use crate::Millis;
use crate::params::ConditionParams;
use crate::severity::Severity;

/// One automatic response step, in the order the definition lists it (ADR-067 section 3).
///
/// The action id is one of the ADR's closed action set (resend a bundle, send a repair bundle,
/// re-bootstrap a record range, release held records, re-request after upgrade, advance a
/// numbering series, renew a certificate, retry apply, notify a contact or role) or an app's own
/// id; this crate only carries it, it never performs it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResponseStep {
    /// The action id.
    pub action: String,
    /// How many times the action may be attempted before the response gives up on this step
    /// (ADR-067 section 3's budget, counted in attempts).
    pub limit: u32,
}

impl ResponseStep {
    /// A response step for `action` that may be attempted `limit` times.
    #[must_use]
    pub fn new(action: &str, limit: u32) -> Self {
        Self {
            action: action.to_owned(),
            limit,
        }
    }
}

/// One step of the escalation schedule: once a condition has been open for `after_millis`, its
/// severity becomes `to`.
///
/// Escalation is one-way (ADR-067 section 1): a step whose `to` is not higher than the
/// condition's current severity is ignored, so a schedule can never lower a condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Escalation {
    /// Elapsed open time at which the step applies, measured from when the condition was raised.
    pub after_millis: Millis,
    /// The severity the condition takes.
    pub to: Severity,
}

impl Escalation {
    /// A step that makes a condition `to` severe once it has been open for `after_millis`.
    #[must_use]
    pub const fn new(after_millis: Millis, to: Severity) -> Self {
        Self { after_millis, to }
    }
}

/// A condition's fixed definition: everything about it that does not depend on a subject.
///
/// Definitions are platform data (ADR-067 section 1): the platform's catalogue and an app's own
/// conditions are both [`ConditionDef`]s registered in one [`Registry`](crate::Registry), and get
/// the same lifecycle, display and notification from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConditionDef {
    /// Stable id, upper snake case (`MISSING_DEP`). Validated by
    /// [`Registry::register`](crate::Registry::register).
    pub id: String,
    /// The severity the condition is raised at.
    pub severity: Severity,
    /// What the subject is: `node`, `stream`, `record`, `tenant`, or an app's own kind.
    pub subject_kind: String,
    /// The localisation key of the one plain-language sentence (ADR-067 section 6), translated
    /// like any user-interface text.
    pub explanation_key: String,
    /// The parameter names that sentence braces.
    pub params: Vec<String>,
    /// The automatic actions, in order.
    pub response: Vec<ResponseStep>,
    /// The escalation schedule; sorted by `after_millis` when it is registered.
    pub escalation: Vec<Escalation>,
    /// The operator command ids that apply (ADR-067 section 6), if any.
    pub actions: Vec<String>,
}

impl ConditionDef {
    /// A definition with no parameters, response, escalation or operator actions.
    #[must_use]
    pub fn new(id: &str, severity: Severity, subject_kind: &str, explanation_key: &str) -> Self {
        Self {
            id: id.to_owned(),
            severity,
            subject_kind: subject_kind.to_owned(),
            explanation_key: explanation_key.to_owned(),
            params: Vec::new(),
            response: Vec::new(),
            escalation: Vec::new(),
            actions: Vec::new(),
        }
    }

    /// Declare a parameter name the explanation braces.
    #[must_use]
    pub fn with_param(mut self, name: &str) -> Self {
        self.params.push(name.to_owned());
        self
    }

    /// Append an automatic response step.
    #[must_use]
    pub fn with_response(mut self, step: ResponseStep) -> Self {
        self.response.push(step);
        self
    }

    /// Append an escalation step (the registry sorts the schedule by `after_millis`).
    #[must_use]
    pub fn with_escalation(mut self, step: Escalation) -> Self {
        self.escalation.push(step);
        self
    }

    /// Append an operator command id that applies to a condition of this kind.
    #[must_use]
    pub fn with_action(mut self, command: &str) -> Self {
        self.actions.push(command.to_owned());
        self
    }

    /// The declared parameter names that `params` does not carry, in declaration order.
    ///
    /// The engine records what it is given; this is for a producer that wants to check itself
    /// before raising, so an explanation can never render with a hole in it.
    #[must_use]
    pub fn missing_params<'a>(&'a self, params: &ConditionParams) -> Vec<&'a str> {
        self.params
            .iter()
            .filter(|name| params.get(name.as_str()).is_none())
            .map(String::as_str)
            .collect()
    }
}
