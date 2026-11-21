//! Conformance with `contracts/management-api/openapi.yaml`: every path, method, status and error
//! code of the document is exercised or compared. The document is read as text (no YAML parser is
//! a dependency of this crate).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use axum::http::StatusCode;
use common::*;
use serde_json::json;
use spot_mgmt_api::MgmtCode;

fn spec_text() -> String {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../contracts/management-api/openapi.yaml"
    );
    std::fs::read_to_string(path).unwrap()
}

/// `(method, path)` pairs from the `paths:` section.
fn documented_routes(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut current: Option<String> = None;
    let mut in_paths = false;
    for line in text.lines() {
        if line.starts_with("paths:") {
            in_paths = true;
            continue;
        }
        if in_paths && line.starts_with("components:") {
            break;
        }
        if !in_paths {
            continue;
        }
        if let Some(rest) = line.strip_prefix("  /mgmt/v1") {
            current = Some(format!("/mgmt/v1{}", rest.trim_end_matches(':')));
        } else if let (Some(path), Some(method)) = (
            &current,
            ["get", "post"]
                .into_iter()
                .find(|m| line == format!("    {m}:")),
        ) {
            out.push((method.to_uppercase(), path.clone()));
        }
    }
    out
}

/// `(code, status, retryable)` from the `x-spot-error-codes` registry.
fn documented_codes(text: &str) -> Vec<(String, u16, bool)> {
    let mut out = Vec::new();
    let mut in_registry = false;
    for line in text.lines() {
        if line.starts_with("x-spot-error-codes:") {
            in_registry = true;
            continue;
        }
        if !in_registry {
            continue;
        }
        let trimmed = line.trim();
        if let Some((name, rest)) = trimmed.split_once(": [") {
            if !name.starts_with("mgmt.") {
                continue;
            }
            let mut parts = rest.split(',');
            let status: u16 = parts.next().unwrap().trim().parse().unwrap();
            let retryable = parts.next().unwrap().trim() == "true";
            out.push((name.to_owned(), status, retryable));
        }
    }
    out
}

#[test]
fn the_error_registry_matches_the_document_exactly() {
    let documented = documented_codes(&spec_text());
    assert_eq!(documented.len(), MgmtCode::ALL.len());
    for (name, status, retryable) in documented {
        let code = MgmtCode::ALL
            .iter()
            .find(|code| code.as_str() == name)
            .unwrap_or_else(|| panic!("{name} is documented but not implemented"));
        assert_eq!(code.status().as_u16(), status, "{name}");
        assert_eq!(code.retryable(), retryable, "{name}");
    }
}

#[test]
fn the_document_lists_the_seven_operations() {
    let routes = documented_routes(&spec_text());
    assert_eq!(routes.len(), 7, "{routes:?}");
}

#[tokio::test]
async fn every_documented_path_and_method_answers_with_its_documented_success_status() {
    let h = Harness::new();
    // health: unauthenticated liveness.
    let (status, body) = h.send("GET", "/mgmt/v1/health", None, None, None).await;
    assert_eq!(
        (status, &body["data"]["status"]),
        (StatusCode::OK, &json!("live"))
    );
    // list and get.
    let (status, body) = h
        .send("GET", "/mgmt/v1/operations", None, None, Some(alice()))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 5);
    assert!(body["next_cursor"].is_null());
    let (status, body) = h
        .send(
            "GET",
            "/mgmt/v1/operations/site.status",
            None,
            None,
            Some(alice()),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["id"], "site.status");
    assert_eq!(body["data"]["requires"], json!([]));
    // validate returns the equivalent command line and executes nothing.
    let (status, body) = h
        .send(
            "POST",
            "/mgmt/v1/operations/site.backup.run/validate",
            Some(json!({ "params": backup_params() }).to_string()),
            None,
            Some(alice()),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body["data"]["command_line"]
            .as_str()
            .unwrap()
            .contains("backup")
    );
    assert_eq!(body["data"]["requires"], json!(["signature"]));
    // invoke, status, cancel.
    let (status, body) = h.invoke(h.signed("site.backup.run", backup_params())).await;
    assert_eq!(status, StatusCode::ACCEPTED);
    let id = body["data"]["command_id"].as_str().unwrap().to_owned();
    let done = h.wait_state(&id, "succeeded").await;
    assert!(done["data"]["created_at"].as_str().unwrap().ends_with('Z'));
    let (status, body) = h
        .send(
            "POST",
            &format!("/mgmt/v1/commands/{id}/cancel"),
            None,
            None,
            Some(alice()),
        )
        .await;
    // A finished command is unchanged: its final status, not a conflict.
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["state"], "succeeded");
}

#[tokio::test]
async fn error_envelopes_have_the_documented_shape() {
    let h = Harness::new();
    let (status, body) = h
        .send(
            "GET",
            "/mgmt/v1/operations/site.unknown",
            None,
            None,
            Some(alice()),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let error = &body["error"];
    assert_eq!(error["code"], "mgmt.unknown_operation");
    assert_eq!(error["retryable"], false);
    assert!(error["message"].is_string());
    assert!(error["request_id"].as_str().unwrap().starts_with("req-"));
}

#[tokio::test]
async fn listing_filters_and_pages() {
    let h = Harness::new();
    let (_, page) = h
        .send(
            "GET",
            "/mgmt/v1/operations?limit=2",
            None,
            None,
            Some(alice()),
        )
        .await;
    assert_eq!(page["data"].as_array().unwrap().len(), 2);
    let cursor = page["next_cursor"].as_str().unwrap().to_owned();
    let (_, rest) = h
        .send(
            "GET",
            &format!("/mgmt/v1/operations?cursor={cursor}&limit=50"),
            None,
            None,
            Some(alice()),
        )
        .await;
    assert_eq!(rest["data"].as_array().unwrap().len(), 3);
    let (_, critical) = h
        .send(
            "GET",
            "/mgmt/v1/operations?risk=critical",
            None,
            None,
            Some(alice()),
        )
        .await;
    assert_eq!(critical["data"].as_array().unwrap().len(), 2);
    let (status, body) = h
        .send(
            "GET",
            "/mgmt/v1/operations?limit=0",
            None,
            None,
            Some(alice()),
        )
        .await;
    assert_eq!(
        (status, &body["error"]["details"]["parameter"]),
        (StatusCode::UNPROCESSABLE_ENTITY, &json!("limit"))
    );
}

#[tokio::test]
async fn validate_never_echoes_a_secret() {
    let h = Harness::new();
    let params = json!({ "site": "site-1", "confirm": "TOKEN12345678" });
    let (status, body) = h
        .send(
            "POST",
            "/mgmt/v1/operations/site.restore.apply/validate",
            Some(json!({ "params": params }).to_string()),
            None,
            Some(alice()),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!body.to_string().contains("TOKEN12345678"));
    assert_eq!(body["data"]["redacted"], json!(["confirm"]));
}
