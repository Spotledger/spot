//! Error conversion extension for `AppError` → `ApiError`.

use spot_app_api::AppError;
use spot_http::api::service::ApiError;

/// Extension trait to convert `AppError` into the HTTP layer's `ApiError`.
pub trait ApiErrorExt {
    /// Convert this `AppError` into an `ApiError`, preserving the class's registry code,
    /// fields, details, and explanation.
    fn into_api_error(self) -> ApiError;
}

impl ApiErrorExt for AppError {
    fn into_api_error(self) -> ApiError {
        crate::conversion::api_error(self)
    }
}
