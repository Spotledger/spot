//! The baseline: the compiled-in Linux and Windows control sets, their TOML form, and the
//! validation that keeps a hand-edited baseline sound.
//!
//! A baseline is data (TOML), parsed into a [`Baseline`]. Parsing reports every problem it
//! finds rather than the first, so a hand-edited file is fixed in one pass. Structural
//! validation ([`Baseline::problems`]) checks that control ids are unique, titles are
//! non-empty, every control's OS family matches the baseline's, every check is well formed
//! and every remedy parameter is a lower-case snake name. Validation against a live
//! [`Catalogue`] ([`Baseline::validate_against`]) additionally proves each remedy names an
//! operation and parameter set the catalogue accepts.

use std::collections::BTreeSet;

use spot_mgmt_ops::Catalogue;

use crate::error::HardeningError;
use crate::model::{CheckSpec, Control, OsFamily, is_param_name};

/// The compiled-in Linux baseline.
pub const LINUX_BASELINE: &str = include_str!("../baselines/linux.toml");
/// The compiled-in Windows baseline.
pub const WINDOWS_BASELINE: &str = include_str!("../baselines/windows.toml");

/// A hardening baseline for one OS family.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Baseline {
    /// The OS family every control in the baseline applies to.
    pub os_family: OsFamily,
    /// The controls, in declaration order (the report keeps this order).
    #[serde(rename = "control")]
    pub controls: Vec<Control>,
}

impl Baseline {
    /// Parses baseline text, then checks its structure.
    ///
    /// # Errors
    ///
    /// Every [`HardeningError`] the text has: it does not parse, a control id is repeated, a
    /// title is empty, a control's OS family does not match the baseline's, a check is
    /// malformed, or a remedy parameter name is not a lower-case snake name.
    pub fn parse(text: &str) -> Result<Self, Vec<HardeningError>> {
        let baseline: Self = toml::from_str(text).map_err(|error| {
            vec![HardeningError::BaselineParse {
                detail: error.message().to_owned(),
            }]
        })?;
        let problems = baseline.problems();
        if problems.is_empty() {
            Ok(baseline)
        } else {
            Err(problems)
        }
    }

    /// The compiled-in Linux baseline.
    ///
    /// # Errors
    ///
    /// [`HardeningError`] if the compiled-in text is invalid; a test asserts it always parses.
    pub fn linux() -> Result<Self, Vec<HardeningError>> {
        Self::parse(LINUX_BASELINE)
    }

    /// The compiled-in Windows baseline.
    ///
    /// # Errors
    ///
    /// [`HardeningError`] if the compiled-in text is invalid; a test asserts it always parses.
    pub fn windows() -> Result<Self, Vec<HardeningError>> {
        Self::parse(WINDOWS_BASELINE)
    }

    /// The control named `id`, when the baseline has one.
    #[must_use]
    pub fn control(&self, id: &crate::model::ControlId) -> Option<&Control> {
        self.controls.iter().find(|control| &control.id == id)
    }

    /// The controls, in declaration order.
    pub fn iter(&self) -> impl Iterator<Item = &Control> {
        self.controls.iter()
    }

    /// Every structural problem in the baseline, collected.
    #[must_use]
    pub fn problems(&self) -> Vec<HardeningError> {
        let mut problems = Vec::new();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for control in &self.controls {
            if !seen.insert(control.id.as_str()) {
                problems.push(HardeningError::ControlDuplicate {
                    id: control.id.as_str().to_owned(),
                });
            }
            if control.title.trim().is_empty() {
                problems.push(HardeningError::ControlInvalid {
                    id: control.id.as_str().to_owned(),
                    detail: "the title is empty".to_owned(),
                });
            }
            if control.os_family != self.os_family {
                problems.push(HardeningError::ControlInvalid {
                    id: control.id.as_str().to_owned(),
                    detail: "the control's os_family does not match the baseline's".to_owned(),
                });
            }
            check_problems(&control.id, &control.check, &mut problems);
            if let Some(remedy) = &control.remedy {
                for name in remedy.params.keys() {
                    if !is_param_name(name) {
                        problems.push(HardeningError::ControlInvalid {
                            id: control.id.as_str().to_owned(),
                            detail: format!(
                                "remedy parameter `{name}` is not a lower-case snake name"
                            ),
                        });
                    }
                }
            }
        }
        problems
    }

    /// Proves every remedy against a live catalogue: the operation must exist and every
    /// parameter must pass the catalogue's own validation.
    ///
    /// # Errors
    ///
    /// One [`HardeningError::RemedyInvalid`] per problem, naming the control.
    pub fn validate_against(&self, catalogue: &Catalogue) -> Result<(), Vec<HardeningError>> {
        let mut problems = Vec::new();
        for control in &self.controls {
            let Some(remedy) = &control.remedy else {
                continue;
            };
            if let Err(errors) = catalogue.validate(&remedy.to_invocation()) {
                for error in errors {
                    problems.push(HardeningError::RemedyInvalid {
                        id: control.id.as_str().to_owned(),
                        detail: error.to_string(),
                    });
                }
            }
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }
}

/// Collects the structural problems of one check, recursing through the combinators.
fn check_problems(id: &crate::model::ControlId, check: &CheckSpec, out: &mut Vec<HardeningError>) {
    match check {
        CheckSpec::TextIn { allow, .. } if allow.is_empty() => {
            out.push(HardeningError::CheckInvalid {
                id: id.as_str().to_owned(),
                detail: "a `text_in` check has an empty allow list".to_owned(),
            });
        }
        CheckSpec::Int { min, max, .. } if matches!((min, max), (Some(min), Some(max)) if min > max) =>
        {
            out.push(HardeningError::CheckInvalid {
                id: id.as_str().to_owned(),
                detail: "an `int` check has `min` greater than `max`".to_owned(),
            });
        }
        CheckSpec::Subset { fact, allowed_fact } if fact == allowed_fact => {
            out.push(HardeningError::CheckInvalid {
                id: id.as_str().to_owned(),
                detail: "a `subset` check compares a fact with itself".to_owned(),
            });
        }
        CheckSpec::All { checks } | CheckSpec::Any { checks } if checks.is_empty() => {
            out.push(HardeningError::CheckInvalid {
                id: id.as_str().to_owned(),
                detail: "a combinator check has no nested checks".to_owned(),
            });
        }
        CheckSpec::All { checks } | CheckSpec::Any { checks } => {
            for nested in checks {
                check_problems(id, nested, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::model::Severity;

    #[test]
    fn the_compiled_in_baselines_parse() {
        let linux = Baseline::linux().unwrap();
        assert_eq!(linux.os_family, OsFamily::Linux);
        assert!(linux.controls.len() >= 10);
        let windows = Baseline::windows().unwrap();
        assert_eq!(windows.os_family, OsFamily::Windows);
        assert!(windows.controls.len() >= 5);
    }

    #[test]
    fn the_linux_baseline_covers_every_required_control() {
        let baseline = Baseline::linux().unwrap();
        let ids: BTreeSet<&str> = baseline.controls.iter().map(|c| c.id.as_str()).collect();
        for required in [
            "ssh.root_login",
            "ssh.password_auth",
            "mac.enforcing",
            "firewall.default_deny",
            "firewall.declared_ports",
            "updates.security_auto",
            "services.unnecessary",
            "time.sync",
            "kernel.sysctls",
            "files.spotledger_permissions",
        ] {
            assert!(ids.contains(required), "{required} is missing");
        }
    }

    #[test]
    fn the_windows_baseline_covers_every_required_control() {
        let baseline = Baseline::windows().unwrap();
        let ids: BTreeSet<&str> = baseline.controls.iter().map(|c| c.id.as_str()).collect();
        for required in [
            "windows.firewall_profiles",
            "windows.smbv1",
            "windows.rdp_nla",
            "windows.defender",
            "windows.uac",
        ] {
            assert!(ids.contains(required), "{required} is missing");
        }
    }

    #[test]
    fn a_duplicate_control_id_is_reported() {
        let text = r#"
os_family = "linux"

[[control]]
id = "a.one"
title = "One"
os_family = "linux"
severity = "low"
reversible = false
[control.check]
kind = "bool"
fact = "a.flag"
expect = true

[[control]]
id = "a.one"
title = "One again"
os_family = "linux"
severity = "low"
reversible = false
[control.check]
kind = "bool"
fact = "a.flag"
expect = true
"#;
        let problems = Baseline::parse(text).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|p| p.code() == crate::error::CONTROL_DUPLICATE)
        );
    }

    #[test]
    fn an_os_family_mismatch_is_reported() {
        let text = r#"
os_family = "linux"

[[control]]
id = "a.one"
title = "One"
os_family = "windows"
severity = "low"
reversible = false
[control.check]
kind = "bool"
fact = "a.flag"
expect = true
"#;
        let problems = Baseline::parse(text).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|p| p.code() == crate::error::CONTROL_INVALID)
        );
    }

    #[test]
    fn an_empty_combinator_is_reported() {
        let text = r#"
os_family = "linux"

[[control]]
id = "a.one"
title = "One"
os_family = "linux"
severity = "low"
reversible = false
[control.check]
kind = "all"
checks = []
"#;
        let problems = Baseline::parse(text).unwrap_err();
        assert!(
            problems
                .iter()
                .any(|p| p.code() == crate::error::CHECK_INVALID)
        );
    }

    #[test]
    fn a_remedy_is_read_as_a_typed_operation_reference() {
        let baseline = Baseline::linux().unwrap();
        let ssh = baseline
            .control(&crate::model::ControlId::try_new("ssh.root_login").unwrap())
            .unwrap();
        let remedy = ssh.remedy.as_ref().unwrap();
        assert_eq!(remedy.op.as_str(), "os.service.disable");
        assert_eq!(ssh.severity, Severity::High);
        assert!(ssh.lockout_risk);
        assert!(ssh.auto_remediable());
    }
}
