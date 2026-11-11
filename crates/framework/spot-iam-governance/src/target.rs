//! The validated escalation target.
//!
//! An approval rule whose `escalate_to` is empty could match an empty actor id and so grant
//! approval to nobody in particular. [`EscalationTarget`] makes that unrepresentable: it is
//! built only through the fallible [`EscalationTarget::new`], and it is the type a stored or
//! configured [`ApprovalPolicy`](crate::ApprovalPolicy) is loaded into, so a bad value fails
//! the whole load. The matching helpers here ([`valid_actor`], [`target_matches`]) are what
//! the approval check uses, so an empty actor id never matches anything.

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::GovError;

/// The longest escalation target or actor id, in bytes (the directory's sign-in name bound).
pub const MAX_TARGET_BYTES: usize = 254;

/// The prefix that makes a target a role reference the BPM task layer resolves.
pub(crate) const ROLE_PREFIX: &str = "role:";

/// Whether `text` is a usable actor id or name: non-blank, no control characters, bounded.
#[must_use]
pub(crate) fn valid_text(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_TARGET_BYTES && !text.chars().any(char::is_control)
}

/// Whether `target` is a valid approver or escalation target: a valid actor id, or `role:`
/// followed by a valid role name.
#[must_use]
pub(crate) fn valid_target(target: &str) -> bool {
    if !valid_text(target) || target != target.trim() {
        return false;
    }
    match target.strip_prefix(ROLE_PREFIX) {
        Some(role) => valid_text(role) && role == role.trim(),
        None => true,
    }
}

/// Whether `by` is a usable actor id. An empty, blank or control-character actor id is never
/// one, and never matches any target.
#[must_use]
pub(crate) fn valid_actor(by: &str) -> bool {
    valid_text(by) && by == by.trim()
}

/// Whether the actor `by` is the target: the same name, or, for a `role:<name>` target, a user
/// for whom `holds_role(by, name)` is true. An invalid actor or an invalid target matches
/// nothing.
pub(crate) fn target_matches(
    target: &str,
    by: &str,
    holds_role: &dyn Fn(&str, &str) -> bool,
) -> bool {
    if !valid_actor(by) || !valid_target(target) {
        return false;
    }
    match target.strip_prefix(ROLE_PREFIX) {
        Some(role) => holds_role(by, role),
        None => by == target,
    }
}

/// Who an expired approval escalates to: an actor id or a `role:<name>` reference, proven
/// non-blank, free of control characters and at most [`MAX_TARGET_BYTES`] long.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct EscalationTarget(String);

impl EscalationTarget {
    /// The target `text`, trimmed.
    ///
    /// # Errors
    ///
    /// [`GovError::RequestInvalid`] (code `iam_governance.request_invalid`) when the text is
    /// empty or blank after trimming, has a control character, is longer than
    /// [`MAX_TARGET_BYTES`], or is `role:` with no valid role name. The error never contains
    /// the value.
    pub fn new(text: &str) -> Result<EscalationTarget, GovError> {
        let trimmed = text.trim();
        if valid_target(trimmed) {
            Ok(EscalationTarget(trimmed.to_owned()))
        } else {
            Err(GovError::RequestInvalid {
                subject: "(escalation target)".to_owned(),
                detail: format!(
                    "an escalation target needs an actor id or `role:<name>` with text, no \
                     control characters and at most {MAX_TARGET_BYTES} bytes"
                ),
            })
        }
    }

    /// The target as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for EscalationTarget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        EscalationTarget::new(&text).map_err(|e| serde::de::Error::custom(e.code()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_usable_target_is_accepted_and_trimmed() {
        assert_eq!(EscalationTarget::new(" dan ").expect("ok").as_str(), "dan");
        assert!(EscalationTarget::new("role:ceo").is_ok());
    }

    #[test]
    fn bad_targets_are_refused_without_echoing_the_value() {
        let long = "x".repeat(MAX_TARGET_BYTES + 1);
        for bad in ["", "   ", "a\0b", "a\nb", "role:", "role:  ", long.as_str()] {
            let err = EscalationTarget::new(bad).expect_err("refused");
            assert_eq!(err.code(), crate::codes::REQUEST_INVALID);
            assert!(!format!("{err}").contains("a\0b"));
        }
    }

    #[test]
    fn an_empty_actor_matches_nothing() {
        let any_role = |_: &str, _: &str| true;
        assert!(!target_matches("", "", &any_role));
        assert!(!target_matches("dan", "", &any_role));
        assert!(!target_matches("role:ceo", "", &any_role));
        assert!(!target_matches("role:ceo", "  ", &any_role));
        assert!(!target_matches("", "dan", &any_role));
        assert!(target_matches("dan", "dan", &any_role));
        assert!(target_matches("role:ceo", "dan", &any_role));
    }
}
