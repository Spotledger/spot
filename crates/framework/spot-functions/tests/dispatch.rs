//! Dispatch: every error code, the guest access matrix, and handler passthrough.
#![allow(clippy::unwrap_used)]

mod common;

use common::*;
use spot_functions::{
    Caller, Context, FunctionDecl, FunctionRegistry, FunctionRegistryBuilder, RateLimit, codes,
};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use spot_app_api::error::ErrorClass;
use spot_functions::HttpVerb;

/// Registers one host-built declaration under `app1` and builds the registry.
fn registry_with(decl: FunctionDecl) -> (FunctionRegistry, Arc<Mutex<Vec<Observed>>>) {
    let (handler, observed) = FakeHandler::new();
    let mut b = FunctionRegistryBuilder::new();
    b.register_decl("app1", decl, Arc::new(handler));
    let registry = match b.build() {
        Ok(registry) => registry,
        Err(conflicts) => panic!("registry should build: {conflicts:?}"),
    };
    (registry, observed)
}

/// The suite's plain, non-guest, limitless function declaration.
fn plain() -> FunctionDecl {
    FunctionDecl {
        key: ok_key(),
        verbs: None,
        guest: false,
        limit: None,
        arguments: None,
    }
}

#[tokio::test]
async fn unknown_function_answers_not_found() {
    let (registry, _) = registry_with(plain());
    let err = registry
        .dispatch(
            &ctx_for(user("u1"), None),
            &key("app1", "mod1", "other"),
            json_args(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::NOT_FOUND);
    assert_eq!(err.class, ErrorClass::MethodNotFound);
}

#[tokio::test]
async fn unknown_module_and_unknown_app_answer_not_found_too() {
    let (registry, _) = registry_with(plain());
    for probe in [key("app1", "other", "fn1"), key("other", "mod1", "fn1")] {
        let err = registry
            .dispatch(&ctx_for(user("u1"), None), &probe, json_args())
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::NOT_FOUND, "probe: {probe}");
    }
}

#[tokio::test]
async fn wrong_verb_answers_verb_not_allowed_with_the_allow_set() {
    let mut decl = plain();
    decl.verbs = Some(vec![HttpVerb::Get]);
    let (registry, _) = registry_with(decl);
    let err = registry
        .dispatch(
            &ctx_for(user("u1"), Some(HttpVerb::Delete)),
            &ok_key(),
            json_args(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::VERB_NOT_ALLOWED);
    assert_eq!(
        err.details.get("allow").and_then(|v| v.as_str()),
        Some("GET")
    );
}

#[tokio::test]
async fn declared_verb_is_accepted() {
    let mut decl = plain();
    decl.verbs = Some(vec![HttpVerb::Get, HttpVerb::Post]);
    let (registry, observed) = registry_with(decl);
    let reply = registry
        .dispatch(
            &ctx_for(user("u1"), Some(HttpVerb::Post)),
            &ok_key(),
            json_args(),
        )
        .await
        .unwrap();
    assert_eq!(reply, serde_json::json!({ "ok": true }));
    assert_eq!(observed.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn missing_verb_on_a_http_function_answers_verb_not_allowed() {
    // A function that answers HTTP but was called without a verb (a job, not a transport).
    let mut decl = plain();
    decl.verbs = Some(vec![HttpVerb::Get]);
    let (registry, _) = registry_with(decl);
    let err = registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::VERB_NOT_ALLOWED);
}

#[tokio::test]
async fn allow_set_lists_every_declared_verb() {
    let mut decl = plain();
    decl.verbs = Some(vec![HttpVerb::Get, HttpVerb::Post]);
    let (registry, _) = registry_with(decl);
    let err = registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
        .await
        .unwrap_err();
    assert_eq!(
        err.details.get("allow").and_then(|v| v.as_str()),
        Some("GET, POST")
    );
}

#[tokio::test]
async fn anonymous_on_non_guest_answers_not_found_never_auth_required() {
    // The function exists and even declares verbs, but existence is never revealed to a guest.
    let mut decl = plain();
    decl.verbs = Some(vec![HttpVerb::Get]);
    let (registry, _) = registry_with(decl);
    for verb in [None, Some(HttpVerb::Get), Some(HttpVerb::Delete)] {
        let err = registry
            .dispatch(&ctx_for(Caller::Anonymous, verb), &ok_key(), json_args())
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::NOT_FOUND, "verb: {verb:?}");
    }
}

#[tokio::test]
async fn guest_function_answers_anonymous_get() {
    let mut decl = plain();
    decl.guest = true;
    decl.verbs = Some(vec![HttpVerb::Get]);
    let (registry, observed) = registry_with(decl);
    let reply = registry
        .dispatch(
            &ctx_for(Caller::Anonymous, Some(HttpVerb::Get)),
            &ok_key(),
            json_args(),
        )
        .await
        .unwrap();
    assert_eq!(reply, serde_json::json!({ "ok": true }));
    assert_eq!(observed.lock().unwrap()[0].verb, Some("GET"));
}

#[tokio::test]
async fn guest_function_refuses_anonymous_write_verb_with_auth_required() {
    let mut decl = plain();
    decl.guest = true;
    decl.verbs = Some(vec![
        HttpVerb::Get,
        HttpVerb::Head,
        HttpVerb::Options,
        HttpVerb::Post,
        HttpVerb::Put,
        HttpVerb::Patch,
        HttpVerb::Delete,
    ]);
    let (registry, _) = registry_with(decl);
    // The declared verbs that change things need a signed-in caller.
    for verb in [
        HttpVerb::Post,
        HttpVerb::Put,
        HttpVerb::Patch,
        HttpVerb::Delete,
    ] {
        let err = registry
            .dispatch(
                &ctx_for(Caller::Anonymous, Some(verb)),
                &ok_key(),
                json_args(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::AUTH_REQUIRED, "verb: {verb:?}");
        assert_eq!(err.class, ErrorClass::PermissionDenied);
    }
    // And the read verbs stay open to the guest.
    for verb in [HttpVerb::Get, HttpVerb::Head, HttpVerb::Options] {
        let reply = registry
            .dispatch(
                &ctx_for(Caller::Anonymous, Some(verb)),
                &ok_key(),
                json_args(),
            )
            .await;
        assert!(reply.is_ok(), "verb: {verb:?}");
    }
}

#[tokio::test]
async fn guest_function_undeclared_verb_is_verb_not_allowed_before_auth() {
    // A guest probing a verb the function does not answer gets the verb error, not the auth
    // one: the verb set is public, authentication is about the allowed verbs.
    let mut decl = plain();
    decl.guest = true;
    decl.verbs = Some(vec![HttpVerb::Get]);
    let (registry, _) = registry_with(decl);
    for verb in [HttpVerb::Post, HttpVerb::Put, HttpVerb::Delete] {
        let err = registry
            .dispatch(
                &ctx_for(Caller::Anonymous, Some(verb)),
                &ok_key(),
                json_args(),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code, codes::VERB_NOT_ALLOWED, "verb: {verb:?}");
    }
}

#[tokio::test]
async fn guest_matrix_every_cell() {
    // The full matrix: caller {anonymous, user} × guest {true, false} × verb {GET, POST}.
    for (guest, anonymous, verb, expected) in [
        // Guest function, anonymous, read: allowed.
        (true, true, HttpVerb::Get, None),
        // Guest function, anonymous, write: auth_required.
        (true, true, HttpVerb::Post, Some(codes::AUTH_REQUIRED)),
        // Guest function, user: everything allowed.
        (true, false, HttpVerb::Get, None),
        (true, false, HttpVerb::Post, None),
        // Non-guest function, anonymous: not_found whatever the verb.
        (false, true, HttpVerb::Get, Some(codes::NOT_FOUND)),
        (false, true, HttpVerb::Post, Some(codes::NOT_FOUND)),
        // Non-guest function, user: allowed.
        (false, false, HttpVerb::Post, None),
    ] {
        let mut decl = plain();
        decl.guest = guest;
        decl.verbs = Some(vec![HttpVerb::Get, HttpVerb::Post]);
        let (registry, _) = registry_with(decl);
        let caller = if anonymous {
            Caller::Anonymous
        } else {
            user("u1")
        };
        let result = registry
            .dispatch(&ctx_for(caller, Some(verb)), &ok_key(), json_args())
            .await;
        match expected {
            None => assert!(
                result.is_ok(),
                "guest={guest} anon={anonymous} verb={verb:?}: {result:?}"
            ),
            Some(code) => {
                let err = result.unwrap_err();
                assert_eq!(
                    err.code, code,
                    "guest={guest} anon={anonymous} verb={verb:?}"
                );
            }
        }
    }
}

#[tokio::test]
async fn rate_limited_carries_retry_after() {
    let mut decl = plain();
    decl.limit = Some(RateLimit {
        permits: 1,
        per: Duration::from_secs(10),
    });
    let (registry, _) = registry_with(decl);
    let first = registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
        .await;
    assert!(first.is_ok());
    let err = registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::RATE_LIMITED);
    assert_eq!(
        err.details.get("retry_after").and_then(|v| v.as_u64()),
        Some(10_000)
    );
    assert_eq!(err.class, ErrorClass::Unavailable);
}

#[tokio::test]
async fn bad_arguments_answers_bad_arguments_with_the_schema_problem() {
    let mut decl = plain();
    decl.arguments = Some(serde_json::json!({
        "type": "object",
        "required": ["customer"],
        "properties": { "customer": { "type": "string" } }
    }));
    let (registry, _) = registry_with(decl);
    // Missing required field.
    let err = registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), serde_json::json!({}))
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::BAD_ARGUMENTS);
    assert!(err.explanation.as_deref().unwrap().contains("customer"));
    // Wrong type.
    let err = registry
        .dispatch(
            &ctx_for(user("u1"), None),
            &ok_key(),
            serde_json::json!({ "customer": 3 }),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, codes::BAD_ARGUMENTS);
    // Right shape passes to the handler.
    let reply = registry
        .dispatch(
            &ctx_for(user("u1"), None),
            &ok_key(),
            serde_json::json!({ "customer": "acme" }),
        )
        .await;
    assert!(reply.is_ok());
}

#[tokio::test]
async fn handler_errors_pass_through_untouched() {
    let failing = FailingHandler(spot_app_api::error::AppError::new(
        ErrorClass::Validation,
        "app1.qty_not_positive",
        "The quantity is not positive.",
    ));
    let mut b = FunctionRegistryBuilder::new();
    b.register_decl("app1", plain(), Arc::new(failing));
    let registry = b.build().unwrap();
    let err = registry
        .dispatch(&ctx_for(user("u1"), None), &ok_key(), json_args())
        .await
        .unwrap_err();
    assert_eq!(err.code, "app1.qty_not_positive");
    assert_eq!(err.class, ErrorClass::Validation);
}

#[tokio::test]
async fn context_reaches_the_handler_intact() {
    let mut decl = plain();
    decl.verbs = Some(vec![HttpVerb::Post]);
    let (registry, observed) = registry_with(decl);
    let ctx = Context {
        caller: user("u42"),
        verb: Some(HttpVerb::Post),
        tenant: "acme".to_owned(),
        command_id: Some("cmd-1".to_owned()),
    };
    registry
        .dispatch(&ctx, &ok_key(), serde_json::json!({ "a": 1 }))
        .await
        .unwrap();
    let seen = observed.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].tenant, "acme");
    assert_eq!(seen[0].caller_bucket, "user:u42");
    assert_eq!(seen[0].verb, Some("POST"));
    assert_eq!(seen[0].args, r#"{"a":1}"#);
}

#[tokio::test]
async fn system_caller_is_never_anonymous() {
    // A platform component is not a guest: it may call a non-guest function.
    let (registry, observed) = registry_with(plain());
    let reply = registry
        .dispatch(
            &ctx_for(Caller::System("scheduler".to_owned()), None),
            &ok_key(),
            json_args(),
        )
        .await;
    assert!(reply.is_ok());
    assert_eq!(
        observed.lock().unwrap()[0].caller_bucket,
        "system:scheduler"
    );
}
