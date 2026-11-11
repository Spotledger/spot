//! The acceptance through the real engine: after any random sequence of create, update, post,
//! cancel, correct and delete made through the document engine on the fake backend, the stored
//! edge set equals the link set of the documents the engine holds.
//!
//! Operations the engine refuses (deleting a posted document, cancelling a draft, ...) are part
//! of the sequence: a refused operation must leave the edge set exactly as it was.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{doc, name, row, set};
use proptest::prelude::*;
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{CommandId, RecordId, SiteId, Value};
use spot_app_api::{Document, DocumentPatch, RowPatch};
use spot_core::ManualClock;
use spot_document::engine::{Engine, TxParticipant, V7Ids};
use spot_edge_sync::{EdgeParticipant, EdgeSchema, MemoryEdgeStore, check_invariant};
use spot_meta::{Customisations, EffectiveMeta, Registry};
use spot_seams::RegistryBuilder;
use spot_storage_fake::{FakeBackend, FakeTx};
use time::OffsetDateTime;

fn inv() -> Invocation {
    Invocation {
        site: SiteId::new("edges.property").unwrap(),
        actor: Actor::System {
            component: name("edge_property"),
        },
        command_id: CommandId::new("cmd-prop").ok(),
        language: None,
        now: OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap(),
    }
}

fn meta() -> Arc<EffectiveMeta> {
    let mut defs = common::definitions();
    for def in &mut defs {
        if def.name == "order" {
            def.is_posted = true;
        }
    }
    Arc::new(Registry::build(defs, &Customisations::new()).unwrap())
}

#[derive(Clone, Debug)]
enum Step {
    CreateOrder { a: u8, b: u8, c: u8 },
    CreateProject { a: u8 },
    CreateTask { a: u8, b: u8, c: u8 },
    Update { slot: u8, a: u8, b: u8 },
    Post { slot: u8 },
    Cancel { slot: u8 },
    Correct { slot: u8 },
    Delete { slot: u8 },
}

fn step_strategy() -> impl Strategy<Value = Step> {
    let byte = 0u8..6;
    prop_oneof![
        (byte.clone(), byte.clone(), byte.clone()).prop_map(|(a, b, c)| Step::CreateOrder {
            a,
            b,
            c
        }),
        byte.clone().prop_map(|a| Step::CreateProject { a }),
        (byte.clone(), byte.clone(), byte.clone()).prop_map(|(a, b, c)| Step::CreateTask {
            a,
            b,
            c
        }),
        (any::<u8>(), byte.clone(), byte).prop_map(|(slot, a, b)| Step::Update { slot, a, b }),
        any::<u8>().prop_map(|slot| Step::Post { slot }),
        any::<u8>().prop_map(|slot| Step::Cancel { slot }),
        any::<u8>().prop_map(|slot| Step::Correct { slot }),
        any::<u8>().prop_map(|slot| Step::Delete { slot }),
    ]
}

struct World {
    engine: Engine<FakeBackend>,
    store: MemoryEdgeStore,
    schema: EdgeSchema,
    parties: Vec<RecordId>,
    /// Every document the sequence made, by DocType, in creation order.
    tracked: Vec<(&'static str, RecordId)>,
    succeeded: usize,
}

impl World {
    async fn new() -> Self {
        let meta = meta();
        let store = MemoryEdgeStore::new();
        let engine = Engine::new(
            FakeBackend::new(),
            meta.clone(),
            Arc::new(RegistryBuilder::new().build().unwrap()),
            Arc::new(ManualClock::new(
                OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap(),
            )),
            Arc::new(V7Ids),
        )
        .unwrap();
        engine.prepare().await.unwrap();
        let source = meta.clone();
        let participant: EdgeParticipant<FakeTx, _> =
            EdgeParticipant::new(Arc::new(move || source.clone()), store.clone())
                .with_relations(vec![common::relation_spec()]);
        engine.set_participants(vec![Arc::new(participant) as Arc<dyn TxParticipant<FakeTx>>]);
        let mut parties = Vec::new();
        for n in 0..4 {
            let mut party = doc("party", RecordId::new_v7());
            set(&mut party, "code", Value::Text(format!("P{n}")));
            parties.push(engine.create(&inv(), party).await.unwrap().value.id());
        }
        let schema = EdgeSchema::derive(&meta, &[common::relation_spec()]);
        Self {
            engine,
            store,
            schema,
            parties,
            tracked: Vec::new(),
            succeeded: 0,
        }
    }

    fn party(&self, n: u8) -> Option<RecordId> {
        (n > 0).then(|| self.parties[usize::from(n - 1) % self.parties.len()])
    }

    fn slot(&self, slot: u8, doctype: &str) -> Option<RecordId> {
        let of: Vec<_> = self.tracked.iter().filter(|(d, _)| *d == doctype).collect();
        (!of.is_empty()).then(|| of[usize::from(slot) % of.len()].1)
    }

    async fn run(&mut self, step: &Step) {
        match step {
            Step::CreateOrder { a, b, c } => {
                let mut order = doc("order", RecordId::new_v7());
                if let Some(p) = self.party(*a) {
                    set(&mut order, "customer", Value::Id(p));
                }
                if let Some(p) = self.party(*b) {
                    set(&mut order, "ref_kind", Value::Text("party".into()));
                    set(&mut order, "ref", Value::Id(p));
                    order
                        .rows_mut(name("watchers"))
                        .push(row(RecordId::new_v7(), &[("party", Value::Id(p))]));
                }
                if let Some(p) = self.party(*c) {
                    order
                        .rows_mut(name("lines"))
                        .push(row(RecordId::new_v7(), &[("item", Value::Id(p))]));
                }
                self.created("order", self.engine.create(&inv(), order).await);
            }
            Step::CreateProject { a } => {
                let mut project = doc("project", RecordId::new_v7());
                if let Some(p) = self.party(*a) {
                    set(&mut project, "customer", Value::Id(p));
                }
                self.created("project", self.engine.create(&inv(), project).await);
            }
            Step::CreateTask { a, b, c } => {
                let mut task = doc("task_link", RecordId::new_v7());
                if let Some(o) = self.slot(*a, "order") {
                    set(&mut task, "order", Value::Id(o));
                }
                if let Some(p) = self.slot(*b, "project") {
                    set(&mut task, "project", Value::Id(p));
                }
                set(&mut task, "role", Value::Text(format!("role{c}")));
                self.created("task_link", self.engine.create(&inv(), task).await);
            }
            Step::Update { slot, a, b } => {
                let Some((doctype, id)) = self.any(*slot) else {
                    return;
                };
                let Some(current) = self.engine.peek(doctype, id).await.unwrap() else {
                    return;
                };
                let mut patch = DocumentPatch::default();
                match doctype {
                    "order" => {
                        let value = self.party(*a).map_or(Value::Null, Value::Id);
                        patch.fields.insert(name("customer"), value);
                        let rows = self
                            .party(*b)
                            .map(|p| {
                                vec![RowPatch {
                                    id: None,
                                    fields: [(name("item"), Value::Id(p))].into(),
                                }]
                            })
                            .unwrap_or_default();
                        patch.tables.insert(name("lines"), rows);
                    }
                    "project" => {
                        let value = self.party(*a).map_or(Value::Null, Value::Id);
                        patch.fields.insert(name("customer"), value);
                    }
                    "task_link" => {
                        patch
                            .fields
                            .insert(name("role"), Value::Text(format!("r{a}{b}")));
                    }
                    _ => return,
                }
                let result = self
                    .engine
                    .update(&inv(), doctype, id, patch, current.version().unwrap())
                    .await;
                self.counted(result.is_ok());
            }
            Step::Post { slot } => {
                if let Some((d, id, v)) = self.target(*slot, "order").await {
                    let r = self.engine.post(&inv(), d, id, v).await;
                    self.counted(r.is_ok());
                }
            }
            Step::Cancel { slot } => {
                if let Some((d, id, v)) = self.target(*slot, "order").await {
                    let r = self.engine.cancel(&inv(), d, id, v).await;
                    self.counted(r.is_ok());
                }
            }
            Step::Correct { slot } => {
                if let Some((d, id, v)) = self.target(*slot, "order").await {
                    let r = self.engine.correct(&inv(), d, id, v).await;
                    if let Ok(outcome) = &r {
                        self.tracked.push(("order", outcome.value.id()));
                    }
                    self.counted(r.is_ok());
                }
            }
            Step::Delete { slot } => {
                let Some((doctype, id)) = self.any(*slot) else {
                    return;
                };
                if let Some(current) = self.engine.peek(doctype, id).await.unwrap() {
                    let r = self
                        .engine
                        .delete(&inv(), doctype, id, current.version().unwrap())
                        .await;
                    self.counted(r.is_ok());
                }
            }
        }
    }

    fn any(&self, slot: u8) -> Option<(&'static str, RecordId)> {
        (!self.tracked.is_empty()).then(|| self.tracked[usize::from(slot) % self.tracked.len()])
    }

    async fn target(
        &self,
        slot: u8,
        doctype: &'static str,
    ) -> Option<(&'static str, RecordId, spot_app_api::prelude::Version)> {
        let id = self.slot(slot, doctype)?;
        let current = self.engine.peek(doctype, id).await.unwrap()?;
        Some((doctype, id, current.version().unwrap()))
    }

    fn created(
        &mut self,
        doctype: &'static str,
        result: spot_app_api::AppResult<spot_app_api::context::Outcome<Document>>,
    ) {
        if let Ok(outcome) = &result {
            self.tracked.push((doctype, outcome.value.id()));
        }
        self.counted(result.is_ok());
    }

    fn counted(&mut self, ok: bool) {
        self.succeeded += usize::from(ok);
    }

    async fn edges(&self) -> std::collections::BTreeSet<spot_edge_sync::Edge> {
        self.store.all(self.engine.backend()).await.unwrap()
    }

    /// Edge count drift against every document the engine still holds.
    async fn drift(&self) -> Vec<spot_edge_sync::Drift> {
        let mut docs = Vec::new();
        for (doctype, id) in &self.tracked {
            if let Some(doc) = self.engine.peek(doctype, *id).await.unwrap() {
                docs.push(doc);
            }
        }
        check_invariant(&self.schema, docs.iter(), self.edges().await)
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    #[test]
    fn the_edge_set_equals_the_link_set_after_every_engine_operation(
        steps in prop::collection::vec(step_strategy(), 0..40)
    ) {
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.block_on(async {
            let mut world = World::new().await;
            for step in &steps {
                let before = world.edges().await;
                let ok_before = world.succeeded;
                world.run(step).await;
                if world.succeeded == ok_before {
                    // An operation the engine refused (or rolled back) leaves no edge behind.
                    prop_assert_eq!(&world.edges().await, &before, "a refused {:?} changed the edges", step);
                }
                let drift = world.drift().await;
                prop_assert!(drift.is_empty(), "drift after {:?}: {:?}", step, drift);
            }
            Ok(())
        })?;
    }
}
