//! Reading typed arguments out of a validated [`Validated`] invocation.
//!
//! The catalogue has already checked every value against its declared type, filled
//! defaults and refused unknown parameters. These helpers add the module's own shape
//! checks (a value present when the module needs it, the right enum spelling) and return a
//! [`WindowsError::InvalidArgument`] rather than panicking, so a hand-built `Validated` in
//! a unit test is refused the same way a malformed command would be.

use spot_mgmt_ops::{ParamValue, Validated};

use crate::error::WindowsError;

/// The raw value of `name`, when the invocation carries it.
fn find<'a>(args: &'a Validated, name: &str) -> Option<&'a ParamValue> {
    args.params
        .iter()
        .find(|param| param.name == name)
        .map(|param| &param.value)
}

/// A required value, or [`WindowsError::InvalidArgument`].
fn required<'a>(args: &'a Validated, name: &str) -> Result<&'a ParamValue, WindowsError> {
    find(args, name).ok_or_else(|| WindowsError::InvalidArgument {
        param: name.to_owned(),
        detail: "the parameter is missing".to_owned(),
    })
}

/// A required text value.
pub(crate) fn text<'a>(args: &'a Validated, name: &str) -> Result<&'a str, WindowsError> {
    match required(args, name)? {
        ParamValue::Text(value) => Ok(value),
        _ => Err(wrong_type(name, "text")),
    }
}

/// An optional text value.
pub(crate) fn optional_text<'a>(
    args: &'a Validated,
    name: &str,
) -> Result<Option<&'a str>, WindowsError> {
    match find(args, name) {
        None => Ok(None),
        Some(ParamValue::Text(value)) => Ok(Some(value)),
        Some(_) => Err(wrong_type(name, "text")),
    }
}

/// A boolean value, defaulting to `false` when absent.
pub(crate) fn boolean(args: &Validated, name: &str) -> Result<bool, WindowsError> {
    match find(args, name) {
        None => Ok(false),
        Some(ParamValue::Bool(value)) => Ok(*value),
        Some(_) => Err(wrong_type(name, "boolean")),
    }
}

/// A required integer value.
pub(crate) fn integer(args: &Validated, name: &str) -> Result<i64, WindowsError> {
    match required(args, name)? {
        ParamValue::Int(value) => Ok(*value),
        _ => Err(wrong_type(name, "integer")),
    }
}

/// A duration value in whole seconds, defaulting to `default` when absent.
pub(crate) fn duration_or(args: &Validated, name: &str, default: u64) -> Result<u64, WindowsError> {
    match find(args, name) {
        None => Ok(default),
        Some(ParamValue::Duration(value)) => Ok(*value),
        Some(_) => Err(wrong_type(name, "duration")),
    }
}

/// A list of text values, or an empty vector when absent.
pub(crate) fn text_list(args: &Validated, name: &str) -> Result<Vec<String>, WindowsError> {
    match find(args, name) {
        None => Ok(Vec::new()),
        Some(ParamValue::List(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    ParamValue::Text(value) => out.push(value.clone()),
                    _ => return Err(wrong_type(name, "a list of text")),
                }
            }
            Ok(out)
        }
        Some(_) => Err(wrong_type(name, "a list of text")),
    }
}

/// A list of integer values, or an empty vector when absent.
pub(crate) fn integer_list(args: &Validated, name: &str) -> Result<Vec<i64>, WindowsError> {
    match find(args, name) {
        None => Ok(Vec::new()),
        Some(ParamValue::List(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    ParamValue::Int(value) => out.push(*value),
                    _ => return Err(wrong_type(name, "a list of integers")),
                }
            }
            Ok(out)
        }
        Some(_) => Err(wrong_type(name, "a list of integers")),
    }
}

/// A required enum choice, as text.
pub(crate) fn choice<'a>(args: &'a Validated, name: &str) -> Result<&'a str, WindowsError> {
    match required(args, name)? {
        ParamValue::Text(value) => Ok(value),
        _ => Err(wrong_type(name, "an enum choice")),
    }
}

/// An optional enum choice, as text.
pub(crate) fn optional_choice<'a>(
    args: &'a Validated,
    name: &str,
) -> Result<Option<&'a str>, WindowsError> {
    match find(args, name) {
        None => Ok(None),
        Some(ParamValue::Text(value)) => Ok(Some(value)),
        Some(_) => Err(wrong_type(name, "an enum choice")),
    }
}

/// The error for a value whose shape does not fit the module's expectation.
fn wrong_type(name: &str, expected: &str) -> WindowsError {
    WindowsError::InvalidArgument {
        param: name.to_owned(),
        detail: format!("the value is not {expected}"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_mgmt_ops::{OpId, ParamType, ParamValue};

    use super::*;

    fn validated() -> Validated {
        Validated {
            op: OpId::new("os.service.list").unwrap(),
            params: vec![
                spot_mgmt_ops::AcceptedParam {
                    name: "name".to_owned(),
                    ty: ParamType::Text {
                        max_len: 64,
                        pattern: None,
                    },
                    value: ParamValue::Text("wuauserv".to_owned()),
                    from_default: false,
                    secret: false,
                },
                spot_mgmt_ops::AcceptedParam {
                    name: "wait".to_owned(),
                    ty: ParamType::Duration,
                    value: ParamValue::Duration(30),
                    from_default: true,
                    secret: false,
                },
                spot_mgmt_ops::AcceptedParam {
                    name: "allow_critical".to_owned(),
                    ty: ParamType::Bool,
                    value: ParamValue::Bool(true),
                    from_default: false,
                    secret: false,
                },
            ],
        }
    }

    #[test]
    fn reads_each_declared_shape() {
        let args = validated();
        assert_eq!(text(&args, "name").unwrap(), "wuauserv");
        assert_eq!(duration_or(&args, "wait", 5).unwrap(), 30);
        assert!(boolean(&args, "allow_critical").unwrap());
        assert!(!boolean(&args, "missing").unwrap());
        assert_eq!(optional_text(&args, "missing").unwrap(), None);
    }

    #[test]
    fn a_missing_required_value_is_an_invalid_argument() {
        let args = validated();
        let error = text(&args, "missing").unwrap_err();
        assert_eq!(error.code(), crate::error::INVALID_ARGUMENT);
    }

    #[test]
    fn a_wrong_shape_is_an_invalid_argument() {
        let args = validated();
        assert!(integer(&args, "name").is_err());
        assert!(text_list(&args, "name").is_err());
    }
}
