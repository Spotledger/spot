//! Evaluating one [`CheckSpec`] against a [`FactSource`].
//!
//! Evaluation is total: a check never panics and never returns an error to its caller. A fact
//! the host does not report makes the control [`Outcome::NotApplicable`]; a fact source that
//! cannot answer makes it [`Outcome::Error`]; anything else is a pass or an explainable fail.
//! Every fail names both what the host reports (`found`) and what the baseline requires
//! (`expected`), so the report is readable without the baseline beside it.
//!
//! The combinators are order-independent: [`CheckSpec::All`] is a fail when any child fails,
//! otherwise an error when any child errors, otherwise not applicable when any child is, and a
//! pass otherwise; [`CheckSpec::Any`] is a pass when any child passes, otherwise an error, then
//! not applicable, then a fail.

use crate::facts::FactSource;
use crate::model::{CheckSpec, FactKey, FactValue};
use crate::report::Outcome;

/// Evaluates `check` against `source`.
#[must_use]
pub fn check_outcome(check: &CheckSpec, source: &dyn FactSource) -> Outcome {
    match check {
        CheckSpec::Bool { fact, expect } => match read(source, fact) {
            Err(outcome) => outcome,
            Ok(None) => not_reported(fact),
            Ok(Some(FactValue::Bool(actual))) => {
                if actual == *expect {
                    Outcome::Pass
                } else {
                    Outcome::Fail {
                        found: format!("`{actual}`"),
                        expected: format!("`{expect}`"),
                    }
                }
            }
            Ok(Some(other)) => type_error(fact, "a boolean", &other),
        },
        CheckSpec::Text { fact, expect } => match read(source, fact) {
            Err(outcome) => outcome,
            Ok(None) => not_reported(fact),
            Ok(Some(FactValue::Text(actual))) => {
                if actual == *expect {
                    Outcome::Pass
                } else {
                    Outcome::Fail {
                        found: format!("`{actual}`"),
                        expected: format!("`{expect}`"),
                    }
                }
            }
            Ok(Some(other)) => type_error(fact, "text", &other),
        },
        CheckSpec::TextIn { fact, allow } => {
            if allow.is_empty() {
                return Outcome::Error {
                    detail: format!("{}: the allow list is empty", fact.as_str()),
                };
            }
            match read(source, fact) {
                Err(outcome) => outcome,
                Ok(None) => not_reported(fact),
                Ok(Some(FactValue::Text(actual))) => {
                    if allow.contains(&actual) {
                        Outcome::Pass
                    } else {
                        Outcome::Fail {
                            found: format!("`{actual}`"),
                            expected: format!("one of {}", quoted_list(allow)),
                        }
                    }
                }
                Ok(Some(other)) => type_error(fact, "text", &other),
            }
        }
        CheckSpec::Int { fact, min, max } => match read(source, fact) {
            Err(outcome) => outcome,
            Ok(None) => not_reported(fact),
            Ok(Some(FactValue::Int(actual))) => {
                let low_ok = min.is_none_or(|bound| actual >= bound);
                let high_ok = max.is_none_or(|bound| actual <= bound);
                if low_ok && high_ok {
                    Outcome::Pass
                } else {
                    Outcome::Fail {
                        found: format!("`{actual}`"),
                        expected: range_description(*min, *max),
                    }
                }
            }
            Ok(Some(other)) => type_error(fact, "a whole number", &other),
        },
        CheckSpec::Present { fact } => match read(source, fact) {
            Err(outcome) => outcome,
            Ok(Some(_)) => Outcome::Pass,
            Ok(None) => Outcome::Fail {
                found: "not reported".to_owned(),
                expected: "a reported value".to_owned(),
            },
        },
        CheckSpec::Absent { fact } => match read(source, fact) {
            Err(outcome) => outcome,
            Ok(None) => Outcome::Pass,
            Ok(Some(value)) => Outcome::Fail {
                found: describe(&value),
                expected: "no value".to_owned(),
            },
        },
        CheckSpec::Subset { fact, allowed_fact } => evaluate_subset(source, fact, allowed_fact),
        CheckSpec::All { checks } => evaluate_all(checks, source),
        CheckSpec::Any { checks } => evaluate_any(checks, source),
    }
}

/// The subset check: every element of `fact` must be one of `allowed_fact`'s elements.
fn evaluate_subset(source: &dyn FactSource, fact: &FactKey, allowed_fact: &FactKey) -> Outcome {
    let actual = match read(source, fact) {
        Ok(value) => value,
        Err(outcome) => return outcome,
    };
    let allowed = match read(source, allowed_fact) {
        Ok(value) => value,
        Err(outcome) => return outcome,
    };
    match (actual, allowed) {
        (None, _) => not_reported(fact),
        (Some(_), None) => not_reported(allowed_fact),
        (Some(FactValue::TextList(items)), Some(FactValue::TextList(allow))) => {
            let extra: Vec<&String> = items.iter().filter(|item| !allow.contains(item)).collect();
            if extra.is_empty() {
                Outcome::Pass
            } else {
                Outcome::Fail {
                    found: quoted_list_of_refs(&extra),
                    expected: format!("only {}", quoted_list(&allow)),
                }
            }
        }
        (Some(FactValue::TextList(_)), Some(other)) => {
            type_error(allowed_fact, "a list of text", &other)
        }
        (Some(other), Some(_)) => type_error(fact, "a list of text", &other),
    }
}

/// `all`: a fail wins, then an error, then not applicable, then a pass.
fn evaluate_all(checks: &[CheckSpec], source: &dyn FactSource) -> Outcome {
    if checks.is_empty() {
        return Outcome::Error {
            detail: "an `all` check has no nested checks".to_owned(),
        };
    }
    let mut fail: Option<Outcome> = None;
    let mut error: Option<Outcome> = None;
    let mut not_applicable: Option<Outcome> = None;
    for nested in checks {
        match check_outcome(nested, source) {
            Outcome::Pass => {}
            outcome @ Outcome::Fail { .. } => {
                fail.get_or_insert(outcome);
            }
            outcome @ Outcome::Error { .. } => {
                error.get_or_insert(outcome);
            }
            outcome @ Outcome::NotApplicable { .. } => {
                not_applicable.get_or_insert(outcome);
            }
        }
    }
    fail.or(error).or(not_applicable).unwrap_or(Outcome::Pass)
}

/// `any`: a pass wins, then an error, then not applicable, then a fail.
fn evaluate_any(checks: &[CheckSpec], source: &dyn FactSource) -> Outcome {
    if checks.is_empty() {
        return Outcome::Error {
            detail: "an `any` check has no nested checks".to_owned(),
        };
    }
    let mut passed = false;
    let mut error: Option<Outcome> = None;
    let mut not_applicable: Option<Outcome> = None;
    let mut fail: Option<Outcome> = None;
    for nested in checks {
        match check_outcome(nested, source) {
            Outcome::Pass => passed = true,
            outcome @ Outcome::Error { .. } => {
                error.get_or_insert(outcome);
            }
            outcome @ Outcome::NotApplicable { .. } => {
                not_applicable.get_or_insert(outcome);
            }
            outcome @ Outcome::Fail { .. } => {
                fail.get_or_insert(outcome);
            }
        }
    }
    if passed {
        Outcome::Pass
    } else {
        error.or(not_applicable).or(fail).unwrap_or(Outcome::Pass)
    }
}

/// Reads one fact, mapping a source error to the control's error outcome.
fn read(source: &dyn FactSource, key: &FactKey) -> Result<Option<FactValue>, Outcome> {
    source.fact(key).map_err(|error| Outcome::Error {
        detail: error.to_string(),
    })
}

/// The outcome for a fact the host does not report.
fn not_reported(key: &FactKey) -> Outcome {
    Outcome::NotApplicable {
        reason: format!("{} is not reported on this host", key.as_str()),
    }
}

/// The outcome for a fact of the wrong type.
fn type_error(key: &FactKey, expected: &str, found: &FactValue) -> Outcome {
    Outcome::Error {
        detail: format!(
            "{} is not {expected} (found {})",
            key.as_str(),
            describe(found)
        ),
    }
}

/// A one-line description of a fact value.
fn describe(value: &FactValue) -> String {
    match value {
        FactValue::Bool(value) => format!("`{value}`"),
        FactValue::Text(value) => format!("`{value}`"),
        FactValue::Int(value) => format!("`{value}`"),
        FactValue::TextList(values) => quoted_list(values),
    }
}

/// A quoted, comma-separated list.
fn quoted_list(values: &[String]) -> String {
    let refs: Vec<&String> = values.iter().collect();
    quoted_list_of_refs(&refs)
}

/// A quoted, comma-separated list of borrowed strings.
fn quoted_list_of_refs(values: &[&String]) -> String {
    values
        .iter()
        .map(|value| format!("`{value}`"))
        .collect::<Vec<String>>()
        .join(", ")
}

/// A human description of an integer range.
fn range_description(min: Option<i64>, max: Option<i64>) -> String {
    match (min, max) {
        (Some(min), Some(max)) => format!("`{min}..={max}`"),
        (Some(min), None) => format!("`>= {min}`"),
        (None, Some(max)) => format!("`<= {max}`"),
        (None, None) => "any whole number".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::facts::Facts;
    use crate::model::FactKey;

    fn key(text: &str) -> FactKey {
        FactKey::try_new(text).unwrap()
    }

    fn facts() -> Facts {
        Facts::new()
            .with(key("ssh.enabled"), FactValue::Bool(true))
            .with(
                key("ssh.permit_root_login"),
                FactValue::Text("yes".to_owned()),
            )
            .with(key("mac.mode"), FactValue::Text("enforcing".to_owned()))
            .with(key("kernel.threads_max"), FactValue::Int(100))
            .with(
                key("services.running"),
                FactValue::TextList(vec!["sshd".to_owned(), "telnet".to_owned()]),
            )
            .with(
                key("services.allowed"),
                FactValue::TextList(vec!["sshd".to_owned()]),
            )
    }

    #[test]
    fn bool_passes_and_fails_explainably() {
        let source = facts();
        assert_eq!(
            check_outcome(
                &CheckSpec::Bool {
                    fact: key("ssh.enabled"),
                    expect: true,
                },
                &source
            ),
            Outcome::Pass
        );
        assert_eq!(
            check_outcome(
                &CheckSpec::Bool {
                    fact: key("ssh.enabled"),
                    expect: false,
                },
                &source
            ),
            Outcome::Fail {
                found: "`true`".to_owned(),
                expected: "`false`".to_owned(),
            }
        );
    }

    #[test]
    fn a_missing_fact_is_not_applicable() {
        let source = facts();
        let outcome = check_outcome(
            &CheckSpec::Bool {
                fact: key("selinux.enabled"),
                expect: true,
            },
            &source,
        );
        assert!(matches!(outcome, Outcome::NotApplicable { .. }));
    }

    #[test]
    fn a_wrong_type_is_an_error() {
        let source = facts();
        let outcome = check_outcome(
            &CheckSpec::Bool {
                fact: key("mac.mode"),
                expect: true,
            },
            &source,
        );
        assert!(matches!(outcome, Outcome::Error { .. }));
    }

    #[test]
    fn text_in_accepts_any_listed_value() {
        let source = facts();
        assert_eq!(
            check_outcome(
                &CheckSpec::TextIn {
                    fact: key("mac.mode"),
                    allow: vec!["enforcing".to_owned()],
                },
                &source
            ),
            Outcome::Pass
        );
        assert!(matches!(
            check_outcome(
                &CheckSpec::TextIn {
                    fact: key("mac.mode"),
                    allow: vec!["disabled".to_owned()],
                },
                &source
            ),
            Outcome::Fail { .. }
        ));
    }

    #[test]
    fn int_bounds_are_open_to_either_side() {
        let source = facts();
        assert_eq!(
            check_outcome(
                &CheckSpec::Int {
                    fact: key("kernel.threads_max"),
                    min: Some(50),
                    max: None,
                },
                &source
            ),
            Outcome::Pass
        );
        assert!(matches!(
            check_outcome(
                &CheckSpec::Int {
                    fact: key("kernel.threads_max"),
                    min: Some(200),
                    max: None,
                },
                &source
            ),
            Outcome::Fail { .. }
        ));
    }

    #[test]
    fn subset_flags_only_the_extra_elements() {
        let source = facts();
        let outcome = check_outcome(
            &CheckSpec::Subset {
                fact: key("services.running"),
                allowed_fact: key("services.allowed"),
            },
            &source,
        );
        assert_eq!(
            outcome,
            Outcome::Fail {
                found: "`telnet`".to_owned(),
                expected: "only `sshd`".to_owned(),
            }
        );
    }

    #[test]
    fn present_and_absent_judge_the_key_itself() {
        let source = facts();
        assert_eq!(
            check_outcome(
                &CheckSpec::Present {
                    fact: key("mac.mode")
                },
                &source
            ),
            Outcome::Pass
        );
        assert!(matches!(
            check_outcome(
                &CheckSpec::Absent {
                    fact: key("mac.mode")
                },
                &source
            ),
            Outcome::Fail { .. }
        ));
        assert_eq!(
            check_outcome(
                &CheckSpec::Absent {
                    fact: key("smb1.enabled")
                },
                &source
            ),
            Outcome::Pass
        );
    }

    #[test]
    fn all_and_any_combine_their_children() {
        let source = facts();
        let all = CheckSpec::All {
            checks: vec![
                CheckSpec::Bool {
                    fact: key("ssh.enabled"),
                    expect: true,
                },
                CheckSpec::Text {
                    fact: key("mac.mode"),
                    expect: "enforcing".to_owned(),
                },
            ],
        };
        assert_eq!(check_outcome(&all, &source), Outcome::Pass);

        let any = CheckSpec::Any {
            checks: vec![
                CheckSpec::Bool {
                    fact: key("ssh.enabled"),
                    expect: false,
                },
                CheckSpec::TextIn {
                    fact: key("mac.mode"),
                    allow: vec!["enforcing".to_owned()],
                },
            ],
        };
        assert_eq!(check_outcome(&any, &source), Outcome::Pass);

        let none = CheckSpec::Any {
            checks: vec![
                CheckSpec::Bool {
                    fact: key("ssh.enabled"),
                    expect: false,
                },
                CheckSpec::Text {
                    fact: key("mac.mode"),
                    expect: "disabled".to_owned(),
                },
            ],
        };
        assert!(matches!(
            check_outcome(&none, &source),
            Outcome::Fail { .. }
        ));
    }

    #[test]
    fn an_any_that_only_reports_unknown_facts_is_not_applicable() {
        let source = facts();
        let any = CheckSpec::Any {
            checks: vec![
                CheckSpec::Bool {
                    fact: key("selinux.enabled"),
                    expect: true,
                },
                CheckSpec::Bool {
                    fact: key("apparmor.enabled"),
                    expect: true,
                },
            ],
        };
        assert!(matches!(
            check_outcome(&any, &source),
            Outcome::NotApplicable { .. }
        ));
    }

    #[test]
    fn an_empty_allow_list_is_an_error() {
        let source = facts();
        let outcome = check_outcome(
            &CheckSpec::TextIn {
                fact: key("mac.mode"),
                allow: Vec::new(),
            },
            &source,
        );
        assert!(matches!(outcome, Outcome::Error { .. }));
    }
}
