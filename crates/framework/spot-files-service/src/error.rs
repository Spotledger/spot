//! The file service's error vocabulary: one variant per refusal, each with the stable dotted
//! code, HTTP status and retryability the native API's error envelope needs.
//!
//! The codes reuse the contract's existing `file.*`, `document.not_found`, `permission.denied`
//! and `api.invalid_request` entries where one fits, and add `fileservice.type_mismatch` and
//! `fileservice.quarantined` (catalogue `fileservice.toml`) for the two outcomes the contract
//! does not name yet. Nothing here carries a storage key or a provider detail, so an error can
//! never leak where the bytes live.

/// A refusal from the file service.
///
/// `code`, `status` and `retryable` are the machine-readable contract; `message` is
/// plain-language and never names a storage path, key or provider.
#[derive(Debug, thiserror::Error)]
pub enum FileServiceError {
    /// The id is unknown, the bytes are gone, or the caller may not read the file: the one
    /// answer for all three, so an id alone reveals nothing. Code `document.not_found`.
    #[error("no file with this id exists, or the caller may not see it")]
    NotFound,
    /// The caller may not upload to the destination, or is a guest where a signed-in user is
    /// required. Code `permission.denied`.
    #[error("the caller is not permitted to do this")]
    PermissionDenied,
    /// The declared content type is not allowed by the site's policy. Code
    /// `api.invalid_request`.
    #[error("the content type `{content_type}` is not allowed for upload")]
    TypeDenied {
        /// The rejected media type.
        content_type: String,
    },
    /// The declared content type does not match the bytes. Code `fileservice.type_mismatch`.
    #[error(
        "the declared content type `{declared}` does not match the bytes, which look like `{sniffed}`"
    )]
    TypeMismatch {
        /// The type the client declared.
        declared: String,
        /// The type the magic number sniffed.
        sniffed: String,
    },
    /// The streamed body passed the site's upload limit; nothing was stored. Code
    /// `file.too_large`.
    #[error("the upload exceeds the limit of {max_bytes} bytes")]
    TooLarge {
        /// The limit that was exceeded.
        max_bytes: u64,
    },
    /// The upload would pass the caller's or the site's quota; nothing was stored. Code
    /// `file.too_large`.
    #[error("the upload would exceed the {scope} quota of {limit} bytes")]
    QuotaExceeded {
        /// `site` or `user`.
        scope: &'static str,
        /// The quota in bytes.
        limit: u64,
    },
    /// The file is quarantined by the site's scan seam; downloads are refused until it is
    /// cleared. Code `fileservice.quarantined`.
    #[error("the file is quarantined and cannot be downloaded")]
    Quarantined {
        /// Plain-language reason from the scanner.
        reason: String,
    },
    /// The bytes could not be stored or the record could not be written. Code
    /// `file.store_failed`.
    #[error("the file could not be stored: {message}")]
    StoreFailed {
        /// Plain-language cause, for logs only.
        message: String,
    },
    /// The site's scan seam failed; the upload was not stored. Code `file.scan_failed`.
    #[error("the file scan failed: {message}")]
    ScanFailed {
        /// Plain-language cause, for logs only.
        message: String,
    },
    /// The scan found malware; the upload was refused and nothing was stored. Code
    /// `file.virus_detected`.
    #[error("the scan found malware in the upload")]
    VirusDetected {
        /// Plain-language cause from the scanner, for logs only.
        message: String,
    },
    /// An internal failure with no more specific code. Code `http.internal`.
    #[error("internal file service error: {message}")]
    Internal {
        /// Plain-language cause, for logs only.
        message: String,
    },
}

impl FileServiceError {
    /// The stable dotted code for the API error envelope.
    pub fn code(&self) -> &'static str {
        match self {
            FileServiceError::NotFound => "document.not_found",
            FileServiceError::PermissionDenied => "permission.denied",
            FileServiceError::TypeDenied { .. } => "api.invalid_request",
            FileServiceError::TypeMismatch { .. } => "fileservice.type_mismatch",
            FileServiceError::TooLarge { .. } | FileServiceError::QuotaExceeded { .. } => {
                "file.too_large"
            }
            FileServiceError::Quarantined { .. } => "fileservice.quarantined",
            FileServiceError::StoreFailed { .. } => "file.store_failed",
            FileServiceError::ScanFailed { .. } => "file.scan_failed",
            FileServiceError::VirusDetected { .. } => "file.virus_detected",
            FileServiceError::Internal { .. } => "http.internal",
        }
    }

    /// The HTTP status the contract registers for this code.
    pub fn status(&self) -> u16 {
        match self {
            FileServiceError::NotFound => 404,
            FileServiceError::PermissionDenied => 403,
            FileServiceError::TypeDenied { .. } => 400,
            FileServiceError::TypeMismatch { .. } => 422,
            FileServiceError::TooLarge { .. } | FileServiceError::QuotaExceeded { .. } => 413,
            FileServiceError::Quarantined { .. } => 423,
            FileServiceError::StoreFailed { .. } => 500,
            FileServiceError::ScanFailed { .. } => 502,
            FileServiceError::VirusDetected { .. } => 422,
            FileServiceError::Internal { .. } => 500,
        }
    }

    /// Whether the same request unchanged may succeed on a retry.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            FileServiceError::StoreFailed { .. }
                | FileServiceError::ScanFailed { .. }
                | FileServiceError::Internal { .. }
        )
    }

    /// Wraps a store failure as [`FileServiceError::StoreFailed`].
    pub fn store(message: impl Into<String>) -> Self {
        FileServiceError::StoreFailed {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::FileServiceError;

    #[test]
    fn codes_statuses_and_retryability_are_total() {
        let cases: [(FileServiceError, &str, u16, bool); 11] = [
            (FileServiceError::NotFound, "document.not_found", 404, false),
            (
                FileServiceError::PermissionDenied,
                "permission.denied",
                403,
                false,
            ),
            (
                FileServiceError::TypeDenied {
                    content_type: "text/html".to_owned(),
                },
                "api.invalid_request",
                400,
                false,
            ),
            (
                FileServiceError::TypeMismatch {
                    declared: "image/png".to_owned(),
                    sniffed: "application/pdf".to_owned(),
                },
                "fileservice.type_mismatch",
                422,
                false,
            ),
            (
                FileServiceError::TooLarge { max_bytes: 10 },
                "file.too_large",
                413,
                false,
            ),
            (
                FileServiceError::QuotaExceeded {
                    scope: "user",
                    limit: 10,
                },
                "file.too_large",
                413,
                false,
            ),
            (
                FileServiceError::Quarantined {
                    reason: "review".to_owned(),
                },
                "fileservice.quarantined",
                423,
                false,
            ),
            (
                FileServiceError::StoreFailed {
                    message: "disk".to_owned(),
                },
                "file.store_failed",
                500,
                true,
            ),
            (
                FileServiceError::ScanFailed {
                    message: "timeout".to_owned(),
                },
                "file.scan_failed",
                502,
                true,
            ),
            (
                FileServiceError::VirusDetected {
                    message: "eicar".to_owned(),
                },
                "file.virus_detected",
                422,
                false,
            ),
            (
                FileServiceError::Internal {
                    message: "bug".to_owned(),
                },
                "http.internal",
                500,
                true,
            ),
        ];
        for (error, code, status, retryable) in cases {
            assert_eq!(error.code(), code, "{error:?}");
            assert_eq!(error.status(), status, "{error:?}");
            assert_eq!(error.retryable(), retryable, "{error:?}");
        }
    }

    #[test]
    fn no_error_message_names_a_path_or_key() {
        let errors = [
            FileServiceError::NotFound,
            FileServiceError::StoreFailed {
                message: "connection refused".to_owned(),
            },
        ];
        for error in errors {
            let text = error.to_string();
            assert!(!text.contains('/'), "{text}");
            assert!(!text.contains("private/"), "{text}");
        }
    }
}
