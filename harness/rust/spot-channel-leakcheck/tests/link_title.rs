//! Channel `LinkTitle`: `spot-api-services` through `EngineLookup::search` (the real
//! `LookupService` behind `POST /resource/{doctype}/lookup/{field}`), over the real document
//! `Engine` with the `PermissionAuthorizer`. The target DocType's title field is a protected
//! field (`holder`, group `pci`), so a leak of the title is a leak of a protected value.
//! Every page and every refusal goes to the harness.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::*;
use spot_api_services::{EngineLookup, ListFieldGuard};
use spot_app_api::Document;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::document::ChildRow;
use spot_app_api::prelude::{CommandId, RecordId, SiteId, Value};
use spot_core::ManualClock;
use spot_document::engine::{Engine, MemoryPrincipals, PermissionAuthorizer, V7Ids};
use spot_field_guard::leakcheck::{Corpus, Output, assert_no_leak, find_leaks};
use spot_field_guard::{BoundedQueue, FieldGuard, MetaProtection, StaticProtection};
use spot_http::api::lookup::{LookupPage, LookupRequest};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, Credential, LookupService, RequestCtx};
use spot_http::request::RequestId;
use spot_meta::{BuildOptions, FieldSpec, FieldType, Registry};
use spot_permissions::PermissionEngine;
use spot_seams::RegistryBuilder;
use spot_storage_fake::FakeBackend;

fn now() -> spot_app_api::prelude::OffsetDateTime {
    spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH
}

/// The shared definitions, with the invoice titled by a protected field and a `ticket` DocType
/// whose `invoice` field links to it.
fn linked_meta() -> Arc<spot_meta::EffectiveMeta> {
    let mut defs = definitions();
    defs[0].title_field = Some("holder".to_owned());
    let mut ticket = spot_meta::Definition::new("ticket");
    let mut link = FieldSpec::new("invoice", FieldType::Link);
    link.options = Some(DOCTYPE.to_owned());
    ticket.fields = vec![FieldSpec::new("subject", FieldType::Text), link];
    defs.push(ticket);
    let options = BuildOptions::new().with_derivations(&derivations());
    Registry::with_options(defs, options)
        .unwrap_or_else(|e| panic!("fixture meta: {e:?}"))
        .effective()
}

struct Rig {
    lookup: EngineLookup<Engine<FakeBackend>>,
    tokens: BTreeMap<String, RecordId>,
}

fn ctx(token: &str) -> RequestCtx {
    RequestCtx {
        site: SiteId::new("leak.test").unwrap(),
        credential: Some(Credential::new(token)),
        idempotency_key: None,
        language: None,
        request_id: RequestId::generate(),
        peer: None,
    }
}

fn invoice(corpus: &Corpus, i: usize) -> Document {
    let value = |label: &str| corpus.get(label).unwrap().value().to_owned();
    let mut doc = Document::new(name(DOCTYPE), RecordId::new_v7());
    doc.set(name("number"), Value::Text(format!("INV-{:04}", i + 1)));
    doc.set(name("owner"), Value::Text("seed".into()));
    doc.set(name("card_number"), Value::Text(value("card")));
    doc.set(name("holder"), Value::Text(value("lookalike")));
    doc.set(name("profile_json"), Value::Text(value("in_json")));
    doc.set(name("salary"), Value::Text(value("salary")));
    doc.set(name("annual_cost"), Value::Text(value("annual")));
    for (j, memo) in corpus.get("split_rows").unwrap().planted.iter().enumerate() {
        let mut fields = BTreeMap::new();
        fields.insert(name("iban"), Value::Text(value("iban")));
        fields.insert(name("memo"), Value::Text(memo.clone()));
        fields.insert(
            name("qty"),
            Value::Decimal(rust_decimal::Decimal::new(1000 + j as i64, 3)),
        );
        doc.rows_mut(name("items")).push(ChildRow {
            id: RecordId::new_v7(),
            fields,
        });
    }
    doc
}

/// `failing`: a guard whose protection source knows no DocType (every guard call errors).
async fn rig(failing: bool) -> Rig {
    let meta = linked_meta();
    let protection = Arc::new(MetaProtection::new(&meta));
    let perms = Arc::new(PermissionEngine::new(Arc::new(snapshot(
        &meta,
        &*protection,
    ))));
    let clock = Arc::new(ManualClock::new(now()));
    let trace = Arc::new(BoundedQueue::new(4096));
    let audit = Arc::new(BoundedQueue::new(4096));
    let guard = if failing {
        FieldGuard::new(
            perms.clone(),
            Arc::new(StaticProtection::new(Vec::new())),
            trace,
            audit,
            clock.clone(),
        )
    } else {
        FieldGuard::new(perms.clone(), protection, trace, audit, clock.clone())
    };
    let tokens: BTreeMap<String, RecordId> = PRINCIPALS
        .iter()
        .map(|p| ((*p).to_owned(), RecordId::new_v7()))
        .collect();
    let mut memory = MemoryPrincipals::new();
    for p in PRINCIPALS {
        memory = memory.with(&tokens[p], principal(p));
    }
    let memory = Arc::new(memory);
    let engine = Arc::new(
        Engine::new(
            FakeBackend::new(),
            meta.clone(),
            Arc::new(RegistryBuilder::new().build().unwrap()),
            clock.clone(),
            Arc::new(V7Ids),
        )
        .unwrap()
        .with_authorizer(Arc::new(PermissionAuthorizer::new(
            perms.clone(),
            memory.clone(),
        ))),
    );
    engine.prepare().await.unwrap();
    let seed = Invocation {
        site: SiteId::new("leak.test").unwrap(),
        actor: Actor::System {
            component: name("leakcheck"),
        },
        command_id: CommandId::new("seed").ok(),
        language: None,
        now: now(),
    };
    let corpus = corpus();
    for i in 0..3 {
        engine.create(&seed, invoice(&corpus, i)).await.unwrap();
    }
    let by_token = Arc::new(tokens.clone());
    let lookup = EngineLookup::new(
        engine,
        meta,
        clock,
        Arc::new(move |c: &RequestCtx| {
            let token = c
                .credential
                .as_ref()
                .map(|c| c.token.clone())
                .ok_or_else(|| ApiError::from_code(Code::AuthRequired))?;
            by_token
                .get(&token)
                .map(|id| Actor::User { id: *id, via: None })
                .ok_or_else(|| ApiError::from_code(Code::AuthRequired))
        }),
        ListFieldGuard::new(Arc::new(guard), memory),
    );
    Rig { lookup, tokens }
}

async fn search(rig: &Rig, who: &str, text: &str) -> Result<LookupPage, ApiError> {
    let _ = &rig.tokens;
    let request = LookupRequest {
        text: text.to_owned(),
        ..LookupRequest::default()
    };
    rig.lookup
        .search(&ctx(who), "ticket", "invoice", &request)
        .await
}

fn output(result: &Result<LookupPage, ApiError>) -> Output {
    match result {
        Ok(page) => Output::Text(format!("{page:?}")),
        Err(e) => Output::Text(format!("{e:?}")),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_title_never_leaks() {
    let corpus = corpus();
    let rig = rig(false).await;
    let secret = corpus.get("lookalike").unwrap().value().to_owned();
    for who in PRINCIPALS {
        // LinkTitle needs `read` / `read_masked`: the groups seen are those of a read channel.
        let visible = visible_labels(who, false);
        let forbidden = corpus.without(&visible);
        let mut outputs: Vec<Output> = Vec::new();

        // Typing nothing, and typing the protected title itself (the filter must not match on
        // a field the caller may not read).
        let browse = search(&rig, who, "").await;
        let typed = search(&rig, who, &secret).await;
        let miss = search(&rig, who, "no-such-title-zzz").await;
        outputs.extend([output(&browse), output(&typed), output(&miss)]);

        if visible.contains(&"lookalike") {
            // The positive control: the caller who may read the title sees it as the label and
            // finds the rows by typing it.
            for page in [&browse, &typed] {
                let page = page.as_ref().unwrap();
                assert!(!page.rows.is_empty(), "{who}: no candidates");
                let seen: Vec<String> = find_leaks(
                    &corpus.only(&["lookalike"]),
                    &[Output::Text(format!("{page:?}"))],
                )
                .into_iter()
                .map(|l| l.secret)
                .collect();
                assert!(
                    seen.iter().any(|s| s == "lookalike"),
                    "{who}: the title should show but does not"
                );
            }
        } else if let (Ok(typed), Ok(miss)) = (&typed, &miss) {
            // No inference: typing the protected value finds nothing, exactly as typing a
            // value that matches nothing does.
            assert!(typed.rows.is_empty(), "{who}: matched on a protected title");
            assert_eq!(typed, miss, "{who}: the answer differs from a plain miss");
        }
        assert_no_leak(&forbidden, &outputs);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guard_error_falls_back_to_the_id_label() {
    let corpus = corpus();
    let rig = rig(true).await;
    for who in ["analyst", "admin", "clerk"] {
        let page = search(&rig, who, "").await;
        assert_no_leak(&corpus, &[output(&page)]);
        let typed = search(&rig, who, corpus.get("lookalike").unwrap().value()).await;
        assert_no_leak(&corpus, &[output(&typed)]);
    }
}
