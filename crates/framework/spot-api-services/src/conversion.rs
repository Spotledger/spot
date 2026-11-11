//! Conversions between HTTP layer types and app-api types.

use serde_json::Value as Json;
use spot_app_api::Message;
use spot_app_api::document::RowPatch;
use spot_app_api::prelude::RecordId;
use spot_app_api::{Document, DocumentPatch};
use spot_http::api::codec::{ApiDocument, ChildRowDoc, DocumentPatchApi, FieldValue};
use spot_http::api::service::{
    ApiError, FieldIssue, Message as ApiMessage, MessageLevel, WriteResult,
};

/// Convert `AppError` to `ApiError` preserving all information.
pub fn api_error(err: spot_app_api::AppError) -> ApiError {
    let body = err.into_body();
    let registry = spot_http::api::registry::Code::from_code_str(body.class.api_code())
        .unwrap_or(spot_http::api::registry::Code::HttpInternal);
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
    mapped
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
