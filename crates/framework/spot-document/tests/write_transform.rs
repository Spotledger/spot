//! The write-transform seam (WP-SEC-10.T7): a transform rewrites the document inside the write's
//! transaction, after validation and before the participants and the storage write, on the fake
//! backend.
//!
//! The transform here stands in for the sealed-fields module: it turns the `name` of a customer into
//! `enc:` and the reversed text, and refuses the text `BOOM`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use common::{n, now};
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, RecordId, SiteId, Value};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Document, DocumentPatch, ErrorClass, Operation,
};
use spot_core::ManualClock;
use spot_document::engine::{
    Engine, IdentityTransform, TxParticipant, V7Ids, WriteIntent, WriteTransform, WriteView,
};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{ChangeFeed, FeedPosition, Transaction};
use spot_storage_fake::FakeBackend;

const PLAIN: &str = "PLAINTEXT";

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("write-transform.test").unwrap(),
        actor: Actor::System {
            component: n("transform_tests"),
        },
        command_id: CommandId::new("cmd-1").ok(),
        language: None,
        now: now(),
    }
}

/// `name` becomes `enc:` + the reversed text; `BOOM` is refused. Counts its calls.
struct Scramble {
    calls: Mutex<Vec<(String, Operation)>>,
}

impl WriteTransform for Scramble {
    fn transform(&self, write: &WriteView<'_>, doc: &mut Document) -> AppResult<()> {
        self.calls
            .lock()
            .unwrap()
            .push((write.doctype.to_owned(), write.operation));
        let Value::Text(text) = doc.get("name").clone() else {
            return Ok(());
        };
        if text.starts_with("enc:") {
            return Ok(());
        }
        if text == "BOOM" {
            return Err(AppError::new(
                ErrorClass::PermissionDenied,
                "test.transform_refused",
                "The field cannot be stored.",
            ));
        }
        let reversed: String = text.chars().rev().collect();
        doc.set(n("name"), format!("enc:{reversed}"));
        Ok(())
    }
}

/// What a participant's `before_write` saw in `name`.
struct Peek(Arc<Mutex<Vec<String>>>);

impl<Tx: Transaction> TxParticipant<Tx> for Peek {
    fn before_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        write: &'a WriteIntent<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if let Some(Value::Text(text)) = write.new.map(|d| d.get("name").clone()) {
                self.0.lock().unwrap().push(text);
            }
            Ok(())
        })
    }
}

async fn engine() -> Engine<FakeBackend> {
    let engine = Engine::new(
        FakeBackend::new(),
        common::meta(),
        Arc::new(RegistryBuilder::new().build().unwrap()),
        Arc::new(ManualClock::new(now())),
        Arc::new(V7Ids),
    )
    .unwrap();
    engine.prepare().await.unwrap();
    engine
}

fn customer(name: &str) -> Document {
    let mut doc = Document::new(n("customer"), RecordId::new_v7());
    doc.set(n("name"), name);
    doc
}

async fn outbox_text(engine: &Engine<FakeBackend>) -> String {
    let events = engine
        .backend()
        .read_outbox(FeedPosition::START, 10_000)
        .await
        .unwrap();
    format!("{events:?}")
}

#[tokio::test]
async fn a_written_value_is_stored_transformed_and_read_back_transformed() {
    let engine = engine().await;
    let scramble = Arc::new(Scramble {
        calls: Mutex::default(),
    });
    engine.set_write_transform(scramble.clone());
    let saved = engine.create(&inv(), customer(PLAIN)).await.unwrap().value;
    let stored = engine
        .load(&inv(), "customer", saved.id())
        .await
        .unwrap()
        .value;
    let Value::Text(text) = stored.get("name").clone() else {
        panic!("text")
    };
    assert_ne!(text, PLAIN);
    assert_eq!(text, "enc:TXETNIALP");
    // An update that carries the stored value again stores it unchanged (the transform is idempotent).
    let mut patch = DocumentPatch::default();
    patch.fields.insert(n("name"), text.clone().into());
    engine
        .update(
            &inv(),
            "customer",
            saved.id(),
            patch,
            stored.version().unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        scramble
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, op)| *op)
            .collect::<Vec<_>>(),
        [Operation::Create, Operation::Update]
    );
}

#[tokio::test]
async fn no_plaintext_reaches_the_participants_the_outbox_or_the_journal_capture() {
    let engine = engine().await;
    engine.set_write_transform(Arc::new(Scramble {
        calls: Mutex::default(),
    }));
    let seen = Arc::new(Mutex::new(Vec::new()));
    engine.set_participants(vec![
        Arc::new(Peek(seen.clone())) as Arc<dyn TxParticipant<_>>
    ]);
    engine.create(&inv(), customer(PLAIN)).await.unwrap();
    // The participant runs after the transform: it sees the stored form only.
    assert_eq!(*seen.lock().unwrap(), ["enc:TXETNIALP"]);
    assert!(!outbox_text(&engine).await.contains(PLAIN));
}

#[tokio::test]
async fn a_failed_transform_aborts_the_write_and_leaves_nothing_behind() {
    let engine = engine().await;
    engine.set_write_transform(Arc::new(Scramble {
        calls: Mutex::default(),
    }));
    let doc = customer("BOOM");
    let id = doc.id();
    let err = engine.create(&inv(), doc).await.unwrap_err();
    assert_eq!(err.code, "test.transform_refused");
    let missing = engine.load(&inv(), "customer", id).await.unwrap_err();
    assert_eq!(missing.class, ErrorClass::NotFound);
    assert!(!outbox_text(&engine).await.contains("BOOM"));
}

#[tokio::test]
async fn an_invalid_write_never_reaches_the_transform() {
    let engine = engine().await;
    let scramble = Arc::new(Scramble {
        calls: Mutex::default(),
    });
    engine.set_write_transform(scramble.clone());
    // `name` is longer than its 20 characters: validation refuses it first.
    let err = engine
        .create(&inv(), customer(&"x".repeat(40)))
        .await
        .unwrap_err();
    assert_eq!(err.class, ErrorClass::Validation);
    assert!(scramble.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_site_without_a_transform_is_unchanged_and_the_identity_changes_nothing() {
    for engine in [
        engine().await,
        engine()
            .await
            .with_write_transform(Arc::new(IdentityTransform)),
    ] {
        let saved = engine.create(&inv(), customer(PLAIN)).await.unwrap().value;
        let stored = engine
            .load(&inv(), "customer", saved.id())
            .await
            .unwrap()
            .value;
        assert_eq!(stored.get("name"), &Value::Text(PLAIN.to_owned()));
    }
}

#[tokio::test]
async fn a_delete_is_not_transformed() {
    let engine = engine().await;
    let scramble = Arc::new(Scramble {
        calls: Mutex::default(),
    });
    let saved = engine
        .create(&inv(), customer("plain"))
        .await
        .unwrap()
        .value;
    engine.set_write_transform(scramble.clone());
    engine
        .delete(&inv(), "customer", saved.id(), saved.version().unwrap())
        .await
        .unwrap();
    assert!(scramble.calls.lock().unwrap().is_empty());
}
