//! The delegate behind `call_function` (`POST /api/v1/function/{app}/{module}/{function}`).

use serde_json::Value as Json;
use spot_http::api::service::{ApiError, BoxFuture, Message as ApiMessage, RequestCtx};

/// Serves `ResourceService::call_function` for an `EngineResources`.
///
/// The delegate runs as the calling user: it receives the request's own [`RequestCtx`] and
/// nothing wider. A delegate that cannot serve a call returns an [`ApiError`]; it never answers
/// success for work it did not do.
pub trait FunctionDelegate: Send + Sync {
    /// Calls `app.module.function` with `args`; `via_get` is true when the call arrived as a GET.
    fn call<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        app: &'a str,
        module: &'a str,
        function: &'a str,
        args: Json,
        via_get: bool,
    ) -> BoxFuture<'a, Result<(Json, Vec<ApiMessage>), ApiError>>;
}

/// The detail every function error carries: the stable `function.*` code the dispatcher chose.
/// The envelope's `error.code` is the contract's registry code for the status; this detail keeps
/// the finer code (`function.verb_not_allowed` and `function.not_found` are both 404/405 class
/// answers a client can tell apart).
pub const FUNCTION_CODE_DETAIL: &str = "function_code";

/// Maps a function dispatch error to the contract's error envelope.
///
/// * `function.not_found` is `404 method.not_found`;
/// * `function.verb_not_allowed` is `405 api.method_not_allowed`, the `allow` detail naming the
///   verbs the function answers (the HTTP layer renders the `Allow` header from it);
/// * `function.auth_required` is `401 auth.required`;
/// * `function.rate_limited` is `429 api.rate_limited` with `retry_after` in whole seconds
///   (rounded up, at least 1);
/// * `function.bad_arguments` is `400 api.invalid_request`, the explanation naming the schema
///   problem and never an argument value.
///
/// Anything else (a handler's own error) goes through the ordinary `AppError` mapping untouched,
/// so a handler's field errors and details reach the caller as they would from a controller.
pub fn function_error(error: spot_app_api::AppError) -> ApiError {
    use spot_http::api::registry::Code;
    let code = error.code.clone();
    let registry = match code.as_str() {
        "function.not_found" => Code::MethodNotFound,
        "function.verb_not_allowed" => Code::ApiMethodNotAllowed,
        "function.auth_required" => Code::AuthRequired,
        "function.rate_limited" => Code::ApiRateLimited,
        "function.bad_arguments" => Code::ApiInvalidRequest,
        _ => return crate::conversion::api_error(error),
    };
    let body = error.into_body();
    let mut mapped = ApiError::new(registry, body.message);
    mapped.explanation = body.explanation;
    for (key, value) in body.details {
        let value = if code == "function.rate_limited" && key == "retry_after" {
            // Milliseconds in, whole seconds out.
            let millis = value.as_u64().unwrap_or(1000);
            Json::from(millis.div_ceil(1000).max(1))
        } else {
            value
        };
        mapped.details.insert(key, value);
    }
    mapped
        .details
        .insert(FUNCTION_CODE_DETAIL.to_owned(), Json::String(code));
    mapped
}

#[cfg(test)]
mod tests {
    use super::*;
    use spot_app_api::AppError;
    use spot_app_api::error::ErrorClass;

    fn mapped(class: ErrorClass, code: &str) -> ApiError {
        function_error(AppError::new(class, code, "message"))
    }

    #[test]
    fn each_function_code_maps_to_its_registry_status_and_keeps_its_own_code() {
        use spot_http::api::registry::Code;
        for (class, code, registry, status) in [
            (ErrorClass::MethodNotFound, "function.not_found", Code::MethodNotFound, 404),
            (ErrorClass::MethodNotFound, "function.verb_not_allowed", Code::ApiMethodNotAllowed, 405),
            (ErrorClass::PermissionDenied, "function.auth_required", Code::AuthRequired, 401),
            (ErrorClass::Unavailable, "function.rate_limited", Code::ApiRateLimited, 429),
            (ErrorClass::Validation, "function.bad_arguments", Code::ApiInvalidRequest, 400),
        ] {
            let error = mapped(class, code);
            assert_eq!(error.code, spot_http::api::service::ErrorCode::Registry(registry));
            assert_eq!(error.status().as_u16(), status, "{code}");
            assert_eq!(error.details[FUNCTION_CODE_DETAIL], Json::String(code.to_owned()));
        }
    }

    #[test]
    fn the_rate_limit_wait_is_whole_seconds_rounded_up_and_the_allow_set_survives() {
        let limited = function_error(
            AppError::new(ErrorClass::Unavailable, "function.rate_limited", "slow down")
                .detail("retry_after", 1_001_u64),
        );
        assert_eq!(limited.details["retry_after"], Json::from(2));
        let instant = function_error(
            AppError::new(ErrorClass::Unavailable, "function.rate_limited", "slow down")
                .detail("retry_after", 0_u64),
        );
        assert_eq!(instant.details["retry_after"], Json::from(1));
        let verb = function_error(
            AppError::new(ErrorClass::MethodNotFound, "function.verb_not_allowed", "no")
                .detail("allow", "GET, POST"),
        );
        assert_eq!(verb.details["allow"], Json::String("GET, POST".to_owned()));
    }

    #[test]
    fn the_bad_arguments_explanation_is_kept_and_a_handlers_own_error_is_untouched() {
        let bad = function_error(
            AppError::new(ErrorClass::Validation, "function.bad_arguments", "rejected")
                .explained("n: expected integer"),
        );
        assert_eq!(bad.explanation.as_deref(), Some("n: expected integer"));
        let own = function_error(AppError::new(
            ErrorClass::PermissionDenied,
            "sample.no_right",
            "needs a right",
        ));
        assert_eq!(own.status().as_u16(), 403);
        assert!(!own.details.contains_key(FUNCTION_CODE_DETAIL));
    }
}
