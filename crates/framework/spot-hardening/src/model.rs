//! The hardening data model: the OS families and severities, the validated control and fact
//! names, the check vocabulary, the remedy reference into the operation catalogue, and the
//! remediation policy.
//!
//! Everything here is data: a baseline is a list of [`Control`]s, a check is a [`CheckSpec`]
//! tree over named facts, and a remedy is an [`OpRef`] into the typed operation catalogue
//! (`spot-mgmt-ops`). No behaviour, no I/O and no ambient reads live in this module; the
//! evaluation and planning live in their own modules.
//!
//! # Rules (the behaviour this module is, in our own words)
//!
//! - A [`ControlId`] and a [`FactKey`] are dotted lower-case names of two or more segments,
//!   each matching `[a-z][a-z0-9_]*`, validated on construction — including through serde, so
//!   a malformed name cannot enter a baseline from a payload.
//! - [`Severity`] is ordered `Low < Medium < High < Critical`; the ordering is part of the
//!   contract (the remediation policy acts only at or above a minimum severity).
//! - A [`CheckSpec`] is a closed tree: leaf checks read one fact (or two, for [`CheckSpec::Subset`])
//!   and the combinators [`CheckSpec::All`] and [`CheckSpec::Any`] nest them. There is no
//!   arbitrary expression and no `Any` value, so a check is always explainable.
//! - A [`Control`] is auto-remediable when it carries a [`Control::remedy`] and is
//!   [`Control::reversible`]; a [`Control::lockout_risk`] control is only applied when the
//!   policy allows it and a connectivity guard is present.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use spot_mgmt_ops::{Invocation, OpId, ParamValue};

use crate::error::HardeningError;

/// Whether a text is one dotted lower-case segment: a lower-case letter, then lower-case
/// letters, digits or underscores.
fn is_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Whether a text is a dotted lower-case name of two or more segments.
fn is_dotted_lower(text: &str) -> bool {
    let segments: Vec<&str> = text.split('.').collect();
    segments.len() >= 2 && segments.iter().all(|segment| is_segment(segment))
}

/// Whether a text is a lower-case snake identifier: the shape a parameter name has.
pub(crate) fn is_param_name(name: &str) -> bool {
    is_segment(name)
}

/// The operating-system family a baseline and its controls apply to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsFamily {
    /// A Linux distribution (Fedora, a RHEL-compatible distribution or Ubuntu LTS).
    Linux,
    /// A Windows server or workstation.
    Windows,
}

impl OsFamily {
    /// The family's lower-case name, as the baseline files and reports spell it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }
}

/// How much a control matters, ordered `Low < Medium < High < Critical`.
///
/// The ordering is part of the contract: the remediation policy acts only on controls at or
/// above its minimum severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// Worth fixing but not dangerous.
    Low,
    /// A meaningful weakening of the host.
    Medium,
    /// A serious weakening that an attacker can use.
    High,
    /// A control whose absence is exploitable at once.
    Critical,
}

/// A control's stable identifier: a dotted lower-case name such as `ssh.root_login`.
///
/// Validated on construction, so every method can trust the shape; serde deserializes through
/// [`TryFrom`] for the same guarantee.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct ControlId(String);

impl ControlId {
    /// The id named `text`, or why it was refused.
    ///
    /// # Errors
    ///
    /// [`HardeningError::ControlInvalid`] when the text is not a dotted lower-case name of two
    /// or more segments.
    pub fn try_new(text: &str) -> Result<Self, HardeningError> {
        if is_dotted_lower(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(HardeningError::ControlInvalid {
                id: text.to_owned(),
                detail: "a control id must be two or more dot-separated lower-case segments"
                    .to_owned(),
            })
        }
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ControlId {
    type Error = HardeningError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::try_new(&text)
    }
}

impl TryFrom<&str> for ControlId {
    type Error = HardeningError;

    fn try_from(text: &str) -> Result<Self, Self::Error> {
        Self::try_new(text)
    }
}

impl std::fmt::Display for ControlId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// The name of one fact an agent reports: a dotted lower-case name such as `ssh.enabled`.
///
/// Validated on construction, exactly like [`ControlId`], so a check can never name a fact
/// that is not well formed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String")]
pub struct FactKey(String);

impl FactKey {
    /// The key named `text`, or why it was refused.
    ///
    /// # Errors
    ///
    /// [`HardeningError::FactInvalid`] when the text is not a dotted lower-case name of two or
    /// more segments.
    pub fn try_new(text: &str) -> Result<Self, HardeningError> {
        if is_dotted_lower(text) {
            Ok(Self(text.to_owned()))
        } else {
            Err(HardeningError::FactInvalid {
                fact: text.to_owned(),
                detail: "a fact key must be two or more dot-separated lower-case segments"
                    .to_owned(),
            })
        }
    }

    /// The key as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for FactKey {
    type Error = HardeningError;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::try_new(&text)
    }
}

impl TryFrom<&str> for FactKey {
    type Error = HardeningError;

    fn try_from(text: &str) -> Result<Self, Self::Error> {
        Self::try_new(text)
    }
}

impl std::fmt::Display for FactKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// One fact an agent reported, in the small typed vocabulary a check understands.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum FactValue {
    /// A boolean (a flag such as "the firewall's default policy is deny").
    Bool(bool),
    /// A single text value (an SELinux mode, an SSH directive, a service state).
    Text(String),
    /// A whole number (a port, a count).
    Int(i64),
    /// A list of text values (running services, open ports).
    TextList(Vec<String>),
}

/// How one fact is judged against what the baseline expects.
///
/// The set is deliberately small and closed: every check is explainable as "the fact must
/// be this", "must be one of these", "must be at least this" or "must be a subset of that",
/// or a boolean combination of those. A check that needs an arbitrary expression is not a
/// typed check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CheckSpec {
    /// The fact must be a boolean equal to `expect`.
    Bool {
        /// The fact to read.
        fact: FactKey,
        /// The required value.
        expect: bool,
    },
    /// The fact must be text equal to `expect`.
    Text {
        /// The fact to read.
        fact: FactKey,
        /// The required value.
        expect: String,
    },
    /// The fact must be text equal to one of `allow`.
    TextIn {
        /// The fact to read.
        fact: FactKey,
        /// The accepted values.
        allow: Vec<String>,
    },
    /// The fact must be a whole number within the optional bounds.
    Int {
        /// The fact to read.
        fact: FactKey,
        /// The smallest accepted value, when bounded below.
        min: Option<i64>,
        /// The largest accepted value, when bounded above.
        max: Option<i64>,
    },
    /// The fact must be present, whatever its value.
    Present {
        /// The fact that must be reported.
        fact: FactKey,
    },
    /// The fact must be absent (for example a service that must not exist).
    Absent {
        /// The fact that must not be reported.
        fact: FactKey,
    },
    /// Every element of the fact's text list must be one of the allowed fact's elements.
    Subset {
        /// The fact whose elements must all be allowed.
        fact: FactKey,
        /// The fact whose elements are the allow-list.
        allowed_fact: FactKey,
    },
    /// Every nested check must pass.
    All {
        /// The nested checks; must not be empty.
        checks: Vec<CheckSpec>,
    },
    /// At least one nested check must pass.
    Any {
        /// The nested checks; must not be empty.
        checks: Vec<CheckSpec>,
    },
}

/// A reference to one typed operation in the management catalogue, with its parameters.
///
/// The operation id is a [`spot_mgmt_ops::OpId`] and every parameter is a typed
/// [`spot_mgmt_ops::ParamValue`], so a remedy is exactly what the catalogue validates and the
/// agent can run — never a hand-built command line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpRef {
    /// The operation to run.
    pub op: OpId,
    /// The operation's parameters, by name; empty when the operation takes none.
    #[serde(default)]
    pub params: BTreeMap<String, ParamValue>,
}

impl OpRef {
    /// A reference to `op` with no parameters.
    #[must_use]
    pub fn new(op: OpId) -> Self {
        Self {
            op,
            params: BTreeMap::new(),
        }
    }

    /// The same reference with one parameter set, replacing any value already under `name`.
    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: ParamValue) -> Self {
        self.params.insert(name.into(), value);
        self
    }

    /// This reference as a catalogue invocation, for validation.
    #[must_use]
    pub fn to_invocation(&self) -> Invocation {
        let mut invocation = Invocation::new(self.op.clone());
        for (name, value) in &self.params {
            invocation = invocation.with(name.clone(), value.clone());
        }
        invocation
    }
}

/// One hardening control: what it checks, how serious it is and how it can be remediated.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Control {
    /// The control's stable id.
    pub id: ControlId,
    /// One-line human title, shown in the report.
    pub title: String,
    /// The OS family the control applies to; must match its baseline's.
    pub os_family: OsFamily,
    /// The check to run against the agent's facts.
    pub check: CheckSpec,
    /// The operation that remediates a failing control, when one exists.
    #[serde(default)]
    pub remedy: Option<OpRef>,
    /// How serious a failure is.
    pub severity: Severity,
    /// Whether the remedy can be undone (a control without a reversible remedy is never
    /// auto-applied).
    #[serde(default)]
    pub reversible: bool,
    /// Whether applying the remedy can cut off access to the host (SSH, firewall changes).
    #[serde(default)]
    pub lockout_risk: bool,
}

impl Control {
    /// Whether the control can be remediated automatically: it has a remedy and the remedy is
    /// reversible.
    #[must_use]
    pub fn auto_remediable(&self) -> bool {
        self.remedy.is_some() && self.reversible
    }
}

/// How the remediation planner is allowed to act.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    /// Never apply; every remediable control is only suggested.
    Suggest,
    /// Apply the remedies the policy permits.
    AutoRemediate,
}

/// The rules the remediation planner applies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Whether the planner may apply anything at all.
    pub mode: PolicyMode,
    /// Act only on controls at or above this severity.
    pub minimum_severity: Severity,
    /// Whether lock-out-risk remedies may be applied automatically.
    pub allow_lockout_risk: bool,
    /// Whether a connectivity-safety guard (verified out-of-band access) is present.
    pub connectivity_guard: bool,
}

impl Policy {
    /// The safe default: suggest only, act on nothing.
    #[must_use]
    pub fn suggest() -> Self {
        Self {
            mode: PolicyMode::Suggest,
            minimum_severity: Severity::Low,
            allow_lockout_risk: false,
            connectivity_guard: false,
        }
    }

    /// A policy that applies every reversible remedy at or above `minimum_severity`, but only
    /// touches lock-out-risk controls when a guard is present.
    #[must_use]
    pub fn auto(minimum_severity: Severity) -> Self {
        Self {
            mode: PolicyMode::AutoRemediate,
            minimum_severity,
            allow_lockout_risk: false,
            connectivity_guard: false,
        }
    }

    /// The same policy with a connectivity-safety guard present and lock-out risk allowed.
    #[must_use]
    pub fn with_lockout_guard(mut self, guard_present: bool) -> Self {
        self.allow_lockout_risk = true;
        self.connectivity_guard = guard_present;
        self
    }

    /// Whether a lock-out-risk remedy may be applied under this policy.
    #[must_use]
    pub fn may_apply_lockout(&self) -> bool {
        self.allow_lockout_risk && self.connectivity_guard
    }
}

impl Default for Policy {
    fn default() -> Self {
        Self::suggest()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn control_ids_validate_their_shape() {
        assert!(ControlId::try_new("ssh.root_login").is_ok());
        assert!(ControlId::try_new("windows.firewall_profiles").is_ok());
        assert!(ControlId::try_new("one").is_err());
        assert!(ControlId::try_new("Ssh.root").is_err());
        assert!(ControlId::try_new("ssh..root").is_err());
        assert!(ControlId::try_new("ssh.root-login").is_err());
    }

    #[test]
    fn fact_keys_validate_their_shape() {
        assert!(FactKey::try_new("ssh.enabled").is_ok());
        assert!(FactKey::try_new("windows.firewall.public_enabled").is_ok());
        assert!(FactKey::try_new("enabled").is_err());
        assert!(FactKey::try_new("Ssh.enabled").is_err());
    }

    #[test]
    fn severity_is_ordered_low_to_critical() {
        let mut severities = [
            Severity::Critical,
            Severity::Low,
            Severity::High,
            Severity::Medium,
        ];
        severities.sort();
        assert_eq!(
            severities,
            [
                Severity::Low,
                Severity::Medium,
                Severity::High,
                Severity::Critical
            ]
        );
    }

    #[test]
    fn auto_remediable_needs_a_remedy_and_reversibility() {
        let op = OpRef::new(OpId::new("os.service.enable").unwrap());
        let mut control = Control {
            id: ControlId::try_new("time.sync").unwrap(),
            title: "Time sync is on".to_owned(),
            os_family: OsFamily::Linux,
            check: CheckSpec::Bool {
                fact: FactKey::try_new("time.synchronised").unwrap(),
                expect: true,
            },
            remedy: Some(op),
            severity: Severity::Medium,
            reversible: true,
            lockout_risk: false,
        };
        assert!(control.auto_remediable());
        control.reversible = false;
        assert!(!control.auto_remediable());
    }

    #[test]
    fn a_policy_never_applies_lockout_without_a_guard() {
        let mut policy = Policy::auto(Severity::Low);
        assert!(!policy.may_apply_lockout());
        policy = policy.with_lockout_guard(false);
        assert!(!policy.may_apply_lockout());
        policy = policy.with_lockout_guard(true);
        assert!(policy.may_apply_lockout());
    }

    #[test]
    fn ids_and_facts_derive_serde_through_validation() {
        let id = ControlId::try_new("ssh.root_login").unwrap();
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"ssh.root_login\"");
        assert_eq!(serde_json::from_str::<ControlId>(&json).unwrap(), id);
        assert!(serde_json::from_str::<ControlId>("\"one\"").is_err());

        let key = FactKey::try_new("ssh.enabled").unwrap();
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, "\"ssh.enabled\"");
        assert_eq!(serde_json::from_str::<FactKey>(&json).unwrap(), key);
        assert!(serde_json::from_str::<FactKey>("\"enabled\"").is_err());
    }
}
