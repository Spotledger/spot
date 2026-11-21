//! Strategies for the property suite (WP-CTL-10.T11): records, hostile text and delivery
//! schedules. Nothing here reads a clock or a random source: a failing case replays exactly.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::BTreeMap;

use proptest::prelude::*;
use spot_link_exchange::Envelope;
use spot_replica_apply::{DocPayload, RecordPayload, WireValue};
use spot_sync_protocol::RecordName;
use spot_tenant_link::{FieldList, FieldName};

/// Field names a record may carry; the first three are the ones the fixture grant lists.
pub const FIELD_POOL: [&str; 6] = [
    "customer",
    "total",
    "internal_margin",
    "secret_note",
    "party_name",
    "notes",
];

/// The fields the record grant of the fixture world lists (see `common::offer`).
pub const GRANTED: [&str; 2] = ["customer", "total"];

/// A record envelope with a random subset of [`FIELD_POOL`] and random text or decimal values.
pub fn envelope() -> impl Strategy<Value = Envelope> {
    (
        "[A-Z]{3}-[0-9]{4}",
        1u64..1_000,
        proptest::collection::btree_map(
            0usize..FIELD_POOL.len(),
            prop_oneof![
                "[ -~]{0,16}".prop_map(WireValue::Text),
                (0u32..100_000, 0u32..100)
                    .prop_map(|(whole, cents)| WireValue::Decimal(format!("{whole}.{cents:02}"))),
            ],
            0..FIELD_POOL.len(),
        ),
    )
        .prop_map(|(name, version, picked)| {
            let mut fields = BTreeMap::new();
            for (index, value) in picked {
                fields.insert(FIELD_POOL[index].to_owned(), value);
            }
            Envelope {
                name: RecordName::new(&name).unwrap(),
                version,
                record: RecordPayload {
                    document: DocPayload {
                        entity: "invoice".to_owned(),
                        id: "01890a5d-ac96-774b-bcce-b302099a8057".to_owned(),
                        fields,
                        children: BTreeMap::new(),
                    },
                    entries: Vec::new(),
                    meta_version: 1,
                },
            }
        })
}

/// A field list built from a random subset of the pool.
pub fn field_list() -> impl Strategy<Value = FieldList> {
    proptest::collection::btree_set(0usize..FIELD_POOL.len(), 0..FIELD_POOL.len()).prop_map(
        |picked| {
            FieldList::only(
                picked
                    .into_iter()
                    .map(|index| FieldName::new(FIELD_POOL[index]).unwrap()),
            )
        },
    )
}

/// One step of a delivery schedule.
#[derive(Clone, Copy, Debug)]
pub enum Step {
    /// The application queues a message on conversation 0 to 2.
    Enqueue(u8),
    /// The sender pumps its outbox onto the wire.
    Pump,
    /// One in-flight message is delivered, lost, or delivered and left on the wire (duplicated).
    Message { pick: usize, fate: Fate },
    /// One in-flight receipt is delivered, lost, or duplicated.
    Receipt { pick: usize, fate: Fate },
    /// The receiver sweeps expired inputs.
    Sweep,
    /// Both sides restart: every wrapper is rebuilt from the persisted stores, the wire is kept.
    Restart,
}

/// What the channel does to one item.
#[derive(Clone, Copy, Debug)]
pub enum Fate {
    /// Delivered once.
    Deliver,
    /// Lost.
    Drop,
    /// Delivered and a copy stays on the wire.
    Duplicate,
}

pub fn fate() -> impl Strategy<Value = Fate> {
    prop_oneof![
        3 => Just(Fate::Deliver),
        1 => Just(Fate::Drop),
        1 => Just(Fate::Duplicate),
    ]
}

pub fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0u8..3).prop_map(Step::Enqueue),
        3 => Just(Step::Pump),
        4 => (any::<usize>(), fate()).prop_map(|(pick, fate)| Step::Message { pick, fate }),
        3 => (any::<usize>(), fate()).prop_map(|(pick, fate)| Step::Receipt { pick, fate }),
        1 => Just(Step::Sweep),
        1 => Just(Step::Restart),
    ]
}

/// A schedule of up to 60 steps.
pub fn schedule() -> impl Strategy<Value = Vec<Step>> {
    proptest::collection::vec(step(), 0..60)
}
