//! Reading typed arguments back out of a [`Validated`] invocation.
//!
//! The catalogue has already checked every value against its declared type; these helpers
//! only turn a value into the Rust shape a module needs, reporting a missing or
//! mis-declared parameter as an [`InvalidArgument`](crate::error::RuntimeError::InvalidArgument)
//! rather than guessing.

use spot_mgmt_ops::{ParamValue, Validated};

use crate::error::{RuntimeError, RuntimeResult};

/// The value of `name`, when the invocation carries it.
#[must_use]
pub fn value<'a>(args: &'a Validated, name: &str) -> Option<&'a ParamValue> {
    args.params
        .iter()
        .find(|param| param.name == name)
        .map(|param| &param.value)
}

/// Reads a required text parameter.
///
/// # Errors
/// [`RuntimeError::InvalidArgument`] when it is missing or not text.
pub fn text(args: &Validated, op: &str, name: &str) -> RuntimeResult<String> {
    match value(args, name) {
        Some(ParamValue::Text(text)) => Ok(text.clone()),
        _ => Err(missing(op, name, "text")),
    }
}

/// Reads an optional text parameter.
#[must_use]
pub fn text_opt(args: &Validated, name: &str) -> Option<String> {
    match value(args, name) {
        Some(ParamValue::Text(text)) => Some(text.clone()),
        _ => None,
    }
}

/// Reads a required integer parameter.
///
/// # Errors
/// [`RuntimeError::InvalidArgument`] when it is missing or not an integer.
pub fn integer(args: &Validated, op: &str, name: &str) -> RuntimeResult<i64> {
    match value(args, name) {
        Some(ParamValue::Int(value)) => Ok(*value),
        _ => Err(missing(op, name, "integer")),
    }
}

/// Reads an optional integer parameter.
#[must_use]
pub fn integer_opt(args: &Validated, name: &str) -> Option<i64> {
    match value(args, name) {
        Some(ParamValue::Int(value)) => Some(*value),
        _ => None,
    }
}

/// Reads a boolean parameter, defaulting to `false` when absent.
#[must_use]
pub fn boolean(args: &Validated, name: &str) -> bool {
    matches!(value(args, name), Some(ParamValue::Bool(true)))
}

/// Reads a duration parameter in whole seconds, defaulting to zero when absent.
#[must_use]
pub fn duration_secs(args: &Validated, name: &str) -> u64 {
    match value(args, name) {
        Some(ParamValue::Duration(value)) => *value,
        _ => 0,
    }
}

/// Reads a required list-of-text parameter.
///
/// # Errors
/// [`RuntimeError::InvalidArgument`] when it is missing or not a list of text.
pub fn list_text(args: &Validated, op: &str, name: &str) -> RuntimeResult<Vec<String>> {
    match value(args, name) {
        Some(ParamValue::List(items)) => items
            .iter()
            .map(|item| match item {
                ParamValue::Text(text) => Ok(text.clone()),
                _ => Err(missing(op, name, "list of text")),
            })
            .collect(),
        _ => Err(missing(op, name, "list of text")),
    }
}

/// The error for a parameter that is absent or has the wrong shape.
fn missing(op: &str, name: &str, wanted: &str) -> RuntimeError {
    RuntimeError::InvalidArgument {
        op: op.to_owned(),
        param: name.to_owned(),
        detail: format!("expected {wanted}"),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use spot_mgmt_ops::{AcceptedParam, ParamType, Validated};

    use super::*;
    use crate::ids;

    fn validated(params: Vec<AcceptedParam>) -> Validated {
        Validated {
            op: ids::op_id(ids::DB_PARAM_GET).unwrap(),
            params,
        }
    }

    fn param(name: &str, ty: ParamType, value: ParamValue) -> AcceptedParam {
        AcceptedParam {
            name: name.to_owned(),
            ty,
            value,
            from_default: false,
            secret: false,
        }
    }

    #[test]
    fn readers_return_the_declared_shape() {
        let args = validated(vec![
            param(
                "name",
                ParamType::Text {
                    max_len: 32,
                    pattern: None,
                },
                ParamValue::Text("x".to_owned()),
            ),
            param(
                "count",
                ParamType::Int { min: 0, max: 9 },
                ParamValue::Int(3),
            ),
            param("flag", ParamType::Bool, ParamValue::Bool(true)),
            param("wait", ParamType::Duration, ParamValue::Duration(30)),
            param(
                "items",
                ParamType::List(Box::new(ParamType::Text {
                    max_len: 8,
                    pattern: None,
                })),
                ParamValue::List(vec![ParamValue::Text("a".to_owned())]),
            ),
        ]);
        assert_eq!(text(&args, "op", "name").unwrap(), "x");
        assert_eq!(integer(&args, "op", "count").unwrap(), 3);
        assert!(boolean(&args, "flag"));
        assert_eq!(duration_secs(&args, "wait"), 30);
        assert_eq!(list_text(&args, "op", "items").unwrap(), vec!["a"]);
        assert!(text_opt(&args, "name").is_some());
    }

    #[test]
    fn a_wrong_shape_is_an_invalid_argument() {
        let args = validated(Vec::new());
        let error = text(&args, "op", "name").unwrap_err();
        assert_eq!(error.code(), crate::error::INVALID_ARGUMENT);
        assert_eq!(
            integer(&args, "op", "count").unwrap_err().code(),
            crate::error::INVALID_ARGUMENT
        );
    }
}
