//! Mapping the engine's errors onto app errors, with the script's name and position in the
//! support detail (never in the user message).

use spot_app_api::{AppError, ErrorClass};
use spot_ts::ScriptError;

use crate::codes;

/// Whether `error` is a limit the engine enforced (so the operation fails with
/// `serverscript.limit_exceeded`).
pub fn is_limit(error: &ScriptError) -> bool {
    matches!(
        error,
        ScriptError::Timeout { .. }
            | ScriptError::MemoryLimit { .. }
            | ScriptError::OutOfBudget { .. }
            | ScriptError::StackOverflow
            | ScriptError::TooManyHostCalls { .. }
            | ScriptError::OutputTooLarge { .. }
    )
}

/// The app error a failed script run surfaces as.
///
/// A limit becomes `serverscript.limit_exceeded`; a script's own typed validation error keeps
/// its code and field errors ([`ScriptError::into_app_error`]); everything else is `Internal`
/// with its `script.*` code. The script's name, the cause and the source position (when the
/// engine knows one) ride in the support detail.
pub fn map_script_error(error: ScriptError, script: &str) -> AppError {
    let cause = error.code();
    let position = match &error {
        ScriptError::Thrown {
            position: Some(position),
            ..
        } => Some(position.clone()),
        _ => None,
    };
    if is_limit(&error) {
        let mapped = AppError::new(
            ErrorClass::Internal,
            codes::LIMIT_EXCEEDED,
            format!("The script `{script}` exceeded one of its limits."),
        );
        let mapped = mapped.detail("script", script);
        let mapped = mapped.detail("cause", cause);
        return mapped;
    }
    let mapped = error.into_app_error();
    let mapped = mapped.detail("script", script);
    let mut mapped = mapped.detail("cause", cause);
    if let Some(position) = position {
        mapped = mapped.detail("line", position.line);
        mapped = mapped.detail("column", position.column);
        if !position.source.is_empty() {
            mapped = mapped.detail("source", position.source);
        }
    }
    mapped
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_timeout_is_a_limit_exceeded() {
        let error = map_script_error(ScriptError::Timeout { millis: 200 }, "check_qty");
        assert_eq!(error.code, codes::LIMIT_EXCEEDED);
        assert_eq!(error.class, ErrorClass::Internal);
        assert_eq!(error.details["cause"], "script.timeout");
        assert_eq!(error.details["script"], "check_qty");
    }

    #[test]
    fn a_typed_validation_error_keeps_its_fields() {
        let error = map_script_error(
            ScriptError::Validation {
                code: "selling.qty_not_positive".into(),
                message: "Quantity must be positive.".into(),
                fields: vec![spot_ts::FieldProblem {
                    field: "items[2].qty".into(),
                    code: "selling.qty_not_positive".into(),
                    message: "Quantity must be positive.".into(),
                }],
            },
            "check_qty",
        );
        assert_eq!(error.class, ErrorClass::Validation);
        assert_eq!(error.fields[0].code, "selling.qty_not_positive");
        assert_eq!(error.details["script"], "check_qty");
    }
}
