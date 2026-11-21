//! WP-EXT-07.T3: TypeScript scripts, list queries, `call_function`, manifest fixtures and
//! `with_permissions`, on a scaffolded app that declares a function and a fixture.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};

use serde_json::json;
use spot_app_testkit::{AppHarness, Grant, assert, codes, sync_handler};
use spot_scaffold::generate::app::{AppSpec, generate};
use spot_scaffold::names::AppName;
use spot_scaffold::tree::WritePolicy;

/// A scaffolded app (`acme`, with a script) whose manifest also declares the function
/// `pricing/quote` (POST) and a fixture file of two notes.
fn app() -> (tempfile::TempDir, PathBuf) {
    let project = tempfile::tempdir().unwrap();
    let mut spec = AppSpec::new(AppName::parse("acme").unwrap());
    spec.with_script = true;
    let tree = generate(&spec).unwrap();
    tree.write_to_disk(project.path(), &WritePolicy::new())
        .unwrap();
    let dir = project.path().join("app");
    let manifest = dir.join("manifest.toml");
    let mut text = std::fs::read_to_string(&manifest).unwrap();
    text.push_str(
        "\n[[functions]]\nmodule = \"pricing\"\nname = \"quote\"\nhttp = [\"POST\"]\n\n\
         [[fixtures]]\ndoctype = \"note\"\nfile = \"fixtures/notes.json\"\n",
    );
    std::fs::write(&manifest, text).unwrap();
    std::fs::write(
        dir.join("fixtures/notes.json"),
        r#"[{"title": "First", "status": "Open"}, {"title": "Second", "status": "Draft"}]"#,
    )
    .unwrap();
    (project, dir)
}

async fn harness(dir: &Path) -> AppHarness {
    AppHarness::builder()
        .app(dir)
        .function(
            "acme",
            "pricing",
            "quote",
            sync_handler(|ctx, args| {
                // The caller is the signed-in actor, not a guest.
                assert!(matches!(ctx.caller, spot_functions::Caller::User(_)));
                Ok(json!({ "total": args["qty"] }))
            }),
        )
        .build()
        .await
        .unwrap()
}

// ---- manifest fixtures and list queries -------------------------------------------------------

#[tokio::test]
async fn fixtures_load_and_a_list_filters_orders_and_limits() {
    let (_project, dir) = app();
    let harness = harness(&dir).await;
    assert_eq!(harness.load_fixtures().await.unwrap(), 2);
    let admin = harness.as_user("Administrator").unwrap();
    let all = admin.list("note", &json!({})).await.unwrap();
    assert_eq!(all.len(), 2);
    let open = admin
        .list("note", &json!({"where": {"status": "Open"}}))
        .await
        .unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].text("title").unwrap(), Some("First"));
    let descending = admin
        .list(
            "note",
            &json!({"order_by": "title", "desc": true, "limit": 1}),
        )
        .await
        .unwrap();
    assert_eq!(descending.len(), 1);
    assert_eq!(descending[0].text("title").unwrap(), Some("Second"));
}

#[tokio::test]
async fn a_bad_list_query_is_named() {
    let (_project, dir) = app();
    let harness = harness(&dir).await;
    let admin = harness.as_user("Administrator").unwrap();
    for query in [
        json!({"where": {"nope": 1}}),
        json!({"sort": "title"}),
        json!({"limit": "ten"}),
        json!([1]),
    ] {
        let error = admin.list("note", &query).await.unwrap_err();
        assert_eq!(error.code, codes::APP_INVALID);
    }
    assert!(admin.list("nosuch", &json!({})).await.is_err());
}

// ---- with_permissions --------------------------------------------------------------------------

#[tokio::test]
async fn a_user_with_given_permissions_gets_exactly_those() {
    let (_project, dir) = app();
    let harness = harness(&dir).await;
    harness.load_fixtures().await.unwrap();
    harness
        .user("rita")
        .with_permissions(&[Grant::on("note", &["read"])])
        .await
        .unwrap();
    let rita = harness.as_user("rita").unwrap();
    assert_eq!(rita.list("note", &json!({})).await.unwrap().len(), 2);
    // The right she was not given is refused by the engine.
    let refused = rita
        .create("note", &json!({"title": "Nope", "status": "Open"}))
        .await;
    assert::assert_denied(&refused);
}

#[tokio::test]
async fn a_user_with_no_read_right_sees_an_empty_list() {
    let (_project, dir) = app();
    let harness = harness(&dir).await;
    harness.load_fixtures().await.unwrap();
    harness
        .user("cleo")
        .with_permissions(&[Grant::on("note", &["create"])])
        .await
        .unwrap();
    let cleo = harness.as_user("cleo").unwrap();
    assert!(cleo.list("note", &json!({})).await.unwrap().is_empty());
}

#[tokio::test]
async fn permissions_can_be_replaced_and_an_unknown_doctype_changes_nothing() {
    let (_project, dir) = app();
    let harness = harness(&dir).await;
    harness
        .user("rita")
        .with_permissions(&[Grant::on("note", &["read"])])
        .await
        .unwrap();
    let refused = harness
        .user("rita")
        .with_permissions(&[Grant::on("nosuch", &["read"])])
        .await;
    assert!(refused.is_err());
    // The earlier grants still hold: read is allowed, create is not.
    let admin = harness.as_user("Administrator").unwrap();
    let note = admin
        .create("note", &json!({"title": "Mine", "status": "Open"}))
        .await;
    assert::assert_ok(&note);
    let rita = harness.as_user("rita").unwrap();
    assert::assert_ok(&rita.get("note", &note.id().unwrap()).await);
    harness
        .user("rita")
        .with_permissions(&[Grant::on("note", &["read", "create"])])
        .await
        .unwrap();
    let created = harness
        .as_user("rita")
        .unwrap()
        .create("note", &json!({"title": "Now", "status": "Open"}))
        .await;
    assert::assert_ok(&created);
}

#[tokio::test]
async fn a_grant_that_is_not_well_formed_is_refused_before_anything_is_built() {
    let (_project, dir) = app();
    let harness = harness(&dir).await;
    let refused = harness
        .user("rita")
        .with_permissions(&[Grant::on("note\"", &["read"])])
        .await;
    assert_eq!(refused.unwrap_err().code, codes::APP_INVALID);
}

// ---- call_function -----------------------------------------------------------------------------

#[tokio::test]
async fn a_declared_function_runs_through_the_registry() {
    let (_project, dir) = app();
    let harness = harness(&dir).await;
    let admin = harness.as_user("Administrator").unwrap();
    let result = admin
        .call_function("acme", "pricing", "quote", json!({"qty": 3}))
        .await;
    assert::assert_ok(&result);
    assert_eq!(result.method_result(), Some(&json!({"total": 3})));
}

#[tokio::test]
async fn an_undeclared_function_is_not_found() {
    let (_project, dir) = app();
    let harness = harness(&dir).await;
    let admin = harness.as_user("Administrator").unwrap();
    let missing = admin
        .call_function("acme", "pricing", "refund", json!({}))
        .await;
    assert::assert_error(&missing, "function.not_found");
}

#[tokio::test]
async fn a_handler_the_manifest_does_not_declare_is_refused_at_build() {
    let (_project, dir) = app();
    let error = AppHarness::builder()
        .app(&dir)
        .function(
            "acme",
            "pricing",
            "secret",
            sync_handler(|_, _| Ok(json!(null))),
        )
        .build()
        .await
        .err()
        .expect("an undeclared function is refused");
    assert_eq!(error.code, codes::APP_INVALID);
    assert!(error.problems.iter().any(|p| p.contains("pricing/secret")));
}

// ---- TypeScript scripts ------------------------------------------------------------------------

#[tokio::test]
async fn a_script_runs_on_the_real_engine() {
    let (_project, dir) = app();
    let harness = harness(&dir).await;
    let out = harness
        .run_script("app", "scripts/api.ts", "handler", &json!({"x": 1}))
        .await
        .unwrap();
    assert_eq!(
        out,
        json!({"app": "acme", "script": "api", "args": {"x": 1}})
    );
}

#[tokio::test]
async fn a_script_that_does_not_compile_or_is_not_there_says_so_by_code() {
    let (_project, dir) = app();
    std::fs::write(dir.join("scripts/broken.ts"), "export function f( {{{").unwrap();
    let harness = harness(&dir).await;
    let broken = harness
        .run_script("app", "scripts/broken.ts", "f", &json!(null))
        .await
        .unwrap_err();
    assert!(
        broken.code.starts_with("ts.") || broken.code.starts_with("script."),
        "{broken}"
    );
    assert!(
        !broken.message.contains("{{{"),
        "no source text in the message"
    );
    let missing_entry = harness
        .run_script("app", "scripts/api.ts", "nope", &json!(null))
        .await
        .unwrap_err();
    assert_eq!(missing_entry.code, "script.missing_entry");
    for path in [
        "../manifest.toml",
        "scripts/none.ts",
        "/etc/x.ts",
        "scripts/api.js",
    ] {
        let refused = harness
            .run_script("app", path, "handler", &json!(null))
            .await
            .unwrap_err();
        assert_eq!(refused.code, codes::APP_INVALID, "{path}");
    }
    let unknown_app = harness
        .run_script("other", "scripts/api.ts", "handler", &json!(null))
        .await
        .unwrap_err();
    assert_eq!(unknown_app.code, codes::APP_INVALID);
}

#[tokio::test]
async fn a_script_run_is_deterministic() {
    let (_project, dir) = app();
    std::fs::write(
        dir.join("scripts/host.ts"),
        "export function probe(): string { return String(Math.random()); }",
    )
    .unwrap();
    let harness = harness(&dir).await;
    let first = harness
        .run_script("app", "scripts/host.ts", "probe", &json!(null))
        .await
        .unwrap();
    let second = harness
        .run_script("app", "scripts/host.ts", "probe", &json!(null))
        .await
        .unwrap();
    // The random seed is zero: the same run gives the same answer.
    assert_eq!(first, second);
}
