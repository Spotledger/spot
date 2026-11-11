//! The hardening error vocabulary, with stable `hardening.*` codes.
//!
//! Every failure the crate can report is a [`HardeningError`] carrying a stable,
//! machine-readable code ([`HardeningError::code`]). The codes are catalogued in
//! `spot-errors/catalogue/hardening.toml`; they are a library-internal vocabulary (the host
//! agent bridges them onto the wire result), so every catalogue entry is class `none`.
//!
//! Messages name the control, the fact or the operation at fault and never a secret, a
//! parameter value or the raw bytes of an agent's output.

use thiserror::Error;

/// The baseline text does not parse or does not have the expected shape.
pub const BASELINE_PARSE: &str = "hardening.baseline_parse";
/// Two controls in one baseline share an id.
pub const CONTROL_DUPLICATE: &str = "hardening.control_duplicate";
/// A control's shape is invalid (its title, its OS family, or a remedy parameter name).
pub const CONTROL_INVALID: &str = "hardening.control_invalid";
/// A control's check is structurally invalid (an empty list, an inverted range).
pub const CHECK_INVALID: &str = "hardening.check_invalid";
/// A fact key or value is not valid.
pub const FACT_INVALID: &str = "hardening.fact_invalid";
/// A fact source could not produce a fact.
pub const FACT_UNAVAILABLE: &str = "hardening.fact_unavailable";
/// A remedy does not name a valid operation and parameter set.
pub const REMEDY_INVALID: &str = "hardening.remedy_invalid";
/// A compliance report could not be read back from its structured form.
pub const REPORT_INVALID: &str = "hardening.report_invalid";

/// Why a baseline, a fact source, a remedy or a report could not be used.
///
/// Every variant carries a stable code ([`HardeningError::code`]) and a short, secret-free
/// description. `Debug` prints the same fields, so a dumped error never leaks a value.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum HardeningError {
    /// The baseline text does not parse (see [`BASELINE_PARSE`]).
    #[error("the baseline text does not parse: {detail}")]
    BaselineParse {
        /// Why parsing failed, in words (never the raw text).
        detail: String,
    },
    /// Two controls share an id (see [`CONTROL_DUPLICATE`]).
    #[error("two controls in one baseline share the id {id}")]
    ControlDuplicate {
        /// The repeated control id.
        id: String,
    },
    /// A control's shape is invalid (see [`CONTROL_INVALID`]).
    #[error("control {id} is not valid: {detail}")]
    ControlInvalid {
        /// The control id, or the offending text when the id itself was refused.
        id: String,
        /// What is wrong, in words.
        detail: String,
    },
    /// A check is structurally invalid (see [`CHECK_INVALID`]).
    #[error("the check on control {id} is not valid: {detail}")]
    CheckInvalid {
        /// The control id whose check is at fault.
        id: String,
        /// What is wrong, in words.
        detail: String,
    },
    /// A fact key or value is not valid (see [`FACT_INVALID`]).
    #[error("the fact {fact} is not valid: {detail}")]
    FactInvalid {
        /// The offending fact key, or the text that was refused as one.
        fact: String,
        /// What is wrong, in words.
        detail: String,
    },
    /// A fact source could not produce a fact (see [`FACT_UNAVAILABLE`]).
    #[error("the fact source could not read {fact}: {detail}")]
    FactUnavailable {
        /// The fact key the source was asked for.
        fact: String,
        /// Why the source failed, in words (never a raw command output).
        detail: String,
    },
    /// A remedy does not name a valid operation and parameter set (see [`REMEDY_INVALID`]).
    #[error("the remedy for control {id} is not valid: {detail}")]
    RemedyInvalid {
        /// The control whose remedy is at fault.
        id: String,
        /// What is wrong, in words.
        detail: String,
    },
    /// A compliance report could not be read back (see [`REPORT_INVALID`]).
    #[error("a compliance report could not be read: {detail}")]
    ReportInvalid {
        /// Why decoding failed, in words.
        detail: String,
    },
}

impl HardeningError {
    /// The stable machine-readable code of this error. Never a parameter value.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::BaselineParse { .. } => BASELINE_PARSE,
            Self::ControlDuplicate { .. } => CONTROL_DUPLICATE,
            Self::ControlInvalid { .. } => CONTROL_INVALID,
            Self::CheckInvalid { .. } => CHECK_INVALID,
            Self::FactInvalid { .. } => FACT_INVALID,
            Self::FactUnavailable { .. } => FACT_UNAVAILABLE,
            Self::RemedyInvalid { .. } => REMEDY_INVALID,
            Self::ReportInvalid { .. } => REPORT_INVALID,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn codes_are_dotted_lower_case_under_the_crate_prefix() {
        let errors = [
            HardeningError::BaselineParse {
                detail: "missing field".to_owned(),
            },
            HardeningError::ControlDuplicate {
                id: "ssh.root_login".to_owned(),
            },
            HardeningError::ControlInvalid {
                id: "ssh.root_login".to_owned(),
                detail: "empty title".to_owned(),
            },
            HardeningError::CheckInvalid {
                id: "ssh.root_login".to_owned(),
                detail: "empty allow list".to_owned(),
            },
            HardeningError::FactInvalid {
                fact: "ssh.enabled".to_owned(),
                detail: "not dotted".to_owned(),
            },
            HardeningError::FactUnavailable {
                fact: "ssh.enabled".to_owned(),
                detail: "no agent".to_owned(),
            },
            HardeningError::RemedyInvalid {
                id: "ssh.root_login".to_owned(),
                detail: "unknown operation".to_owned(),
            },
            HardeningError::ReportInvalid {
                detail: "bad json".to_owned(),
            },
        ];
        for error in errors {
            let code = error.code();
            assert!(code.starts_with("hardening."), "{code}");
            assert!(
                code.split('.').all(|s| {
                    !s.is_empty()
                        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase())
                        && s.chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                }),
                "{code}"
            );
        }
    }

    #[test]
    fn messages_never_carry_the_raw_value() {
        let error = HardeningError::FactUnavailable {
            fact: "ssh.enabled".to_owned(),
            detail: "the agent is not enrolled".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("ssh.enabled"));
        assert!(text.contains("not enrolled"));
    }
}
