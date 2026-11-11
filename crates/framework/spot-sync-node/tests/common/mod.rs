#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! Test doubles: an in-memory apply sink, a JSON-driven catalog and bundle builders.
#![allow(dead_code)]

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};
use spot_events_contract::SourceId;
use spot_sync_closure::{Bundle, BundleKind, BundleRecord, ClosureError, RecordKey, Role};
use spot_sync_node::{
    AppliedRecord, AppliedSet, ApplyRefusal, BundleApply, Delivery, MemoryParking, NodeCatalog,
    NodeConfig, NodeIntake, ParkingLimits,
};
use spot_sync_skew::{Incoming, NodeReport, RecordRef, SoftwareVersion};

pub const TENANT: &str = "acme";

#[derive(Default)]
pub struct MemApply {
    pub stored: BTreeMap<(String, RecordKey), u64>,
    pub order: Vec<(String, RecordKey)>,
    /// Tenants whose bundles are refused (empty: use `refuse` for all).
    pub refuse_tenants: BTreeSet<String>,
    pub refuse: bool,
    pub calls: usize,
}

impl BundleApply for MemApply {
    fn has(&self, tenant: &str, key: &RecordKey, min_version: u64) -> bool {
        self.stored
            .get(&(tenant.to_owned(), key.clone()))
            .is_some_and(|v| *v >= min_version)
    }

    fn apply_bundle(&mut self, tenant: &str, bundle: &Bundle) -> Result<AppliedSet, ApplyRefusal> {
        self.calls += 1;
        if self.refuse || self.refuse_tenants.contains(tenant) {
            return Err(ApplyRefusal {
                reason: "refused".into(),
            });
        }
        let mut records = Vec::new();
        for r in &bundle.records {
            self.stored
                .insert((tenant.to_owned(), r.key.clone()), r.position);
            self.order.push((tenant.to_owned(), r.key.clone()));
            records.push(AppliedRecord {
                key: r.key.clone(),
                version: r.position,
            });
        }
        Ok(AppliedSet { records })
    }
}

/// References are `image.refs = [[entity, id], ...]`; `image.major` (default 1) feeds triage.
pub struct JsonCatalog;

impl NodeCatalog for JsonCatalog {
    fn references(&self, record: &BundleRecord) -> Result<Vec<RecordKey>, ClosureError> {
        let mut out = Vec::new();
        if let Some(Value::Array(refs)) = record.image.get("refs") {
            for r in refs {
                if let (Some(e), Some(i)) = (
                    r.get(0).and_then(Value::as_str),
                    r.get(1).and_then(Value::as_str),
                ) {
                    out.push(RecordKey::new(e, i));
                }
            }
        }
        Ok(out)
    }

    fn classify(&self, record: &BundleRecord) -> Incoming {
        let major = record
            .image
            .get("major")
            .and_then(Value::as_u64)
            .and_then(|m| u32::try_from(m).ok())
            .unwrap_or(1);
        Incoming {
            record: RecordRef {
                entity: record.key.entity.clone(),
                id: record.key.id.clone(),
                version: record.position,
            },
            topic: "t".into(),
            major,
            requires_metadata: None,
        }
    }
}

pub fn report() -> NodeReport {
    let mut majors = BTreeMap::new();
    majors.insert("t".to_owned(), BTreeSet::from([1_u32]));
    NodeReport {
        software_version: SoftwareVersion::new(1, 0, 0),
        metadata_version: 1,
        migration_epoch: 1,
        epoch_verified: true,
        mid_migration: false,
        decodable_majors: majors,
    }
}

pub fn hub() -> SourceId {
    SourceId::new("hub.example.app").expect("valid source")
}

pub fn config(limits: ParkingLimits) -> NodeConfig {
    NodeConfig {
        hub_source: hub(),
        limits,
        max_tenants: 8,
    }
}

pub type Intake = NodeIntake<MemApply, MemoryParking, JsonCatalog>;

pub fn intake() -> Intake {
    intake_with(ParkingLimits::default())
}

pub fn intake_with(limits: ParkingLimits) -> Intake {
    NodeIntake::new(
        config(limits),
        MemApply::default(),
        MemoryParking::new(),
        JsonCatalog,
        report(),
    )
    .expect("valid config")
}

pub fn rec(id: &str, position: u64, refs: &[&str]) -> BundleRecord {
    let refs: Vec<Value> = refs.iter().map(|r| json!(["Doc", r])).collect();
    BundleRecord {
        role: Role::Root,
        key: RecordKey::new("Doc", id),
        position,
        image: json!({ "refs": refs }),
    }
}

pub fn bundle(records: Vec<BundleRecord>) -> Bundle {
    let bytes = records
        .iter()
        .map(|r| serde_json::to_string(&r.image).map_or(0, |s| s.len()))
        .sum();
    Bundle {
        kind: BundleKind::Root,
        records,
        bytes,
    }
}

pub fn deliver(b: Bundle) -> Delivery {
    deliver_to(TENANT, b)
}

pub fn deliver_to(tenant: &str, b: Bundle) -> Delivery {
    Delivery {
        source: hub(),
        tenant: tenant.into(),
        bundle: b,
    }
}

pub fn key(id: &str) -> RecordKey {
    RecordKey::new("Doc", id)
}
