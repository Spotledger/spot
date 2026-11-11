//! Maps a storage failure onto the native API's error vocabulary (decision F1, 31 Oct 2026).
//!
//! When the connection pool cannot hand out a connection within its wait, or the backend cannot
//! be reached, storage reports [`ErrorKind::Unavailable`]. The contract already registers
//! `server.unavailable` (503, retryable, "wait for `Retry-After`"), so that is what the client
//! must see instead of `500 http.internal`. Every other storage error keeps today's mapping
//! (`http.internal`). The body never names a host, a pool size or a connection string: the
//! message is the registry's default and the storage explanation stays in the logs.
//!
//! The wait is carried as the `retry_after` detail, in whole seconds, the same way a throttled
//! sign-in carries it; [`retry_after_secs`] reads it back so the router can set the
//! `Retry-After` header.
//!
//! # Wiring (one line at the call site that turns a storage error into an `ApiError`)
//!
//! ```text
//! // before: ApiError::from_code(Code::HttpInternal)
//! // after:  storage_error_to_api(&error, None)
//! ```

use serde_json::Value as Json;
use spot_storage_contract::{ErrorKind, StorageError};

use super::{ApiError, Code};

/// The wait advertised for a pool wait that exceeded its limit, in seconds.
pub const POOL_WAIT_RETRY_AFTER_SECS: u64 = 1;

/// The detail key that carries the wait in seconds.
pub const RETRY_AFTER_DETAIL: &str = "retry_after";

/// Maps a storage error to the error the client sees.
///
/// `Unavailable` becomes `server.unavailable` with `retry_after` (1 second for a pool wait, or
/// `retry_after_secs` when the error carries a larger one) and `retryable = true`; every other
/// kind becomes `http.internal`, as before.
#[must_use]
pub fn storage_error_to_api(error: &StorageError, retry_after_secs: Option<u64>) -> ApiError {
    match error.kind {
        ErrorKind::Unavailable => {
            let wait = retry_after_secs
                .unwrap_or(POOL_WAIT_RETRY_AFTER_SECS)
                .max(POOL_WAIT_RETRY_AFTER_SECS);
            ApiError::from_code(Code::ServerUnavailable)
                .detail(RETRY_AFTER_DETAIL, Json::from(wait))
        }
        _ => ApiError::from_code(Code::HttpInternal),
    }
}

/// The `Retry-After` seconds an error carries, if any (for the router's header).
#[must_use]
pub fn retry_after_secs(error: &ApiError) -> Option<u64> {
    error.details.get(RETRY_AFTER_DETAIL).and_then(Json::as_u64)
}
