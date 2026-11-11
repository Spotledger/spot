//! A real `HubRelay` tick with `MasterScope` delivers only the covered entries.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use serde_json::json;
use spot_sync_closure::{
    ClosureError, ImageSource, NoRedaction, RecordKey, References, SourceImage,
};
use spot_sync_hub::{
    Destination, EntryRole, HubRelay, MemoryQueue, MemoryStream, RelayConfig, StreamEntry,
};
use spot_sync_hub_adapters::{MasterScope, ScopeKeysOf};
use spot_sync_master::{Doctype, ScopeKey, Subscription};
use spot_sync_obligations::Lane;
use spot_sync_protocol::NodeId;
use spot_sync_skew::{NodeReport, SoftwareVersion};
use std::collections::BTreeMap;

struct Registry(BTreeMap<RecordKey, Vec<String>>);

impl ScopeKeysOf for Registry {
    fn scope_keys(&self, k: &RecordKey) -> Option<Vec<String>> {
        self.0.get(k).cloned()
    }
}

struct NoRefs;
impl References for NoRefs {
    fn references(&self, _e: &str, _i: &serde_json::Value) -> Result<Vec<RecordKey>, ClosureError> {
        Ok(Vec::new())
    }
}

struct Src(BTreeMap<RecordKey, SourceImage>);
impl ImageSource for Src {
    fn image(&self, key: &RecordKey) -> Result<Option<SourceImage>, ClosureError> {
        Ok(self.0.get(key).cloned())
    }
}

fn entry(position: u64, id: &str) -> StreamEntry {
    StreamEntry {
        position,
        tenant: "t1".into(),
        source: "hub".into(),
        origin: None,
        txn: format!("t{position}"),
        role: EntryRole::Root,
        key: RecordKey::new("price_list", id),
        version: 1,
        topic: "price_list".into(),
        major: 1,
        lane: Lane::Bulk,
    }
}

fn report() -> NodeReport {
    NodeReport {
        software_version: SoftwareVersion::new(1, 0, 0),
        metadata_version: 1,
        migration_epoch: 1,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: [("price_list".to_owned(), [1u32].into_iter().collect())]
            .into_iter()
            .collect(),
    }
}

fn leak_sub(node: &str, company: &str) -> &'static Subscription {
    Box::leak(Box::new(Subscription::new(
        NodeId::new(node).unwrap(),
        [ScopeKey::new("company", company).unwrap()],
        [Doctype::new("price_list").unwrap()],
    )))
}

fn registry() -> &'static Registry {
    let mut m = BTreeMap::new();
    m.insert(
        RecordKey::new("price_list", "A"),
        vec!["company:C1".to_owned()],
    );
    m.insert(
        RecordKey::new("price_list", "B"),
        vec!["company:C2".to_owned()],
    );
    m.insert(RecordKey::new("price_list", "ALL"), Vec::new());
    // "UNKNOWN" is deliberately absent: unknown scope keys are not covered.
    Box::leak(Box::new(Registry(m)))
}

#[test]
fn a_tick_delivers_only_covered_entries_to_each_destination() {
    let mut images = BTreeMap::new();
    for (i, id) in ["A", "B", "ALL", "UNKNOWN"].iter().enumerate() {
        let key = RecordKey::new("price_list", *id);
        images.insert(
            key.clone(),
            SourceImage::new(key, json!({ "rate": "1.00" }), (i + 1) as u64),
        );
    }
    let mut stream = MemoryStream::new();
    for (i, id) in ["A", "B", "ALL", "UNKNOWN"].iter().enumerate() {
        stream.append(entry((i + 1) as u64, id));
    }
    let mut relay = HubRelay::new(
        RelayConfig::new("t1", "hub"),
        stream,
        MemoryQueue::new(),
        Src(images),
        Box::new(NoRefs),
    )
    .unwrap();
    let reg = registry();
    relay
        .add_destination(Destination::new(
            "edge-c1",
            Box::new(MasterScope::new(leak_sub("edge-c1", "C1"), reg)),
            Box::new(NoRedaction),
            report(),
        ))
        .unwrap();
    relay
        .add_destination(Destination::new(
            "edge-c2",
            Box::new(MasterScope::new(leak_sub("edge-c2", "C2"), reg)),
            Box::new(NoRedaction),
            report(),
        ))
        .unwrap();
    relay.tick(10).unwrap();

    let ids = |dest: &str| -> Vec<String> {
        relay
            .queue()
            .all(dest)
            .iter()
            .flat_map(|p| p.item.bundle.records.iter().map(|r| r.key.id.clone()))
            .collect()
    };
    assert_eq!(ids("edge-c1"), vec!["A", "ALL"]);
    assert_eq!(ids("edge-c2"), vec!["B", "ALL"]);
}
