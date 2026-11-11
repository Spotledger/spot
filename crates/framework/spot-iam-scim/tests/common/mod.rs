//! Shared builders for the SCIM tests.
#![allow(dead_code, missing_docs)]

use std::collections::BTreeMap;

use serde_json::Value as Json;
use spot_iam_access::{AllowRule, KeyAction, KeyScope};
use spot_iam_directory::UserType;
use spot_iam_scim::{
    Credential, Limits, MemoryStore, Method, ScimContext, ScimRequest, ScimResponse, handle,
};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;

/// 2026-10-08 00:00:00 UTC plus `seconds`.
pub fn at(seconds: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_791_417_600 + seconds)
        .unwrap_or_else(|e| panic!("timestamp: {e}"))
}

/// A fresh record id.
pub fn id() -> RecordId {
    RecordId::new_v7()
}

/// A bearer credential owned by an enabled service user, allowing every action.
pub fn credential(tenant: &str, owner: RecordId) -> Credential {
    Credential {
        key: id(),
        owner,
        owner_type: UserType::Service,
        owner_enabled: true,
        tenant: tenant.to_owned(),
        scope: KeyScope {
            allow: vec![AllowRule::new(
                "*",
                [
                    KeyAction::Read,
                    KeyAction::Create,
                    KeyAction::Update,
                    KeyAction::Post,
                    KeyAction::Delete,
                ],
            )],
            scope: Vec::new(),
            expires_at: None,
        },
        expires_at: None,
    }
}

/// Runs one request through the core against `store`.
pub fn call(
    store: &mut MemoryStore,
    credential: &Credential,
    now: OffsetDateTime,
    limits: &Limits,
    request: ScimRequest,
) -> ScimResponse {
    let mut ctx = ScimContext {
        tenant: credential.tenant.clone(),
        credential,
        now,
        limits,
        store,
    };
    handle(request, &mut ctx)
}

/// A `POST` with a JSON body.
pub fn post(path: &str, body: Json) -> ScimRequest {
    ScimRequest::new(Method::Post, path).with_body(body)
}

/// A `GET`.
pub fn get(path: &str) -> ScimRequest {
    ScimRequest::new(Method::Get, path)
}

/// A `PATCH` with a JSON body.
pub fn patch(path: &str, body: Json) -> ScimRequest {
    ScimRequest::new(Method::Patch, path).with_body(body)
}

/// A `DELETE`.
pub fn delete(path: &str) -> ScimRequest {
    ScimRequest::new(Method::Delete, path)
}

/// Replaces every `{{name}}` placeholder in the text.
pub fn substitute(text: &str, captures: &BTreeMap<String, String>) -> String {
    let mut out = text.to_owned();
    for (name, value) in captures {
        out = out.replace(&format!("{{{{{name}}}}}"), value);
    }
    out
}

/// Replaces placeholders in every string of a JSON document.
pub fn substitute_json(value: &Json, captures: &BTreeMap<String, String>) -> Json {
    match value {
        Json::String(text) => Json::String(substitute(text, captures)),
        Json::Array(items) => {
            Json::Array(items.iter().map(|v| substitute_json(v, captures)).collect())
        }
        Json::Object(map) => Json::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), substitute_json(v, captures)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The query parameters of a transcript step.
pub fn query_of(step: &Json) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(map) = step.get("query").and_then(Json::as_object) {
        for (key, value) in map {
            if let Some(text) = value.as_str() {
                out.insert(key.clone(), text.to_owned());
            }
        }
    }
    out
}
