//! Conversions between HTTP layer types and app-api types.

use serde_json::Value as Json;
use spot_app_api::Message;
use spot_app_api::document::RowPatch;
use spot_app_api::prelude::RecordId;
use spot_app_api::{Document, DocumentPatch};
use spot_http::api::codec::{ApiDocument, ChildRowDoc, DocumentPatchApi, FieldValue};
use spot_http::api::registry::Code;
use spot_http::api::service::{
    ApiError, FieldIssue, Message as ApiMessage, MessageLevel, WriteResult,
};
use spot_http::api::storage_error::{POOL_WAIT_RETRY_AFTER_SECS, RETRY_AFTER_DETAIL};

/// Convert `AppError` to `ApiError` preserving all information.
///
/// A storage failure that retries could not absorb reaches here as an `AppError` of class
/// `Unavailable` carrying a `storage_code` detail (`spot_app_api::error::map_storage_error`).
/// It is answered by the rules of `spot_http::api::storage_error_to_api` (WP-PLT-05.T6): code
/// `server.unavailable` (or `server.conflict_retry_exhausted`, which keeps its own code),
/// retryable, with a `retry_after` detail of at least one second. No driver text, SQLSTATE,
/// host or pool name is ever added; the `storage_code` detail is the stable kind only.
pub fn api_error(err: spot_app_api::AppError) -> ApiError {
    let body = err.into_body();
    let storage_unavailable = body.class == spot_app_api::ErrorClass::Unavailable
        && body.details.contains_key("storage_code");
    let registry = if storage_unavailable {
        Code::from_code_str(&body.code)
            .filter(|c| {
                matches!(
                    c,
                    Code::ServerUnavailable | Code::ServerConflictRetryExhausted
                )
            })
            .unwrap_or(Code::ServerUnavailable)
    } else {
        Code::from_code_str(body.class.api_code()).unwrap_or(Code::HttpInternal)
    };
    let mut mapped = ApiError::new(registry, body.message);
    mapped.explanation = body.explanation;
    for field in body.fields {
        mapped.fields.push(FieldIssue::new(
            field.field.as_str().to_owned(),
            field.code,
            field.message,
        ));
    }
    for (key, value) in body.details {
        mapped.details.insert(key, value);
    }
    if storage_unavailable {
        let wait = mapped
            .details
            .get(RETRY_AFTER_DETAIL)
            .and_then(Json::as_u64)
            .unwrap_or(POOL_WAIT_RETRY_AFTER_SECS)
            .max(POOL_WAIT_RETRY_AFTER_SECS);
        mapped
            .details
            .insert(RETRY_AFTER_DETAIL.to_owned(), Json::from(wait));
    }
    mapped
}

/// Whether `code` is one that carries a wait the client must keep (`retry_after`).
pub(crate) fn carries_wait(code: Code) -> bool {
    matches!(
        code,
        Code::ServerUnavailable | Code::ServerConflictRetryExhausted
    )
}

/// Copies the `retry_after` detail of `from` onto `to` when `to`'s code carries a wait. For the
/// places that strip an error down to its registry code: the wait is not a string a step wrote.
pub(crate) fn keep_wait(to: &mut ApiError, from: &ApiError) {
    if let spot_http::api::service::ErrorCode::Registry(code) = &to.code
        && carries_wait(*code)
        && let Some(wait) = from.details.get(RETRY_AFTER_DETAIL).and_then(Json::as_u64)
    {
        to.details
            .insert(RETRY_AFTER_DETAIL.to_owned(), Json::from(wait.max(1)));
    }
}

/// Convert `DocumentPatchApi` to the app API's `DocumentPatch`.
pub fn to_patch(patch: &DocumentPatchApi) -> DocumentPatch {
    DocumentPatch {
        fields: patch.fields.clone(),
        tables: patch
            .tables
            .iter()
            .map(|(name, rows)| {
                (
                    name.clone(),
                    rows.iter()
                        .map(|row| RowPatch {
                            id: row.id,
                            fields: row.fields.clone(),
                        })
                        .collect(),
                )
            })
            .collect(),
    }
}

/// Convert app API `Message` to HTTP `ApiMessage`.
pub fn to_api_message(message: Message) -> ApiMessage {
    ApiMessage {
        level: match message.level {
            spot_app_api::Level::Info => MessageLevel::Info,
            spot_app_api::Level::Warning => MessageLevel::Warning,
        },
        code: message.code,
        text: message.text,
        field: message.field.map(|field| field.as_str().to_owned()),
    }
}

/// Convert a vector of app API messages to HTTP messages.
pub fn messages_of(messages: Vec<Message>) -> Vec<ApiMessage> {
    messages.into_iter().map(to_api_message).collect()
}

/// Convert app API `Outcome<Document>` to HTTP `WriteResult`.
pub fn to_write_result<T>(
    outcome: spot_app_api::context::Outcome<T>,
    to_doc: impl FnOnce(T) -> ApiDocument,
) -> WriteResult {
    WriteResult {
        document: to_doc(outcome.value),
        messages: messages_of(outcome.messages),
    }
}

/// Convert app API `Outcome<(Document, Json)>` to HTTP `MethodOutcome` and messages.
pub fn to_method_outcome(
    outcome: spot_app_api::context::Outcome<(Document, Json)>,
) -> (spot_http::api::service::MethodOutcome, Vec<ApiMessage>) {
    let (document, result) = outcome.value;
    (
        spot_http::api::service::MethodOutcome {
            document: to_api_document(document),
            result,
        },
        messages_of(outcome.messages),
    )
}

/// Convert app API `Document` to HTTP `ApiDocument`.
pub fn to_api_document(document: Document) -> ApiDocument {
    let mut api = ApiDocument::empty();
    api.id = Some(document.id());
    api.version = document.version();
    api.state = document.state().map(|s| s.as_str().to_owned());

    for (name, value) in document.fields() {
        api.fields
            .insert(name.clone(), FieldValue::Scalar(value.clone()));
    }

    for (name, rows) in document.tables() {
        api.fields.insert(
            name.clone(),
            FieldValue::Rows(
                rows.iter()
                    .map(|row| ChildRowDoc {
                        id: Some(row.id),
                        fields: row.fields.clone(),
                    })
                    .collect(),
            ),
        );
    }

    api
}

/// Convert HTTP `ApiDocument` to app API `Document`.
pub fn to_document(api: &ApiDocument, meta: &spot_app_api::meta::DocTypeDef) -> Document {
    let doctype_name = meta.name.clone();
    let doc_id = api.id.unwrap_or_else(RecordId::new_v7);

    let mut fields = std::collections::BTreeMap::new();
    let mut tables = std::collections::BTreeMap::new();

    for (name, value) in &api.fields {
        match value {
            FieldValue::Scalar(s) => {
                fields.insert(name.clone(), s.clone());
            }
            FieldValue::Rows(rows) => {
                let mut table_rows = Vec::new();
                for row in rows {
                    table_rows.push(spot_app_api::document::ChildRow {
                        id: row.id.unwrap_or_else(RecordId::new_v7),
                        fields: row.fields.clone(),
                    });
                }
                tables.insert(name.clone(), table_rows);
            }
        }
    }

    let mut document = Document::new(doctype_name, doc_id);
    for (name, value) in fields {
        document.set(name, value);
    }
    for (name, rows) in tables {
        *document.rows_mut(name) = rows;
    }
    document
}

#[cfg(test)]
mod tests {
    use super::*;
    use spot_app_api::error::{StorageOp, map_storage_error};
    use spot_storage_contract::{ErrorKind, StorageError};

    fn mapped(kind: ErrorKind) -> ApiError {
        api_error(map_storage_error(
            &StorageError::new(kind, "driver text must never appear"),
            StorageOp::Other,
        ))
    }

    #[test]
    fn unavailability_keeps_a_wait_and_never_becomes_internal() {
        for kind in [
            ErrorKind::Unavailable,
            ErrorKind::CommitOutcomeUnknown,
            ErrorKind::LockTimeout,
        ] {
            let error = mapped(kind);
            assert_eq!(error.code.as_str(), "server.unavailable", "{kind:?}");
            assert_eq!(error.details.get(RETRY_AFTER_DETAIL), Some(&Json::from(1)));
            assert!(!format!("{error:?}").contains("driver text"));
        }
    }

    #[test]
    fn conflict_retry_exhausted_keeps_its_code_and_gets_the_wait() {
        let error = mapped(ErrorKind::SerializationFailure);
        assert_eq!(error.code.as_str(), "server.conflict_retry_exhausted");
        assert_eq!(error.details.get(RETRY_AFTER_DETAIL), Some(&Json::from(1)));
    }

    #[test]
    fn other_storage_kinds_keep_their_mapping() {
        assert_eq!(
            mapped(ErrorKind::NotFound).code.as_str(),
            "document.not_found"
        );
        assert_eq!(
            mapped(ErrorKind::UniqueViolation).code.as_str(),
            "document.duplicate"
        );
        assert!(
            !mapped(ErrorKind::NotFound)
                .details
                .contains_key(RETRY_AFTER_DETAIL)
        );
    }

    #[test]
    fn keep_wait_survives_stripping_only_for_a_waiting_code() {
        let source = mapped(ErrorKind::Unavailable);
        let mut stripped = ApiError::from_code(Code::ServerUnavailable);
        keep_wait(&mut stripped, &source);
        assert_eq!(
            stripped.details.get(RETRY_AFTER_DETAIL),
            Some(&Json::from(1))
        );
        let mut other = ApiError::from_code(Code::HttpInternal);
        keep_wait(&mut other, &source);
        assert!(other.details.is_empty());
    }
}
