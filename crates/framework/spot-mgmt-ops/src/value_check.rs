//! The typed value checks: one function, [`check_value`], used twice — the catalogue
//! validates defaults through it at build time and invocations at call time.

use crate::error::InvocationError;
use crate::model::{ParamType, ParamValue};

/// Checks one value against its declared type, pushing every problem it finds into
/// `problems`. Shape first, then bounds, enum membership and length — a value never gets
/// more than one problem per aspect, and never a cascade of follow-up errors about the
/// value it just rejected.
pub(crate) fn check_value(
    op: &str,
    param: &str,
    ty: &ParamType,
    value: &ParamValue,
    problems: &mut Vec<InvocationError>,
) {
    check_value_inner(op, param, ty, value, 0, problems);
}

/// Checks one value against its declared type for a caller outside an operation, for example a
/// setting (WP-ADM-07): the same rules as an invocation, from the one implementation above.
/// `subject` names the thing being checked (a setting key, say); it fills the `param` of every
/// problem and the `op` is empty. An empty result means the value is valid.
#[must_use]
pub fn check_param_value(
    subject: &str,
    ty: &ParamType,
    value: &ParamValue,
) -> Vec<InvocationError> {
    let mut problems = Vec::new();
    check_value("", subject, ty, value, &mut problems);
    problems
}

/// The recursive form; the catalogue refuses nested list types at build time, so any depth
/// beyond a flat list here is a shape error by construction.
fn check_value_inner(
    op: &str,
    param: &str,
    ty: &ParamType,
    value: &ParamValue,
    depth: usize,
    problems: &mut Vec<InvocationError>,
) {
    match (ty, value) {
        (ParamType::Bool, ParamValue::Bool(_)) => {}
        (ParamType::Int { min, max }, ParamValue::Int(actual)) => {
            if *actual < *min || *actual > *max {
                problems.push(InvocationError::OutOfRange {
                    op: op.to_owned(),
                    param: param.to_owned(),
                    value: *actual,
                    min: *min,
                    max: *max,
                });
            }
        }
        (ParamType::Text { max_len, pattern }, ParamValue::Text(text)) => {
            // max_len counts CHARACTERS, not bytes, so non-ASCII text is not capped early.
            let char_count = text.chars().count();
            if char_count > *max_len {
                problems.push(InvocationError::TooLong {
                    op: op.to_owned(),
                    param: param.to_owned(),
                    max_len: *max_len,
                });
            }
            // The pattern check is the WHOLE value (the catalogue anchors the author's
            // pattern); an uncompilable pattern here means a hand-built invocation, which
            // the catalogue would have refused at build time.
            if let Some(pattern) = pattern {
                match whole_match(pattern, text) {
                    Ok(true) => {}
                    Ok(false) => problems.push(InvocationError::PatternMismatch {
                        op: op.to_owned(),
                        param: param.to_owned(),
                    }),
                    // The catalogue refuses an uncompilable pattern; if one arrives anyway
                    // (hand-built invocation), report the value as wrong-typed rather
                    // than silently passing it.
                    Err(()) => problems.push(type_error(op, param)),
                }
            }
        }
        (ParamType::Enum(choices), ParamValue::Text(text)) => {
            if !choices.iter().any(|choice| choice == text) {
                problems.push(InvocationError::NotInEnum {
                    op: op.to_owned(),
                    param: param.to_owned(),
                });
            }
        }
        (ParamType::Duration, ParamValue::Duration(_)) => {
            // A duration is already non-negative by construction (`u64`); operation
            // modules that need a ceiling declare it as an `Int` instead.
        }
        (ParamType::List(inner), ParamValue::List(items)) => {
            if depth >= MAX_LIST_DEPTH {
                problems.push(type_error(op, param));
                return;
            }
            for item in items {
                check_value_inner(op, param, inner, item, depth + 1, problems);
            }
        }
        // Every other pairing is a shape error: one problem, no follow-ups.
        _ => problems.push(type_error(op, param)),
    }
}

/// Whether `text` fully matches `pattern` (`Ok(false)` = a real mismatch, `Err(())` = the
/// pattern itself is not a legal regex).
fn whole_match(pattern: &str, text: &str) -> Result<bool, ()> {
    let Ok(re) = regex::Regex::new(&format!("^(?:{pattern})$")) else {
        return Err(());
    };
    Ok(re.is_match(text))
}

/// The wrong-type problem, one per offending value.
fn type_error(op: &str, param: &str) -> InvocationError {
    InvocationError::WrongType {
        op: op.to_owned(),
        param: param.to_owned(),
    }
}

/// How deep a list value may nest (a flat list is depth 1). The catalogue forbids nested
/// list element types at build time; this guards hand-built invocations.
pub(crate) const MAX_LIST_DEPTH: usize = 4;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standalone_check_applies_the_invocation_rules() {
        let ty = ParamType::Int { min: 1, max: 10 };
        assert!(check_param_value("runtime.jobs.workers", &ty, &ParamValue::Int(5)).is_empty());
        assert_eq!(
            check_param_value("runtime.jobs.workers", &ty, &ParamValue::Int(11)).len(),
            1
        );
        assert_eq!(
            check_param_value("runtime.jobs.workers", &ty, &ParamValue::Bool(true)).len(),
            1
        );
        let en = ParamType::Enum(vec!["a".to_owned(), "b".to_owned()]);
        assert!(check_param_value("k", &en, &ParamValue::Text("a".to_owned())).is_empty());
        assert_eq!(
            check_param_value("k", &en, &ParamValue::Text("c".to_owned())).len(),
            1
        );
    }
}
