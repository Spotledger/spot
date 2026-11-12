//! The error-code registry: one Rust table of code → (HTTP status, retryable, meaning).
//!
//! This table **must equal** the contract's `x-spot-error-codes` mapping in
//! `contracts/native-api/openapi.yaml`; `tests/registry_equality.rs` parses the contract and
//! asserts it, entry for entry. Codes are stable, dotted and lower-case; a code's HTTP status
//! never changes within a major version (ADR-055).

use axum::http::StatusCode;

/// Every platform code in the contract's `x-spot-error-codes` registry. The closed enum makes
/// the router's status/retryable decisions total; app-defined codes live in `fields` and
/// `messages` and are carried as strings (`ErrorCode::App` in `api::service`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Code {
    /// 400: the request is malformed or does not match the schema.
    ApiInvalidRequest,
    /// 400: the body is not valid JSON.
    ApiInvalidJson,
    /// 404: no endpoint at this path.
    ApiUnknownRoute,
    /// 405: the endpoint does not accept this HTTP method.
    ApiMethodNotAllowed,
    /// 415: the body is not `application/json`.
    ApiUnsupportedMediaType,
    /// 428: `If-Match` is required for this change.
    ApiPreconditionRequired,
    /// 409: the `Idempotency-Key` was used with a different body.
    ApiIdempotencyConflict,
    /// 429: too many requests; wait for `Retry-After`.
    ApiRateLimited,
    /// 404: no site is configured for this host name.
    HttpUnknownSite,
    /// 413: the body is larger than the server allows.
    HttpBodyTooLarge,
    /// 504: the request took longer than the server allows.
    HttpTimeout,
    /// 500: an unexpected server error.
    HttpInternal,
    /// 401: no credentials were sent.
    AuthRequired,
    /// 401: the credentials are wrong or no longer valid.
    AuthInvalidCredentials,
    /// 401: the session has ended.
    AuthSessionExpired,
    /// 401: the user has two-factor authentication.
    AuthTotpRequired,
    /// 401: the one-time code is wrong or was already used.
    AuthTotpInvalid,
    /// 403: the user account is disabled.
    AuthAccountDisabled,
    /// 403: the user may not do this.
    PermissionDenied,
    /// 404: no DocType with this name.
    MetaUnknownDoctype,
    /// 404: no document with this id.
    DocumentNotFound,
    /// 412: the document changed since it was read.
    DocumentStaleVersion,
    /// 422: one or more fields are invalid.
    DocumentValidationFailed,
    /// 422: a required field is missing (used in `fields`).
    DocumentRequired,
    /// 422: the DocType has no such field.
    DocumentUnknownField,
    /// 422: a value does not fit its field type.
    DocumentInvalidValue,
    /// 422: the field cannot be changed in the document's state.
    DocumentReadOnlyField,
    /// 422: a link points at a document that does not exist or may not be used.
    DocumentInvalidLink,
    /// 409: another document already has this unique value.
    DocumentDuplicate,
    /// 409: other documents link to this one.
    DocumentLinked,
    /// 409: the action is not allowed in the document's state.
    DocumentInvalidState,
    /// 404: no such room (realtime `join_error`).
    RealtimeUnknownRoom,
    /// 409: the connection is already in the maximum number of rooms (realtime `join_error`).
    RealtimeTooManyRooms,
    /// 404: no whitelisted method or function with this name.
    MethodNotFound,
    /// 404: no report with this name.
    ReportNotFound,
    /// 502: the seam chosen for a lookup failed or is missing (no fallback).
    LookupSeamFailed,
    /// 400: the changed field has no server-side form steps on the DocType.
    FormFieldUnknown,
    /// 413: the file is larger than the registry's upload limit (retryable).
    FileTooLarge,
    /// 500: the file could not be stored at this time (retryable).
    FileStoreFailed,
    /// 502: the site's `file.scan` seam failed; the upload was not stored.
    FileScanFailed,
    /// 422: the file.scan seam found malware in the upload.
    FileVirusDetected,
    /// 422: the new password fails the tenant's policy (`details.rules`).
    AuthPasswordPolicy,
    /// 404: no such session of the caller's (ended, or someone else's).
    AuthSessionNotFound,
    /// 422: A report argument is unknown or does not fit its parameter type.
    ReportBadArgument,
    /// 404: The DocType defines no print format with this name.
    PrintFormatUnknown,
    /// 500: The print format could not be rendered to a PDF.
    PrintRenderFailed,
    /// 403: The user may not report on or export a requested field.
    PermissionFieldDenied,
    /// 403: This node does not own the partition for this write.
    PartitionNotOwner,
    /// 403: The reveal was refused.
    PermissionRevealDenied,
    /// 422: The export cursor or extraction watermark is malformed, forged, expired, or belongs to another caller or request.
    BulkCursorInvalid,
    /// 410: The watermark is older than the change feed's retention or from before a resync.
    BulkWatermarkExpired,
    /// 429: Too many exports or extractions in flight, or the rows-per-second budget is used up.
    BulkLimit,
    /// 404: No such consumer, or it belongs to someone else.
    EventstreamConsumerNotFound,
    /// 409: Another connection holds this consumer's lease, or the lease sent has been replaced.
    EventstreamConsumerBusy,
    /// 410: The checkpoint or position is no longer held.
    EventstreamResyncRequired,
    /// 429: The consumer has its in-flight window of delivered, unacknowledged events.
    EventstreamWindowFull,
    /// 422: The position is malformed, forged, or issued for another consumer.
    EventstreamPositionInvalid,
    /// 409: The acknowledgement was refused: beyond what was delivered, not a delivered batch's own position, behind the checkpoint, or from a holder that lost its lease.
    EventstreamAckRefused,
    /// 422: The consumer definition is invalid (topic, filter, partition count, start) or changes what cannot change.
    EventstreamSpecInvalid,
    /// 503: the service is temporarily unavailable.
    ServerUnavailable,
    /// 503: the site is in maintenance.
    ServerMaintenance,
    /// 503: concurrent changes kept conflicting.
    ServerConflictRetryExhausted,
    /// 404: No process with this name that the caller may start.
    ProcessNotFound,
    /// 422: The start input does not fit the trigger's input schema.
    ProcessInputInvalid,
    /// 404: No run with this id that the caller may see.
    ProcessRunNotFound,
    /// 409: The run has already ended, so it cannot be cancelled.
    ProcessRunNotActive,
    /// 401: The webhook delivery was rejected.
    ProcessWebhookRejected,
    /// 404: No task with this id that the caller may act on or supervise.
    TaskNotFound,
    /// 409: Another user holds this task's claim.
    TaskAlreadyClaimed,
    /// 409: The caller does not hold the claim on this task.
    TaskNotClaimedByCaller,
    /// 422: The outcome is not one the task offers the caller.
    TaskOutcomeInvalid,
    /// 403: Segregation of duties: the caller created or already decided the document this step guards.
    TaskSegregation,
    /// 422: The delegation is not valid.
    TaskDelegationInvalid,
    /// 422: The user the task is to go to is unknown, inactive or the caller.
    TaskTargetInvalid,
    /// 409: The task is finished or cancelled, so the action is not allowed.
    TaskNotActive,
}

/// The registry table: `code → (status, retryable, meaning)`. Order is the contract's.
const TABLE: &[(Code, u16, bool, &str)] = &[
    (
        Code::ApiInvalidRequest,
        400,
        false,
        "The request is malformed or does not match the schema.",
    ),
    (
        Code::ApiInvalidJson,
        400,
        false,
        "The body is not valid JSON.",
    ),
    (
        Code::ApiUnknownRoute,
        404,
        false,
        "No endpoint at this path.",
    ),
    (
        Code::ApiMethodNotAllowed,
        405,
        false,
        "The endpoint does not accept this HTTP method.",
    ),
    (
        Code::ApiUnsupportedMediaType,
        415,
        false,
        "The body is not application/json.",
    ),
    (
        Code::ApiPreconditionRequired,
        428,
        false,
        "If-Match is required for this change.",
    ),
    (
        Code::ApiIdempotencyConflict,
        409,
        false,
        "The Idempotency-Key was used with a different body.",
    ),
    (
        Code::ApiRateLimited,
        429,
        true,
        "Too many requests; wait for Retry-After.",
    ),
    (
        Code::HttpUnknownSite,
        404,
        false,
        "No site is configured for this host name.",
    ),
    (
        Code::HttpBodyTooLarge,
        413,
        false,
        "The body is larger than the server allows.",
    ),
    (
        Code::HttpTimeout,
        504,
        true,
        "The request took longer than the server allows.",
    ),
    (
        Code::HttpInternal,
        500,
        false,
        "An unexpected server error; the request id identifies it in the logs.",
    ),
    (Code::AuthRequired, 401, false, "No credentials were sent."),
    (
        Code::AuthInvalidCredentials,
        401,
        false,
        "The credentials are wrong or no longer valid.",
    ),
    (
        Code::AuthSessionExpired,
        401,
        false,
        "The session has ended; sign in again.",
    ),
    (
        Code::AuthTotpRequired,
        401,
        false,
        "The user has two-factor authentication; repeat with a one-time code.",
    ),
    (
        Code::AuthTotpInvalid,
        401,
        false,
        "The one-time code is wrong or was already used.",
    ),
    (
        Code::AuthAccountDisabled,
        403,
        false,
        "The user account is disabled.",
    ),
    (
        Code::PermissionDenied,
        403,
        false,
        "The user may not do this.",
    ),
    (
        Code::MetaUnknownDoctype,
        404,
        false,
        "No DocType with this name.",
    ),
    (
        Code::DocumentNotFound,
        404,
        false,
        "No document with this id, or the user may not see it.",
    ),
    (
        Code::DocumentStaleVersion,
        412,
        false,
        "The document changed since it was read; details.current_version.",
    ),
    (
        Code::DocumentValidationFailed,
        422,
        false,
        "One or more fields are invalid; see fields.",
    ),
    (
        Code::DocumentRequired,
        422,
        false,
        "A required field is missing (used in fields).",
    ),
    (
        Code::DocumentUnknownField,
        422,
        false,
        "The DocType has no such field.",
    ),
    (
        Code::DocumentInvalidValue,
        422,
        false,
        "A value does not fit its field type (for example a JSON number for a decimal).",
    ),
    (
        Code::DocumentReadOnlyField,
        422,
        false,
        "The field cannot be changed in the document's state.",
    ),
    (
        Code::DocumentInvalidLink,
        422,
        false,
        "A link points at a document that does not exist or may not be used.",
    ),
    (
        Code::DocumentDuplicate,
        409,
        false,
        "Another document already has this unique value.",
    ),
    (
        Code::DocumentLinked,
        409,
        false,
        "Other documents link to this one, so it cannot be deleted.",
    ),
    (
        Code::DocumentInvalidState,
        409,
        false,
        "The action is not allowed in the document's state.",
    ),
    (
        Code::RealtimeUnknownRoom,
        404,
        false,
        "No such room (sent in a realtime join_error).",
    ),
    (
        Code::RealtimeTooManyRooms,
        409,
        false,
        "The connection is already in the maximum number of rooms (realtime join_error).",
    ),
    (
        Code::MethodNotFound,
        404,
        false,
        "No whitelisted method or function with this name.",
    ),
    (
        Code::ReportNotFound,
        404,
        false,
        "No report with this name.",
    ),
    (
        Code::LookupSeamFailed,
        502,
        false,
        "The seam chosen for the lookup failed or is missing; see explanation. There is no fallback to the default list.",
    ),
    (
        Code::FormFieldUnknown,
        400,
        false,
        "The changed field has no server-side form steps on this DocType; the client's own rules are the answer.",
    ),
    (
        Code::FileTooLarge,
        413,
        true,
        "The file is larger than the registry's upload limit; a smaller file may succeed.",
    ),
    (
        Code::FileStoreFailed,
        500,
        true,
        "The file could not be stored at this time; the same upload may succeed on a retry.",
    ),
    (
        Code::FileScanFailed,
        502,
        false,
        "The file.scan seam set for the site failed; the upload was not stored. Retry later or ask an administrator.",
    ),
    (
        Code::FileVirusDetected,
        422,
        false,
        "The file.scan seam found malware in the upload; it was kept out of storage and the attempt is recorded.",
    ),
    (
        Code::AuthPasswordPolicy,
        422,
        false,
        "The new password fails the tenant's policy; details.rules names each violated rule.",
    ),
    (
        Code::AuthSessionNotFound,
        404,
        false,
        "No such session of yours (it may have ended, or belong to someone else).",
    ),
    (
        Code::ReportBadArgument,
        422,
        false,
        "A report argument is unknown or does not fit its parameter type; the message names the parameter (never its value).",
    ),
    (
        Code::PrintFormatUnknown,
        404,
        false,
        "The DocType defines no print format with this name (a name that belongs to another DocType is the same answer).",
    ),
    (
        Code::PrintRenderFailed,
        500,
        false,
        "The print format could not be rendered to a PDF. Nothing was changed; the message and details never carry template text or document content.",
    ),
    (
        Code::PermissionFieldDenied,
        403,
        false,
        "The user may not report on or export a requested field; the message names the field path (never a value).",
    ),
    (
        Code::PartitionNotOwner,
        403,
        false,
        "This node does not own the partition for this write.",
    ),
    (
        Code::PermissionRevealDenied,
        403,
        false,
        "The reveal was refused. The same answer for a missing document, a forbidden one, a field that cannot be revealed and a caller without the right.",
    ),
    (
        Code::BulkCursorInvalid,
        422,
        false,
        "The export cursor or extraction watermark is malformed, forged, expired, or belongs to another caller or request.",
    ),
    (
        Code::BulkWatermarkExpired,
        410,
        false,
        "The watermark is older than the change feed's retention or from before a resync; run a full export and start again.",
    ),
    (
        Code::BulkLimit,
        429,
        true,
        "Too many exports or extractions in flight, or the rows-per-second budget is used up; wait for Retry-After.",
    ),
    (
        Code::EventstreamConsumerNotFound,
        404,
        false,
        "No such consumer, or it belongs to someone else.",
    ),
    (
        Code::EventstreamConsumerBusy,
        409,
        true,
        "Another connection holds this consumer's lease, or the lease sent has been replaced; retry after Retry-After.",
    ),
    (
        Code::EventstreamResyncRequired,
        410,
        false,
        "The checkpoint or position is no longer held; details.retained_from. Rebuild from a snapshot, then restart the partition.",
    ),
    (
        Code::EventstreamWindowFull,
        429,
        true,
        "The consumer has its in-flight window of delivered, unacknowledged events; acknowledge before pulling more (a pull answers 200 and carries this code in messages).",
    ),
    (
        Code::EventstreamPositionInvalid,
        422,
        false,
        "The position is malformed, forged, or issued for another consumer.",
    ),
    (
        Code::EventstreamAckRefused,
        409,
        false,
        "The acknowledgement was refused: beyond what was delivered, not a delivered batch's own position, behind the checkpoint, or from a holder that lost its lease.",
    ),
    (
        Code::EventstreamSpecInvalid,
        422,
        false,
        "The consumer definition is invalid (topic, filter, partition count, start) or changes what cannot change; see fields.",
    ),
    (
        Code::ServerUnavailable,
        503,
        true,
        "The service is temporarily unavailable; wait for Retry-After.",
    ),
    (
        Code::ServerMaintenance,
        503,
        true,
        "The site is in maintenance (for example during a migration, ADR-22).",
    ),
    (
        Code::ServerConflictRetryExhausted,
        503,
        true,
        "Concurrent changes kept conflicting; try again.",
    ),
    (
        Code::ProcessNotFound,
        404,
        false,
        "No process with this name that the caller may start (the same answer for one that does not exist).",
    ),
    (
        Code::ProcessInputInvalid,
        422,
        false,
        "The start input does not fit the trigger's input schema; fields carries paths only, never values.",
    ),
    (
        Code::ProcessRunNotFound,
        404,
        false,
        "No run with this id that the caller may see.",
    ),
    (
        Code::ProcessRunNotActive,
        409,
        false,
        "The run has already ended, so it cannot be cancelled.",
    ),
    (
        Code::ProcessWebhookRejected,
        401,
        false,
        "The webhook delivery was rejected (bad signature, unknown or disabled trigger, or a replayed delivery); the reason is never given.",
    ),
    (
        Code::TaskNotFound,
        404,
        false,
        "No task with this id that the caller may act on or supervise.",
    ),
    (
        Code::TaskAlreadyClaimed,
        409,
        false,
        "Another user holds this task's claim.",
    ),
    (
        Code::TaskNotClaimedByCaller,
        409,
        false,
        "The caller does not hold the claim on this task.",
    ),
    (
        Code::TaskOutcomeInvalid,
        422,
        false,
        "The outcome is not one the task offers the caller.",
    ),
    (
        Code::TaskSegregation,
        403,
        false,
        "Segregation of duties: the caller created or already decided the document this step guards.",
    ),
    (
        Code::TaskDelegationInvalid,
        422,
        false,
        "The delegation is not valid (for example an end time in the past).",
    ),
    (
        Code::TaskTargetInvalid,
        422,
        false,
        "The user the task is to go to is unknown, inactive or the caller (the same answer for each).",
    ),
    (
        Code::TaskNotActive,
        409,
        false,
        "The task is finished or cancelled, so the action is not allowed.",
    ),
];

impl Code {
    /// Every registry code.
    pub fn all() -> Vec<Code> {
        TABLE.iter().map(|row| row.0).collect()
    }

    /// The dotted machine-readable code.
    pub fn as_str(self) -> &'static str {
        // The table is the registry; the string form lives with the code itself.
        match self {
            Code::ApiInvalidRequest => "api.invalid_request",
            Code::ApiInvalidJson => "api.invalid_json",
            Code::ApiUnknownRoute => "api.unknown_route",
            Code::ApiMethodNotAllowed => "api.method_not_allowed",
            Code::ApiUnsupportedMediaType => "api.unsupported_media_type",
            Code::ApiPreconditionRequired => "api.precondition_required",
            Code::ApiIdempotencyConflict => "api.idempotency_conflict",
            Code::ApiRateLimited => "api.rate_limited",
            Code::HttpUnknownSite => "http.unknown_site",
            Code::HttpBodyTooLarge => "http.body_too_large",
            Code::HttpTimeout => "http.timeout",
            Code::HttpInternal => "http.internal",
            Code::AuthRequired => "auth.required",
            Code::AuthInvalidCredentials => "auth.invalid_credentials",
            Code::AuthSessionExpired => "auth.session_expired",
            Code::AuthTotpRequired => "auth.totp_required",
            Code::AuthTotpInvalid => "auth.totp_invalid",
            Code::AuthAccountDisabled => "auth.account_disabled",
            Code::PermissionDenied => "permission.denied",
            Code::MetaUnknownDoctype => "meta.unknown_doctype",
            Code::DocumentNotFound => "document.not_found",
            Code::DocumentStaleVersion => "document.stale_version",
            Code::DocumentValidationFailed => "document.validation_failed",
            Code::DocumentRequired => "document.required",
            Code::DocumentUnknownField => "document.unknown_field",
            Code::DocumentInvalidValue => "document.invalid_value",
            Code::DocumentReadOnlyField => "document.read_only_field",
            Code::DocumentInvalidLink => "document.invalid_link",
            Code::DocumentDuplicate => "document.duplicate",
            Code::DocumentLinked => "document.linked",
            Code::DocumentInvalidState => "document.invalid_state",
            Code::RealtimeUnknownRoom => "realtime.unknown_room",
            Code::RealtimeTooManyRooms => "realtime.too_many_rooms",
            Code::MethodNotFound => "method.not_found",
            Code::ReportNotFound => "report.not_found",
            Code::LookupSeamFailed => "lookup.seam_failed",
            Code::FormFieldUnknown => "form.field_unknown",
            Code::FileTooLarge => "file.too_large",
            Code::FileStoreFailed => "file.store_failed",
            Code::FileScanFailed => "file.scan_failed",
            Code::FileVirusDetected => "file.virus_detected",
            Code::AuthPasswordPolicy => "auth.password_policy",
            Code::AuthSessionNotFound => "auth.session_not_found",
            Code::ReportBadArgument => "report.bad_argument",
            Code::PrintFormatUnknown => "print.format_unknown",
            Code::PrintRenderFailed => "print.render_failed",
            Code::PermissionFieldDenied => "permission.field_denied",
            Code::PartitionNotOwner => "partition.not_owner",
            Code::PermissionRevealDenied => "permission.reveal_denied",
            Code::BulkCursorInvalid => "bulk.cursor_invalid",
            Code::BulkWatermarkExpired => "bulk.watermark_expired",
            Code::BulkLimit => "bulk.limit",
            Code::EventstreamConsumerNotFound => "eventstream.consumer_not_found",
            Code::EventstreamConsumerBusy => "eventstream.consumer_busy",
            Code::EventstreamResyncRequired => "eventstream.resync_required",
            Code::EventstreamWindowFull => "eventstream.window_full",
            Code::EventstreamPositionInvalid => "eventstream.position_invalid",
            Code::EventstreamAckRefused => "eventstream.ack_refused",
            Code::EventstreamSpecInvalid => "eventstream.spec_invalid",
            Code::ServerUnavailable => "server.unavailable",
            Code::ServerMaintenance => "server.maintenance",
            Code::ServerConflictRetryExhausted => "server.conflict_retry_exhausted",
            Code::ProcessNotFound => "process.not_found",
            Code::ProcessInputInvalid => "process.input_invalid",
            Code::ProcessRunNotFound => "process.run_not_found",
            Code::ProcessRunNotActive => "process.run_not_active",
            Code::ProcessWebhookRejected => "process.webhook_rejected",
            Code::TaskNotFound => "task.not_found",
            Code::TaskAlreadyClaimed => "task.already_claimed",
            Code::TaskNotClaimedByCaller => "task.not_claimed_by_caller",
            Code::TaskOutcomeInvalid => "task.outcome_invalid",
            Code::TaskSegregation => "task.segregation",
            Code::TaskDelegationInvalid => "task.delegation_invalid",
            Code::TaskTargetInvalid => "task.target_invalid",
            Code::TaskNotActive => "task.not_active",
        }
    }

    /// The registry's HTTP status for this code; fixed within a major version.
    pub fn status(self) -> StatusCode {
        TABLE
            .iter()
            .find(|row| row.0 == self)
            .and_then(|row| StatusCode::from_u16(row.1).ok())
            // `StatusCode::from_u16` only fails outside 100..=999; the table holds valid codes.
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    /// The raw status number, as the registry table stores it.
    pub fn status_raw(self) -> u16 {
        TABLE
            .iter()
            .find(|row| row.0 == self)
            .map_or(500, |row| row.1)
    }

    /// Whether the contract marks this error retryable.
    pub fn retryable(self) -> bool {
        TABLE
            .iter()
            .find(|row| row.0 == self)
            .is_some_and(|row| row.2)
    }

    /// The contract's plain-language meaning, and the default message when no more specific
    /// one applies.
    pub fn default_message(self) -> &'static str {
        TABLE
            .iter()
            .find(|row| row.0 == self)
            .map_or("An unexpected server error.", |row| row.3)
    }

    /// The dotted code as it appears in the contract, resolved from a string.
    pub fn from_code_str(code: &str) -> Option<Code> {
        TABLE
            .iter()
            .find(|row| row.0.as_str() == code)
            .map(|row| row.0)
    }
}

/// The whole registry as `(code, status, retryable, meaning)` rows, in contract order. The
/// equality test against `openapi.yaml` reads this.
pub fn entries() -> Vec<(&'static str, u16, bool, &'static str)> {
    TABLE
        .iter()
        .map(|row| (row.0.as_str(), row.1, row.2, row.3))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_are_unique_and_dotted() {
        let mut seen = std::collections::BTreeSet::new();
        for row in TABLE {
            assert!(
                seen.insert(row.0.as_str()),
                "duplicate code {}",
                row.0.as_str()
            );
            let parts: Vec<&str> = row.0.as_str().split('.').collect();
            assert!(parts.len() >= 2);
            for part in &parts {
                let mut b = part.bytes();
                assert!(
                    matches!(b.next(), Some(c) if c.is_ascii_lowercase())
                        && b.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_'),
                    "bad segment in {}",
                    row.0.as_str()
                );
            }
        }
    }

    #[test]
    fn from_code_str_round_trips() {
        for code in Code::all() {
            assert_eq!(Code::from_code_str(code.as_str()), Some(code));
        }
        assert_eq!(Code::from_code_str("selling.qty_not_positive"), None);
        assert_eq!(Code::from_code_str(""), None);
    }

    #[test]
    fn every_variant_is_in_the_table() {
        assert_eq!(Code::all().len(), 75, "registry entry count");
    }
}
