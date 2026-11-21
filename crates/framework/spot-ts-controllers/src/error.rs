//! Mapping the engine's errors onto app errors, with the module name and the source position in
//! the support detail (never in the user message).

use spot_app_api::{AppError, ErrorClass};
use spot_ts::ScriptError;

use crate::codes;

/// Whether `error` is a limit the engine enforced, so the operation fails with
/// `tscontroller.limit_exceeded` and the transaction rolls back.
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

/// The app error a failed controller run surfaces as.
///
/// A limit becomes `tscontroller.limit_exceeded`; a handler's own typed validation error keeps its
/// code and field errors (through the engine's mapping); everything else is `Internal` with its
/// `script.*` code. The module name, the cause and the source position (when the engine knows one)
/// ride in the support detail only.
pub fn map_run_error(error: ScriptError, module: &str) -> AppError {
    let cause = error.code();
    let position = match &error {
        ScriptError::Thrown {
            position: Some(position),
            ..
        } => Some(position.clone()),
        _ => None,
    };
    if is_limit(&error) {
        return AppError::new(
            ErrorClass::Internal,
            codes::LIMIT_EXCEEDED,
            format!("The TypeScript controller `{module}` exceeded one of its limits."),
        )
        .detail("module", module)
        .detail("cause", cause);
    }
    let mut mapped = error
        .into_app_error()
        .detail("module", module)
        .detail("cause", cause);
    if let Some(position) = position {
        mapped = mapped.detail("line", position.line);
        mapped = mapped.detail("column", position.column);
        if !position.source.is_empty() {
            mapped = mapped.detail("source", position.source);
        }
    }
    mapped
}

/// The app error a module that cannot be prepared surfaces as (`tscontroller.compile_failed`),
/// with the module name in the support detail.
pub fn compile_failed(module: &str, why: &str) -> AppError {
    AppError::new(
        ErrorClass::Validation,
        codes::COMPILE_FAILED,
        format!("The TypeScript controller `{module}` did not compile."),
    )
    .detail("module", module)
    .detail("cause", why)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn a_timeout_is_a_limit_exceeded() {
        let error = map_run_error(ScriptError::Timeout { millis: 200 }, "invoice.ts");
        assert_eq!(error.code, codes::LIMIT_EXCEEDED);
        assert_eq!(error.class, ErrorClass::Internal);
        assert_eq!(error.details["cause"], "script.timeout");
        assert_eq!(error.details["module"], "invoice.ts");
    }

    #[test]
    fn a_typed_validation_error_keeps_its_fields() {
        let error = map_run_error(
            ScriptError::Validation {
                code: "selling.qty_not_positive".into(),
                message: "Quantity must be positive.".into(),
                fields: vec![spot_ts::FieldProblem {
                    field: "items[2].qty".into(),
                    code: "selling.qty_not_positive".into(),
                    message: "Quantity must be positive.".into(),
                }],
            },
            "invoice.ts",
        );
        assert_eq!(error.class, ErrorClass::Validation);
        assert_eq!(error.fields[0].code, "selling.qty_not_positive");
        assert_eq!(error.details["module"], "invoice.ts");
    }

    #[test]
    fn a_thrown_error_carries_its_position_in_the_detail() {
        let error = map_run_error(
            ScriptError::Thrown {
                name: "TypeError".into(),
                message: "boom".into(),
                stack: None,
                position: Some(spot_ts::SourcePosition {
                    source: "invoice.ts".into(),
                    line: 12,
                    column: 4,
                }),
            },
            "invoice.ts",
        );
        assert_eq!(error.code, "script.thrown");
        assert_eq!(error.details["line"], 12);
        assert_eq!(error.details["column"], 4);
    }
}
