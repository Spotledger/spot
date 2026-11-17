//! Channel `ListColumn`: `spot-api-services` through `EngineResources::list` (the real
//! `ResourceService` entry point behind `GET /resource/{doctype}` and `POST .../query`), over the
//! real document `Engine` with the `PermissionAuthorizer`, and `with_field_guard`.
//!
//! The page the caller receives (every document, every column, `next_cursor`, `total`) and every
//! refusal goes to the harness. A principal who holds `read` but not `report` is the case the
//! brief names: the document load path shows it the value, the list channel must not.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::sync::Arc;

use common::*;
use spot_api_services::EngineResources;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, RecordId, SiteId, Value};
use spot_app_api::{Document, document::ChildRow};
use spot_core::ManualClock;
use spot_document::engine::{Engine, MemoryPrincipals, PermissionAuthorizer, V7Ids};
use spot_field_guard::leakcheck::{Output, assert_no_leak, find_leaks};
use spot_field_guard::{FieldGuard, StaticProtection};
use spot_http::api::filter::{Comparison, FilterAst, FilterValue, Op, OrderKey};
use spot_http::api::registry::Code;
use spot_http::api::service::{
    ApiError, Credential, ErrorCode, Page, QuerySpec, RequestCtx, ResourceService,
};
use spot_http::request::RequestId;
use spot_seams::RegistryBuilder;
use spot_storage_contract::OrderDir;
use spot_storage_fake::FakeBackend;

struct Rig {
    resources: EngineResources<Engine<FakeBackend>>,
}

fn now() -> spot_app_api::prelude::OffsetDateTime {
    spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH
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

fn corpus_document(corpus: &spot_field_guard::leakcheck::Corpus, i: usize) -> Document {
    let value = |label: &str| corpus.get(label).unwrap().value().to_owned();
    let mut doc = Document::new(name(DOCTYPE), RecordId::new_v7());
    doc.set(name("number"), Value::Text(format!("INV-{:04}", i + 1)));
    doc.set(name("notes"), Value::Text(PLAIN_NOTE.into()));
    doc.set(name("owner"), Value::Text("seed".into()));
    doc.set(name("card_number"), Value::Text(value("card")));
    doc.set(name("holder"), Value::Text(value("lookalike")));
    doc.set(name("profile_json"), Value::Text(value("in_json")));
    doc.set(name("salary"), Value::Text(value("salary")));
    doc.set(
        name("bonus"),
        Value::Decimal(rust_decimal::Decimal::new(150_050, 2)),
    );
    doc.set(name("annual_cost"), Value::Text(value("annual")));
    let split = corpus.get("split_rows").unwrap().planted.clone();
    for (j, memo) in split.iter().enumerate() {
        let mut fields = BTreeMap::new();
        fields.insert(
            name("qty"),
            Value::Decimal(rust_decimal::Decimal::new(2000 + j as i64, 3)),
        );
        fields.insert(name("iban"), Value::Text(value("iban")));
        fields.insert(name("memo"), Value::Text(memo.clone()));
        doc.rows_mut(name("items")).push(ChildRow {
            id: RecordId::new_v7(),
            fields,
        });
    }
    doc
}

/// The engine under the permission authorizer, three invoices created as the platform, and the
/// list adapter over it with `guard` (the real one, or one that fails).
async fn rig(core: &Core, guard: FieldGuard) -> Rig {
    let ids: BTreeMap<String, RecordId> = PRINCIPALS
        .iter()
        .map(|p| ((*p).to_owned(), RecordId::new_v7()))
        .collect();
    let mut memory = MemoryPrincipals::new();
    for p in PRINCIPALS {
        memory = memory.with(&ids[p], principal(p));
    }
    let memory = Arc::new(memory);
    let clock = Arc::new(ManualClock::new(now()));
    let engine = Arc::new(
        Engine::new(
            FakeBackend::new(),
            core.meta.clone(),
            Arc::new(RegistryBuilder::new().build().unwrap()),
            clock.clone(),
            Arc::new(V7Ids),
        )
        .unwrap()
        .with_authorizer(Arc::new(PermissionAuthorizer::new(
            core.engine.clone(),
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
        engine
            .create(&seed, corpus_document(&corpus, i))
            .await
            .unwrap();
    }
    let by_token = Arc::new(ids);
    let resources = EngineResources::new(
        engine,
        core.meta.clone(),
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
    )
    .with_field_guard(Arc::new(guard), memory);
    Rig { resources }
}

fn spec() -> QuerySpec {
    QuerySpec {
        fields: Vec::new(),
        filter: None,
        order: Vec::new(),
        limit: 50,
        cursor: None,
        with_total: true,
    }
}

fn eq(field: &str, text: &str) -> FilterAst {
    FilterAst::Cmp(Comparison {
        field: name(field),
        op: Op::Eq,
        values: vec![FilterValue::Text(text.to_owned())],
        case_insensitive: false,
    })
}

fn output(result: &Result<Page, ApiError>) -> Output {
    match result {
        Ok(page) => Output::Text(format!("{page:?}")),
        Err(e) => Output::Text(format!("{e:?}")),
    }
}

fn unknown_field(e: &ApiError) -> bool {
    e.code == ErrorCode::Registry(Code::DocumentUnknownField)
}

async fn list(rig: &Rig, who: &str, spec: &QuerySpec) -> Result<Page, ApiError> {
    rig.resources.list(&ctx(who), DOCTYPE, spec).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_column_never_leaks() {
    let core = core();
    let corpus = corpus();
    let rig = rig(&core, core.guard.clone()).await;
    for who in PRINCIPALS {
        let visible = visible_labels(who, true);
        let forbidden = corpus.without(&visible);
        let mut outputs: Vec<Output> = Vec::new();

        // The everything-request.
        let all = list(&rig, who, &spec()).await;
        outputs.push(output(&all));
        if let Ok(page) = &all {
            // The positive control: a principal who may report a field sees the value, so a clean
            // result cannot be an empty one.
            let seen: Vec<String> = find_leaks(&corpus.only(&visible), &[output(&all)])
                .into_iter()
                .map(|l| l.secret)
                .collect();
            for label in &visible {
                assert!(
                    seen.iter().any(|s| s == label),
                    "{who}: `{label}` should be listed but is not ({} rows)",
                    page.documents.len()
                );
            }
            if plain_visible(who, true) {
                assert!(
                    format!("{page:?}").contains(PLAIN_NUMBER),
                    "{who}: plain column missing"
                );
            }
        }

        // Each protected column asked for alone, then used to filter and to sort: the column is
        // removed (or shown), the filter and sort answer as an unknown field.
        for (label, _, path) in SECRETS {
            let column = path.split_once('.').map_or(path, |(table, _)| table);
            let mut projected = spec();
            projected.fields = vec![name(column)];
            outputs.push(output(&list(&rig, who, &projected).await));

            if path.contains('.') {
                continue;
            }
            let secret = corpus.get(label).unwrap().value().to_owned();
            let mut filtered = spec();
            filtered.filter = Some(eq(path, &secret));
            let by_filter = list(&rig, who, &filtered).await;
            outputs.push(output(&by_filter));
            let mut sorted = spec();
            sorted.order = vec![OrderKey {
                field: name(path),
                dir: OrderDir::Asc,
                nulls: None,
            }];
            let by_sort = list(&rig, who, &sorted).await;
            outputs.push(output(&by_sort));
            if !visible.contains(&label) {
                // Same shape as a field that does not exist, whoever the caller is.
                for r in [&by_filter, &by_sort] {
                    assert!(
                        r.as_ref().is_err_and(unknown_field),
                        "{who}: a query on `{path}` must be refused as an unknown field"
                    );
                }
            }
        }
        assert_no_leak(&forbidden, &outputs);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_without_report_gets_no_protected_column() {
    // The brief's case: `read_no_report` holds `read` on pci and hr, so the document load path
    // shows it the value; the list channel needs `report`.
    let core = core();
    let corpus = corpus();
    let rig = rig(&core, core.guard.clone()).await;
    let page = list(&rig, "read_no_report", &spec()).await;
    assert_no_leak(&corpus, &[output(&page)]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_guard_error_removes_the_columns_and_never_widens() {
    // A protection source that knows no DocType: the guard errors for every row.
    let core = core();
    let corpus = corpus();
    let failing = FieldGuard::new(
        core.engine.clone(),
        Arc::new(StaticProtection::new(Vec::new())),
        core.trace.clone(),
        core.audit.clone(),
        Arc::new(ManualClock::new(now())),
    );
    let rig = rig(&core, failing).await;
    for who in ["analyst", "admin", "clerk"] {
        let page = list(&rig, who, &spec()).await;
        // Either the query check or the redaction failed closed; neither may carry a value, and
        // the plain column is gone too (the row keeps only its id).
        assert_no_leak(&corpus, &[output(&page)]);
        if let Ok(page) = &page {
            assert!(
                !format!("{page:?}").contains(PLAIN_NUMBER),
                "{who}: a guard error must empty the columns"
            );
        }
    }
}
