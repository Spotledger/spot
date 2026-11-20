//! where_used at scale: two small queries per edge, a bounded sample, the same answers.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::Arc;

use common::{Harness, name, row, text};
use spot_app_api::error::StorageOp;
use spot_app_api::prelude::{Expr, Query, RecordId, Rows, Value, Version};
use spot_app_api::{AppResult, BoxFuture, DocState, Document};
use spot_document::mapping;
use spot_links::{LinkChecks, ReferrerGroup, WhereUsed, where_used};
use spot_meta::EffectiveMeta;
use spot_storage_contract::{
    Catalog, DocumentStore, Isolation, QueryExecutor, StorageBackend, Transaction, TxOptions,
};
use spot_storage_fake::FakeBackend;

/// A `LinkChecks` over the shared store that records every query and the rows it returned. With
/// `visible` set it narrows every top-level edge to those ids, as a scoped repository would.
struct Counting {
    meta: Arc<EffectiveMeta>,
    catalog: Arc<Catalog>,
    backend: FakeBackend,
    queries: Vec<Query>,
    sample_rows: usize,
    visible: Option<Vec<Value>>,
}

impl Counting {
    fn new(h: &Harness) -> Self {
        Self {
            meta: h.meta.clone(),
            catalog: h.catalog.clone(),
            backend: h.backend.clone(),
            queries: Vec::new(),
            sample_rows: 0,
            visible: None,
        }
    }

    fn aggregates(&self) -> usize {
        self.queries
            .iter()
            .filter(|q| matches!(q.select.first().map(|s| &s.expr), Some(Expr::Agg { .. })))
            .count()
    }

    fn samples(&self) -> Vec<&Query> {
        self.queries.iter().filter(|q| q.limit.is_some()).collect()
    }
}

impl LinkChecks for Counting {
    fn effective_meta(&self) -> Arc<EffectiveMeta> {
        self.meta.clone()
    }

    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        Box::pin(async move {
            let mut query = query.clone();
            if let Some(visible) = &self.visible {
                let id = Expr::field(&name("referrer"), &name("id"));
                let narrow = Expr::In(Box::new(id), visible.clone());
                query.filter = Some(match query.filter.take() {
                    Some(existing) => Expr::And(vec![existing, narrow]),
                    None => narrow,
                });
            }
            self.queries.push(query.clone());
            let options = TxOptions::new(name("links_scale"), Isolation::ReadCommitted).read_only();
            let map = |e: &spot_storage_contract::StorageError| {
                spot_app_api::error::map_storage_error(e, StorageOp::Other)
            };
            let mut tx = self
                .backend
                .begin(self.catalog.clone(), options)
                .await
                .map_err(|e| map(&e))?;
            let rows = tx.query(&query).await;
            let _ = tx.rollback().await;
            let rows = rows.map_err(|e| map(&e))?;
            if query.limit.is_some() {
                self.sample_rows += rows.rows.len();
            }
            Ok(rows)
        })
    }
}

/// Inserts documents straight into storage, many per transaction, around the engine.
async fn seed(h: &Harness, docs: Vec<Document>) {
    for chunk in docs.chunks(2_000) {
        let options = TxOptions::new(name("links_seed"), Isolation::ReadCommitted);
        let mut tx = h.backend.begin(h.catalog.clone(), options).await.unwrap();
        for doc in chunk {
            let mut doc = doc.clone();
            doc.mark_saved(Version::FIRST, Some(DocState::Draft));
            let data = mapping::to_storage(&h.meta, &doc).unwrap();
            tx.insert(&data).await.unwrap();
        }
        tx.commit().await.unwrap();
    }
}

fn doc(doctype: &str, fields: &[(&str, Value)]) -> Document {
    let mut doc = Document::new(name(doctype), RecordId::new_v7());
    for (field, value) in fields {
        doc.set(name(field), value.clone());
    }
    doc
}

fn group<'a>(used: &'a WhereUsed, doctype: &str, field: &str) -> Option<&'a ReferrerGroup> {
    used.groups
        .iter()
        .find(|g| g.doctype == doctype && g.field == field)
}

fn smallest(ids: &[RecordId], n: usize) -> Vec<RecordId> {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.truncate(n);
    ids
}

#[tokio::test]
async fn twenty_thousand_referrers_cost_two_small_queries_per_edge() {
    let h = Harness::new().await;
    let customer = h.create("customer", &[("name", text("Big"))]).await;
    let target = Value::Id(customer.id());

    let mut docs = Vec::new();
    let mut by_link = Vec::new();
    for i in 0..8_000 {
        let d = doc(
            "invoice",
            &[
                ("number", Value::Text(format!("L-{i}"))),
                ("customer", target.clone()),
            ],
        );
        by_link.push(d.id());
        docs.push(d);
    }
    let mut by_note = Vec::new();
    for _ in 0..6_000 {
        let d = doc(
            "note",
            &[("about_type", text("customer")), ("about", target.clone())],
        );
        by_note.push(d.id());
        docs.push(d);
    }
    // 2,000 invoices whose three rows all refer to the customer: 6,000 rows, 2,000 documents.
    let mut by_child = Vec::new();
    for i in 0..2_000 {
        let mut d = doc("invoice", &[("number", Value::Text(format!("C-{i}")))]);
        for k in 0..3 {
            d.rows_mut(name("lines")).push(row(&[
                ("item", Value::Text(format!("item {k}"))),
                ("billing_customer", target.clone()),
            ]));
        }
        by_child.push(d.id());
        docs.push(d);
    }
    seed(&h, docs).await;

    let mut checks = Counting::new(&h);
    let used = where_used(&mut checks, "customer", customer.id(), 50)
        .await
        .unwrap();

    assert_eq!(used.total, 16_000);
    let g = group(&used, "invoice", "customer").unwrap();
    assert_eq!(g.count, 8_000);
    assert_eq!(g.sample, smallest(&by_link, 50));
    let g = group(&used, "note", "about").unwrap();
    assert_eq!(g.count, 6_000);
    assert_eq!(g.sample, smallest(&by_note, 50));
    let g = group(&used, "invoice", "billing_customer").unwrap();
    assert_eq!(g.count, 2_000, "three rows of one parent count once");
    assert_eq!(g.sample, smallest(&by_child, 50));

    // Two queries per edge that has referrers: one aggregate, one bounded sample.
    let samples = checks.samples();
    assert_eq!(samples.len(), 3);
    assert!(samples.iter().all(|q| q.limit == Some(50)));
    assert!(samples.iter().all(|q| q.offset == 0));
    assert!(checks.aggregates() >= 3);
    assert_eq!(checks.queries.len(), checks.aggregates() + samples.len());
    assert!(
        checks.sample_rows <= 3 * 50,
        "{} sample rows came back",
        checks.sample_rows
    );
}

#[tokio::test]
async fn a_scoped_repository_hides_half_the_referrers_from_count_and_sample() {
    let h = Harness::new().await;
    let customer = h.create("customer", &[("name", text("Scoped"))]).await;
    let mut docs = Vec::new();
    let mut visible = Vec::new();
    let mut hidden = Vec::new();
    for i in 0..10 {
        let d = doc(
            "invoice",
            &[
                ("number", Value::Text(format!("S-{i}"))),
                ("customer", Value::Id(customer.id())),
            ],
        );
        if i % 2 == 0 {
            visible.push(d.id());
        } else {
            hidden.push(d.id());
        }
        docs.push(d);
    }
    seed(&h, docs).await;

    let mut checks = Counting::new(&h);
    checks.visible = Some(visible.iter().map(|id| Value::Id(*id)).collect());
    let used = where_used(&mut checks, "customer", customer.id(), 100)
        .await
        .unwrap();
    let g = group(&used, "invoice", "customer").unwrap();
    assert_eq!(g.count, 5);
    assert_eq!(g.sample, smallest(&visible, 100));
    assert!(g.sample.iter().all(|id| !hidden.contains(id)));
}

#[tokio::test]
async fn unknown_doctype_empty_target_and_limit_edges() {
    let h = Harness::new().await;
    let mut checks = Counting::new(&h);
    let error = where_used(&mut checks, "widget", RecordId::new_v7(), 5)
        .await
        .unwrap_err();
    assert_eq!(error.code, "meta.unknown_doctype");

    let lonely = h.create("customer", &[("name", text("Lonely"))]).await;
    let used = where_used(&mut checks, "customer", lonely.id(), 5)
        .await
        .unwrap();
    assert_eq!(used.total, 0);
    assert!(used.groups.is_empty());

    let busy = h.create("customer", &[("name", text("Busy"))]).await;
    let docs: Vec<Document> = (0..1_200)
        .map(|i| {
            doc(
                "invoice",
                &[
                    ("number", Value::Text(format!("B-{i}"))),
                    ("customer", Value::Id(busy.id())),
                ],
            )
        })
        .collect();
    let ids: Vec<RecordId> = docs.iter().map(Document::id).collect();
    seed(&h, docs).await;
    for (asked, want) in [(0usize, 20usize), (1, 1), (5_000, 1_000)] {
        let mut checks = Counting::new(&h);
        let used = where_used(&mut checks, "customer", busy.id(), asked)
            .await
            .unwrap();
        let g = group(&used, "invoice", "customer").unwrap();
        assert_eq!(g.count, 1_200);
        assert_eq!(g.sample, smallest(&ids, want), "limit {asked}");
    }
}

/// A small deterministic generator: no float, no dependency.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, below: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % below
    }
}

#[tokio::test]
async fn answers_equal_the_ground_truth_over_random_referrer_sets_and_limits() {
    let mut rng = Lcg(7);
    for round in 0..12 {
        let h = Harness::new().await;
        let target = h.create("customer", &[("name", text("T"))]).await;
        let other = h.create("customer", &[("name", text("Other"))]).await;
        let mut docs = Vec::new();
        let mut direct = Vec::new();
        let mut child = Vec::new();
        for i in 0..rng.next(60) {
            let refers = rng.next(3) != 0;
            let owner = if refers { &target } else { &other };
            let d = doc(
                "invoice",
                &[
                    ("number", Value::Text(format!("R{round}-{i}"))),
                    ("customer", Value::Id(owner.id())),
                ],
            );
            if refers {
                direct.push(d.id());
            }
            docs.push(d);
        }
        for i in 0..rng.next(40) {
            let mut d = doc(
                "invoice",
                &[("number", Value::Text(format!("RC{round}-{i}")))],
            );
            let rows = 1 + rng.next(4);
            let mut any = false;
            for _ in 0..rows {
                let refers = rng.next(2) == 0;
                any |= refers;
                let who = if refers { &target } else { &other };
                d.rows_mut(name("lines")).push(row(&[
                    ("item", text("x")),
                    ("billing_customer", Value::Id(who.id())),
                ]));
            }
            if any {
                child.push(d.id());
            }
            docs.push(d);
        }
        seed(&h, docs).await;
        let limit = rng.next(30) as usize;
        let used = h
            .where_used_limit("customer", target.id(), limit)
            .await
            .unwrap();
        let want_limit = if limit == 0 { 20 } else { limit };
        let g = group(&used, "invoice", "customer");
        assert_eq!(
            g.map_or(0, |g| g.count),
            direct.len() as u64,
            "round {round}"
        );
        if let Some(g) = g {
            assert_eq!(g.sample, smallest(&direct, want_limit));
        }
        let g = group(&used, "invoice", "billing_customer");
        assert_eq!(
            g.map_or(0, |g| g.count),
            child.len() as u64,
            "round {round}"
        );
        if let Some(g) = g {
            assert_eq!(g.sample, smallest(&child, want_limit));
        }
        assert_eq!(used.total, (direct.len() + child.len()) as u64);
    }
}
