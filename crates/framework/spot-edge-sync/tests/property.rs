//! The acceptance, as a property: after any random sequence of insert, update, delete and
//! cancel, the edge set equals the link set.
//!
//! A [`World`] holds the live documents and a [`MemoryEdgeWriter`] holds their edges. Every step
//! goes through the real hook (the same one the document engine calls), so the writer is
//! maintained exactly as production maintains it. After every step [`check_invariant`] must be
//! empty.

// Test code may unwrap and expect what the test itself built.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{block_on, doc, name, rid, row, schema, set};
use proptest::prelude::*;
use spot_app_api::prelude::{Value, Version};
use spot_app_api::{DocState, Document};
use spot_edge_sync::{EdgeSchema, EdgeSync, LifecycleHook, MemoryEdgeWriter, check_invariant};

/// One step of the sequence.
#[derive(Clone, Debug)]
enum Step {
    /// Create or replace the document in `slot`.
    Put {
        slot: u8,
        doctype: u8,
        a: u8,
        b: u8,
        c: u8,
    },
    /// Delete the document in `slot`.
    Delete { slot: u8 },
    /// Cancel the document in `slot` (its links stay, so its edges stay).
    Cancel { slot: u8 },
}

fn step_strategy() -> impl Strategy<Value = Step> {
    prop_oneof![
        (0u8..12, 0u8..4, any::<u8>(), any::<u8>(), any::<u8>()).prop_map(
            |(slot, doctype, a, b, c)| Step::Put {
                slot,
                doctype,
                a,
                b,
                c
            }
        ),
        (0u8..12).prop_map(|slot| Step::Delete { slot }),
        (0u8..12).prop_map(|slot| Step::Cancel { slot }),
    ]
}

/// The live documents, by slot.
#[derive(Default)]
struct World {
    slots: Vec<Option<Document>>,
}

impl World {
    fn get(&self, slot: u8) -> Option<&Document> {
        self.slots.get(slot as usize).and_then(Option::as_ref)
    }

    fn put(&mut self, slot: u8, document: Document) {
        let index = slot as usize;
        if self.slots.len() <= index {
            self.slots.resize(index + 1, None);
        }
        self.slots[index] = Some(document);
    }

    fn take(&mut self, slot: u8) -> Option<Document> {
        self.slots.get_mut(slot as usize).and_then(Option::take)
    }

    fn docs(&self) -> impl Iterator<Item = &Document> {
        self.slots.iter().filter_map(Option::as_ref)
    }

    fn apply(
        &mut self,
        schema: &EdgeSchema,
        hook: &EdgeSync,
        writer: &mut MemoryEdgeWriter,
        step: &Step,
    ) {
        match step {
            Step::Put {
                slot,
                doctype,
                a,
                b,
                c,
            } => {
                let old = self.get(*slot).cloned();
                let new = build_doc(*slot, *doctype, *a, *b, *c);
                block_on(hook.after_write(schema, old.as_ref(), Some(&new), writer))
                    .expect("the in-memory writer never fails");
                self.put(*slot, new);
            }
            Step::Delete { slot } => {
                let old = self.take(*slot);
                if let Some(old) = &old {
                    block_on(hook.after_write(schema, Some(old), None, writer))
                        .expect("the in-memory writer never fails");
                }
            }
            Step::Cancel { slot } => {
                let Some(old) = self.get(*slot).cloned() else {
                    return;
                };
                let mut cancelled = old.clone();
                cancelled.mark_saved(
                    old.version().unwrap_or(Version::FIRST),
                    Some(DocState::Cancelled),
                );
                block_on(hook.after_write(schema, Some(&old), Some(&cancelled), writer))
                    .expect("the in-memory writer never fails");
                self.put(*slot, cancelled);
            }
        }
    }
}

/// A party id from a byte.
fn party_id(n: u8) -> u64 {
    1 + u64::from(n % 8)
}

/// An order id from a byte.
fn order_id(n: u8) -> u64 {
    100 + u64::from(n % 8)
}

/// A project id from a byte.
fn project_id(n: u8) -> u64 {
    200 + u64::from(n % 8)
}

/// Builds a document of the chosen DocType, with links drawn from the byte arguments.
fn build_doc(slot: u8, doctype: u8, a: u8, b: u8, c: u8) -> Document {
    let id = rid(u64::from(slot) + 1);
    match doctype % 4 {
        0 => {
            let mut party = doc("party", id);
            set(&mut party, "code", Value::Text(format!("P{a}")));
            party
        }
        1 => {
            let mut order = doc("order", id);
            set(&mut order, "number", Value::Text(format!("O{slot}")));
            if a > 0 {
                set(&mut order, "customer", Value::Id(rid(party_id(a))));
            }
            if c > 0 {
                let (kind, target) = if b.is_multiple_of(2) {
                    ("party", rid(party_id(c)))
                } else {
                    ("project", rid(project_id(c)))
                };
                set(&mut order, "ref_kind", Value::Text(kind.into()));
                set(&mut order, "ref", Value::Id(target));
            }
            if a.is_multiple_of(3) && b > 0 {
                order.rows_mut(name("lines")).push(row(
                    rid(500 + u64::from(slot)),
                    &[("item", Value::Id(rid(party_id(b))))],
                ));
            }
            if b.is_multiple_of(3) && c > 0 {
                order.rows_mut(name("watchers")).push(row(
                    rid(600 + u64::from(slot)),
                    &[("party", Value::Id(rid(party_id(c))))],
                ));
            }
            order
        }
        2 => {
            let mut project = doc("project", id);
            set(&mut project, "title", Value::Text(format!("J{slot}")));
            if a > 0 {
                set(&mut project, "customer", Value::Id(rid(party_id(a))));
            }
            project
        }
        _ => {
            let mut task = doc("task_link", id);
            if a > 0 {
                set(&mut task, "order", Value::Id(rid(order_id(a))));
            }
            if b > 0 {
                set(&mut task, "project", Value::Id(rid(project_id(b))));
            }
            set(&mut task, "role", Value::Text(format!("role{c}")));
            task
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn the_invariant_holds_after_every_step(steps in prop::collection::vec(step_strategy(), 0..80)) {
        let schema = schema();
        let hook = EdgeSync::new();
        let mut writer = MemoryEdgeWriter::new();
        let mut world = World::default();
        for step in &steps {
            world.apply(&schema, &hook, &mut writer, step);
            let drift = check_invariant(&schema, world.docs(), writer.all());
            prop_assert!(
                drift.is_empty(),
                "drift after {:?}: {:?}",
                step,
                drift
            );
        }
    }
}

#[test]
fn a_custom_link_field_added_later_gets_edges_with_no_code_change() {
    // The world is built and synced with the standard schema.
    let standard = schema();
    let hook = EdgeSync::new();
    let mut writer = MemoryEdgeWriter::new();
    let mut world = World::default();
    for slot in 0..6 {
        world.apply(
            &standard,
            &hook,
            &mut writer,
            &Step::Put {
                slot,
                doctype: slot % 4,
                a: slot + 1,
                b: slot + 2,
                c: slot + 3,
            },
        );
    }
    assert!(check_invariant(&standard, world.docs(), writer.all()).is_empty());

    // A site adds a custom link field to `order` at runtime; a fresh derive picks it up with no
    // code change, and re-syncing every order brings the new edges in.
    let extended = EdgeSchema::derive(&common::meta_with_custom_link(), &[common::relation_spec()]);
    for slot in 0..6u8 {
        let Some(old) = world.get(slot).cloned() else {
            continue;
        };
        if old.doctype().as_str() != "order" {
            continue;
        }
        let mut new = old.clone();
        set(&mut new, "custom_ref", Value::Id(rid(party_id(slot + 1))));
        block_on(hook.after_write(&extended, Some(&old), Some(&new), &mut writer))
            .expect("the in-memory writer never fails");
        world.put(slot, new);
    }
    let drift = check_invariant(&extended, world.docs(), writer.all());
    assert!(
        drift.is_empty(),
        "the custom field's edges are in the set: {drift:?}"
    );
}
