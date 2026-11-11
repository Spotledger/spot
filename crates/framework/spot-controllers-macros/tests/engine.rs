//! The whitelisted methods through the real document engine on the in-memory backend.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{NoteController, n};
use serde_json::json;
use spot_app_api::context::DocumentService;
use spot_app_api::prelude::{CommandId, OffsetDateTime, RecordId, Value};
use spot_app_api::{Actor, Controller, Document, ErrorClass, Invocation};
use spot_controllers_macros::controller;
use spot_core::{SiteId, SystemClock};
use spot_document::engine::{Engine, V7Ids};
use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_seams::RegistryBuilder;
use spot_storage_fake::FakeBackend;

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("whitelist.test").expect("a valid site id"),
        actor: Actor::System {
            component: n("whitelist_tests"),
        },
        command_id: CommandId::new("cmd-1").ok(),
        language: None,
        now: OffsetDateTime::UNIX_EPOCH,
    }
}

async fn engine_with_note(title: &str) -> (Engine<FakeBackend>, RecordId) {
    let mut def = Definition::new("note");
    def.fields.push(FieldSpec::new("title", FieldType::Text));
    let meta = Registry::new(vec![def]).expect("compiles").effective();
    let controller: Arc<dyn Controller> = Arc::new(NoteController);
    let mut builder = RegistryBuilder::new();
    builder.add_controller("notes", controller.clone());
    let engine = Engine::new(
        FakeBackend::new(),
        meta,
        Arc::new(builder.build().expect("seams build")),
        Arc::new(SystemClock::new()),
        Arc::new(V7Ids),
    )
    .expect("engine builds")
    .with_controllers(vec![controller]);
    engine.prepare().await.expect("prepares");
    let mut draft = Document::new(n("note"), RecordId::new_v7());
    draft.set(n("title"), Value::Text(title.to_owned()));
    let created = engine.create(&inv(), draft).await.expect("saved").value;
    (engine, created.id())
}

async fn title(engine: &Engine<FakeBackend>, id: RecordId) -> Value {
    engine
        .peek("note", id)
        .await
        .expect("peek")
        .expect("stored")
        .get("title")
        .clone()
}

#[tokio::test]
async fn typed_arguments_round_trip_and_the_saved_document_returns_with_the_result() {
    let (engine, id) = engine_with_note("draft").await;
    let out = engine
        .call_method(
            &inv(),
            "note",
            id,
            "rename",
            json!({"title": "final", "suffix": "!"}),
            None,
        )
        .await
        .expect("the method runs")
        .value;
    let (doc, result) = out;
    assert_eq!(result, json!({"old": "draft", "new": "final!"}));
    assert_eq!(doc.get("title"), &Value::Text("final!".to_owned()));
    assert_eq!(title(&engine, id).await, Value::Text("final!".to_owned()));
}

#[tokio::test]
async fn optional_arguments_and_enums_are_typed() {
    let (engine, id) = engine_with_note("x").await;
    let (_, tally) = engine
        .call_method(
            &inv(),
            "note",
            id,
            "tally",
            json!({"numbers": [3, 9, 4], "mode": "largest"}),
            None,
        )
        .await
        .expect("runs")
        .value;
    assert_eq!(tally, json!({"value": 9, "count": 3}));
    let (_, peeked) = engine
        .call_method(&inv(), "note", id, "peek_title", json!(null), None)
        .await
        .expect("runs")
        .value;
    assert_eq!(peeked, json!("x"));
}

#[tokio::test]
async fn bad_arguments_name_the_field() {
    let (engine, id) = engine_with_note("x").await;
    // A missing field.
    let e = engine
        .call_method(&inv(), "note", id, "rename", json!({}), None)
        .await
        .expect_err("title is required");
    assert_eq!(e.class, ErrorClass::Validation);
    assert_eq!(e.fields[0].code, "whitelist.bad_arguments");
    assert_eq!(e.fields[0].field.as_str(), "title");
    // An unknown field is denied by default.
    let e = engine
        .call_method(
            &inv(),
            "note",
            id,
            "rename",
            json!({"title": "a", "colour": "red"}),
            None,
        )
        .await
        .expect_err("unknown field");
    assert_eq!(e.fields[0].field.as_str(), "colour");
    // A wrong type has no field in serde's message, so the error covers the arguments.
    let e = engine
        .call_method(
            &inv(),
            "note",
            id,
            "tally",
            json!({"numbers": "many", "mode": "sum"}),
            None,
        )
        .await
        .expect_err("wrong type");
    assert_eq!(e.code, "whitelist.bad_arguments");
    // A method without arguments refuses some.
    let e = engine
        .call_method(&inv(), "note", id, "peek_title", json!({"a": 1}), None)
        .await
        .expect_err("takes none");
    assert_eq!(e.code, "whitelist.bad_arguments");
    // Nothing changed.
    assert_eq!(title(&engine, id).await, Value::Text("x".to_owned()));
}

#[tokio::test]
async fn allow_unknown_lets_extra_fields_through() {
    let (engine, id) = engine_with_note("x").await;
    let (_, echoed) = engine
        .call_method(
            &inv(),
            "note",
            id,
            "approve",
            json!({"anything": [1, 2]}),
            None,
        )
        .await
        .expect("runs")
        .value;
    assert_eq!(echoed, json!({"anything": [1, 2]}));
}

#[tokio::test]
async fn unknown_and_non_whitelisted_methods_are_not_callable() {
    let (engine, id) = engine_with_note("x").await;
    for name in ["secret", "nope", "whitelisted_methods", "Rename", ""] {
        let e = engine
            .call_method(&inv(), "note", id, name, json!(null), None)
            .await
            .expect_err("not callable");
        assert_eq!(e.class, ErrorClass::MethodNotFound, "{name}");
        assert_eq!(e.code, "method.not_found", "{name}");
    }
}

#[tokio::test]
async fn a_panicking_method_fails_cleanly_and_rolls_back() {
    let (engine, id) = engine_with_note("clean").await;
    let e = engine
        .call_method(&inv(), "note", id, "explode", json!(null), None)
        .await
        .expect_err("the panic becomes an error");
    assert_eq!(e.code, "whitelist.failed");
    assert_eq!(e.class, ErrorClass::Internal);
    assert!(!e.message.contains("boom"), "the panic text is not leaked");
    assert_eq!(
        title(&engine, id).await,
        Value::Text("clean".to_owned()),
        "the transaction rolled back"
    );
    // The engine is still usable afterwards.
    engine
        .call_method(&inv(), "note", id, "peek_title", json!(null), None)
        .await
        .expect("still works");
}

#[test]
fn the_method_table_carries_the_mutates_flag_each_right_implies() {
    let methods = NoteController.methods();
    let by_name: std::collections::BTreeMap<_, _> = methods
        .iter()
        .map(|m| (m.name.as_str().to_owned(), m.mutates))
        .collect();
    assert!(by_name["rename"]);
    assert!(!by_name["peek_title"]);
    assert!(!by_name["tally"]);
    assert!(by_name["explode"], "the default right is write");
    assert!(by_name["approve"], "a custom permission mutates");
    assert!(!by_name.contains_key("secret"));
    assert_eq!(by_name.len(), 5);
    assert_eq!(NoteController::WHITELIST_DOCTYPE, "note");
}

/// Every right the macro knows maps to the engine's two checks: `read` is read-only, the rest
/// need `write`; `mutates = false` or `true` overrides.
#[test]
fn the_permission_matrix_of_rights() {
    struct C;
    #[controller(doctype = "note")]
    #[allow(dead_code)]
    impl C {
        #[whitelist(right = "read")]
        async fn r(
            &self,
            _ctx: &mut dyn spot_app_api::MethodContext,
        ) -> spot_app_api::AppResult<()> {
            Ok(())
        }
        #[whitelist(right = "write")]
        async fn w(
            &self,
            _ctx: &mut dyn spot_app_api::MethodContext,
        ) -> spot_app_api::AppResult<()> {
            Ok(())
        }
        #[whitelist(right = "submit")]
        async fn s(
            &self,
            _ctx: &mut dyn spot_app_api::MethodContext,
        ) -> spot_app_api::AppResult<()> {
            Ok(())
        }
        #[whitelist(right = "cancel")]
        async fn c(
            &self,
            _ctx: &mut dyn spot_app_api::MethodContext,
        ) -> spot_app_api::AppResult<()> {
            Ok(())
        }
        #[whitelist(right = "create")]
        async fn k(
            &self,
            _ctx: &mut dyn spot_app_api::MethodContext,
        ) -> spot_app_api::AppResult<()> {
            Ok(())
        }
        #[whitelist(permission = "audit_note", mutates = false)]
        async fn p(
            &self,
            _ctx: &mut dyn spot_app_api::MethodContext,
        ) -> spot_app_api::AppResult<()> {
            Ok(())
        }
        #[whitelist(right = "read", mutates = true, name = "renamed_on_the_wire")]
        async fn q(
            &self,
            _ctx: &mut dyn spot_app_api::MethodContext,
        ) -> spot_app_api::AppResult<()> {
            Ok(())
        }
    }
    let mutates: Vec<(String, bool)> = C::whitelisted_methods()
        .into_iter()
        .map(|m| (m.name.as_str().to_owned(), m.mutates))
        .collect();
    assert_eq!(
        mutates,
        vec![
            ("r".to_owned(), false),
            ("w".to_owned(), true),
            ("s".to_owned(), true),
            ("c".to_owned(), true),
            ("k".to_owned(), true),
            ("p".to_owned(), false),
            ("renamed_on_the_wire".to_owned(), true),
        ]
    );
    assert_eq!(
        C::whitelisted_rights(),
        &[
            ("r", "read"),
            ("w", "write"),
            ("s", "submit"),
            ("c", "cancel"),
            ("k", "create"),
            ("p", "audit_note"),
            ("renamed_on_the_wire", "read"),
        ]
    );
}
