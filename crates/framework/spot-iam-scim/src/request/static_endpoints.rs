//! The static discovery endpoints and small shared request helpers.

use super::*;

// ----- Static endpoints ----------------------------------------------------------------------

pub(super) fn service_provider_config(limits: &Limits) -> Json {
    json!({
        "schemas": [crate::resource::SERVICE_PROVIDER_CONFIG_SCHEMA],
        "documentationUri": "https://spotledger.example/docs/scim",
        "patch": {"supported": true},
        "bulk": {"supported": false, "maxOperations": 0, "maxPayloadSize": 0},
        "filter": {"supported": true, "maxResults": limits.max_count},
        "changePassword": {"supported": false},
        "sort": {"supported": false},
        "etag": {"supported": true},
        "authenticationSchemes": [{
            "type": "oauthbearertoken",
            "name": "Bearer",
            "description": "A dedicated bearer credential owned by a service user",
            "specUri": "https://www.rfc-editor.org/rfc/rfc6750",
            "primary": true,
        }],
    })
}

pub(super) fn schemas() -> Vec<Json> {
    vec![
        json!({
            "id": USER_SCHEMA,
            "name": "User",
            "description": "Core user resource",
            "attributes": [
                {"name": "userName", "type": "string", "multiValued": false, "required": true, "caseExact": false, "mutability": "readWrite", "returned": "default", "uniqueness": "server"},
                {"name": "active", "type": "boolean", "multiValued": false, "required": false, "mutability": "readWrite", "returned": "default"},
            ],
        }),
        json!({
            "id": crate::resource::GROUP_SCHEMA,
            "name": "Group",
            "description": "Core group resource",
            "attributes": [
                {"name": "displayName", "type": "string", "multiValued": false, "required": true, "mutability": "readWrite", "returned": "default", "uniqueness": "server"},
                {"name": "members", "type": "complex", "multiValued": true, "required": false, "mutability": "readWrite", "returned": "default"},
            ],
        }),
    ]
}

pub(super) fn resource_types() -> Vec<Json> {
    vec![
        json!({
            "schemas": [crate::resource::RESOURCE_TYPE_SCHEMA],
            "id": "User",
            "name": "User",
            "endpoint": "/Users",
            "description": "User Account",
            "schema": USER_SCHEMA,
            "schemaExtensions": [{
                "schema": crate::resource::ENTERPRISE_USER_SCHEMA,
                "required": false,
            }],
        }),
        json!({
            "schemas": [crate::resource::RESOURCE_TYPE_SCHEMA],
            "id": "Group",
            "name": "Group",
            "endpoint": "/Groups",
            "description": "Group",
            "schema": crate::resource::GROUP_SCHEMA,
        }),
    ]
}

pub(super) fn require_body(request: &ScimRequest) -> ScimResult<Json> {
    request
        .body
        .clone()
        .ok_or_else(|| ScimError::invalid_syntax("this request needs a JSON body"))
}
