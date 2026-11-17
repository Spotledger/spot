//! The admin function listing (WP-PLT-17.T7): its shape, and who may read it. The Administrator
//! sees every function with its app, verbs, guest flag, limit and argument schema; a viewer, a
//! user with only a scoped administrator assignment, a guest and a platform component get exactly
//! the answer of an unknown function.
//!
//! Not covered here (needs the wiring in the report): the HTTP round trip
//! `GET /api/v1/method/platform/functions/list`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use spot_cli::services::functions_admin::{ListHandler, administrator_check, listing};
use spot_document::engine::MemoryPrincipals;
use spot_functions::{
    Caller, Context, FunctionHandler, FunctionInfo, FunctionKey, HttpVerb, RateLimit,
};
use spot_permissions::{AssignmentContext, Principal, UserPermission};
use spot_storage_contract::RecordId;

fn infos() -> Vec<FunctionInfo> {
    vec![
        FunctionInfo {
            key: FunctionKey::from_parts("sample", "profile", "whoami"),
            app: "sample".to_owned(),
            verbs: Some(vec![HttpVerb::Get]),
            guest: false,
            limit: None,
            arguments: None,
            overridden_by: None,
        },
        FunctionInfo {
            key: FunctionKey::from_parts("sample", "profile", "greet"),
            app: "sample".to_owned(),
            verbs: Some(vec![HttpVerb::Post]),
            guest: false,
            limit: Some(RateLimit {
                permits: 20,
                per: Duration::from_secs(600),
            }),
            arguments: Some(json!({"type": "object", "required": ["name"]})),
            overridden_by: None,
        },
        FunctionInfo {
            key: FunctionKey::from_parts("sample", "public", "ping"),
            app: "sample".to_owned(),
            verbs: Some(vec![HttpVerb::Get]),
            guest: true,
            limit: None,
            arguments: None,
            overridden_by: None,
        },
    ]
}

struct Rig {
    handler: ListHandler,
    admin: RecordId,
    viewer: RecordId,
    scoped: RecordId,
}

fn rig() -> Rig {
    let (admin, viewer, scoped) = (RecordId::new_v7(), RecordId::new_v7(), RecordId::new_v7());
    let scoped_admin = AssignmentContext {
        id: "a2".to_owned(),
        roles: vec![spot_permissions::Role("Tenant Administrator".to_owned())],
        scope: vec![UserPermission {
            user: String::new(),
            doctype: None,
            field_or_link_doctype: "company".to_owned(),
            allowed: vec!["B".to_owned()],
            applies_to: spot_permissions::AppliesTo::AllDoctypes,
            include_descendants: false,
        }],
    };
    let principals = MemoryPrincipals::new()
        .with(
            &admin,
            Principal::user(
                &admin.to_string(),
                vec![AssignmentContext::unscoped("a1", &["Tenant Administrator"])],
            ),
        )
        .with(
            &viewer,
            Principal::user(
                &viewer.to_string(),
                vec![AssignmentContext::unscoped("a3", &["Viewer"])],
            ),
        )
        .with(
            &scoped,
            Principal::user(&scoped.to_string(), vec![scoped_admin]),
        );
    let handler = ListHandler::new(Arc::new(infos), administrator_check(Arc::new(principals)));
    Rig {
        handler,
        admin,
        viewer,
        scoped,
    }
}

fn ctx(caller: Caller) -> Context {
    Context::new("site", caller)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_administrator_sees_every_function_with_its_declaration() {
    let rig = rig();
    let answer = rig
        .handler
        .call(&ctx(Caller::User(rig.admin.to_string())), json!({}))
        .await
        .unwrap();
    assert_eq!(answer, listing(&infos()), "the answer matches the listing");
    let rows = answer["functions"].as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let greet = rows.iter().find(|r| r["name"] == "greet").unwrap();
    assert_eq!(greet["key"], "sample/profile/greet");
    assert_eq!(greet["app"], "sample");
    assert_eq!(greet["module"], "profile");
    assert_eq!(greet["verbs"], json!(["POST"]));
    assert_eq!(greet["guest"], false);
    assert_eq!(greet["limit"], json!({"permits": 20, "per_seconds": 600}));
    assert_eq!(greet["arguments_schema"]["required"], json!(["name"]));
    let ping = rows.iter().find(|r| r["name"] == "ping").unwrap();
    assert_eq!(ping["guest"], true);
    assert!(ping["limit"].is_null());
    // Nothing that is not a declaration: no handler, no bucket, no caller.
    let text = answer.to_string();
    for word in ["handler", "bucket", "caller"] {
        assert!(!text.contains(word), "{word} leaked: {text}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn everyone_else_gets_the_answer_of_an_unknown_function() {
    let rig = rig();
    let others = [
        Caller::User(rig.viewer.to_string()),
        Caller::User(rig.scoped.to_string()),
        Caller::User(RecordId::new_v7().to_string()),
        Caller::User("not-a-record-id".to_owned()),
        Caller::Anonymous,
        Caller::System("job".to_owned()),
    ];
    for caller in others {
        let error = rig
            .handler
            .call(&ctx(caller.clone()), json!({}))
            .await
            .unwrap_err();
        assert_eq!(
            error.code.as_str(),
            "function.not_found",
            "{caller:?} must not learn the listing exists"
        );
    }
}
