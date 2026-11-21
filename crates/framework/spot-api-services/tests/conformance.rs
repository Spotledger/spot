//! Response-schema equivalent of the public HTTP suite, scoped to the adapter's operations.
//! Every lifecycle action (rename, post, correct, a refused cancel, an unknown method) is
//! validated against the contract schema of the status the real engine returns.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use serde_json::{Value as Json, json};
use support::{App, Call};
const BASE: &str = "https://spotledger.test/native-api/adapter";

fn validate(root: &Json, path: &str, method: &str, response: &Call) {
    let op = &root["paths"][path][method];
    assert!(
        op["operationId"].is_string(),
        "missing operation {method} {path}"
    );
    let responses = &op["responses"];
    let status = response.status.to_string();
    let spec = responses
        .get(&status)
        .or_else(|| responses.get("default"))
        .unwrap();
    let response_spec = if let Some(reference) = spec["$ref"].as_str() {
        root.pointer(reference.strip_prefix('#').unwrap()).unwrap()
    } else {
        spec
    };
    let Some(schema) = response_spec.pointer("/content/application~1json/schema") else {
        assert!(response.body.is_null());
        return;
    };
    // Root definitions must remain in scope for every local reference in the response schema.
    let mut document = root.clone();
    document["adapter_response"] = schema.clone();
    let registry = jsonschema::Registry::new()
        .add(BASE, document)
        .unwrap()
        .prepare()
        .unwrap();
    let validator = jsonschema::options()
        .with_registry(&registry)
        .build(&json!({"$ref":format!("{BASE}#/adapter_response")}))
        .unwrap();
    let errors: Vec<_> = validator
        .iter_errors(&response.body)
        .map(|e| e.to_string())
        .collect();
    assert!(
        errors.is_empty(),
        "{} {method} {path}: {errors:?}\n{}",
        response.status,
        response.body
    );
}

#[tokio::test]
async fn every_resource_and_meta_operation_uses_its_contract_response_schema() {
    let root: Json = serde_norway::from_str(include_str!(
        "../../../../contracts/native-api/openapi.yaml"
    ))
    .unwrap();
    let app = App::new().await;
    let created = app.create("Schema").await;
    validate(&root, "/api/v1/resource/{doctype}", "post", &created);
    for (path, method, actual, body) in [
        (
            "/api/v1/meta/{doctype}",
            "get",
            "/api/v1/meta/note".into(),
            None,
        ),
        (
            "/api/v1/resource/{doctype}",
            "get",
            "/api/v1/resource/note".into(),
            None,
        ),
        (
            "/api/v1/resource/{doctype}/query",
            "post",
            "/api/v1/resource/note/query".into(),
            Some(json!({"with_total":true})),
        ),
        (
            "/api/v1/resource/{doctype}/{id}",
            "get",
            created.path(),
            None,
        ),
    ] {
        let r = app.call(&method.to_uppercase(), &actual, &[], body).await;
        r.status(200);
        validate(&root, path, method, &r);
    }
    let updated = app
        .call(
            "PATCH",
            &created.path(),
            &[("if-match", created.etag())],
            Some(json!({"title":"Updated"})),
        )
        .await;
    updated.status(200);
    validate(&root, "/api/v1/resource/{doctype}/{id}", "patch", &updated);
    // Each lifecycle action in the order a document can take it, with the version it needs:
    // rename and an unknown method on the draft, post, correct (cancel and amend), then a second
    // cancel of the already cancelled original, which the state gate refuses.
    let mut etag = updated.etag();
    let mut draft = created.path();
    let mut original = None;
    for (action, status) in [
        ("rename", 200),
        ("methods/probe", 404),
        ("post", 200),
        ("correct", 201),
        ("cancel", 409),
    ] {
        let body = match action {
            "rename" => Some(json!({"name":"New"})),
            "methods/probe" => Some(json!({"args":{}})),
            _ => None,
        };
        let target = original.clone().unwrap_or_else(|| draft.clone());
        let r = app
            .call(
                "POST",
                &format!("{target}/{action}"),
                &[("if-match", etag.clone())],
                body,
            )
            .await;
        r.status(status);
        let path = if action == "methods/probe" {
            "/api/v1/resource/{doctype}/{id}/methods/{method}".into()
        } else {
            format!("/api/v1/resource/{{doctype}}/{{id}}/{action}")
        };
        validate(&root, &path, "post", &r);
        if status == 200 {
            etag = r.etag();
        }
        if action == "correct" {
            // The response is the amended draft; the original stays cancelled.
            let cancelled = app.call("GET", &draft, &[], None).await;
            cancelled.status(200);
            original = Some(draft.clone());
            draft = r.path();
            etag = cancelled.etag();
        }
    }
    let fresh = app.call("GET", &draft, &[], None).await;
    let deleted = app
        .call("DELETE", &draft, &[("if-match", fresh.etag())], None)
        .await;
    deleted.status(204);
    validate(&root, "/api/v1/resource/{doctype}/{id}", "delete", &deleted);
    let missing = app.call("GET", &draft, &[], None).await;
    missing.status(404);
    validate(&root, "/api/v1/resource/{doctype}/{id}", "get", &missing);
}
