//! Shared fixtures: a scripted carrier link, a range book over the in-memory store and a signing
//! world for the snapshot tests.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ed25519_dalek::Signer as _;
use spot_bundle_signing::PublicKey;
use spot_edge_offline_host::carrier::{self, LeaseAnswerMsg};
use spot_edge_offline_host::carrier_loop::{
    CarrierLink, CarrierRecord, Instants, LinkError, LinkFuture,
};
use spot_edge_offline_host::kinds::LEASE_ANSWER_KIND;
use spot_edge_offline_host::ranges::RangeBook;
use spot_edge_offline_host::series::SeriesPolicy;
use spot_offline_identity::{
    DirectoryEntry, Grant, Message, NodeClock, SignedDelegation, Signer, Snapshot, SnapshotStore,
    SyncClass, SyncClassEntry, Tier, TierDeadlines, TrustAnchor, sign_delegation, sign_snapshot,
};
use spot_partition::{HighWaterStore, LowWater, MemorySeriesStore};
use spot_sync_protocol::NodeId;

pub const HOUR: u64 = 3_600_000;
pub const T0: u64 = 1_000_000_000_000;

#[derive(Default)]
pub struct LinkState {
    pub connected: bool,
    pub sent: Vec<CarrierRecord>,
    pub inbox: VecDeque<CarrierRecord>,
    pub fail_send: bool,
}

#[derive(Clone, Default)]
pub struct FakeLink(pub Arc<Mutex<LinkState>>);

impl FakeLink {
    pub fn connected() -> Self {
        let link = Self::default();
        link.0.lock().unwrap().connected = true;
        link
    }

    pub fn set_connected(&self, on: bool) {
        self.0.lock().unwrap().connected = on;
    }

    pub fn sent(&self) -> Vec<CarrierRecord> {
        self.0.lock().unwrap().sent.clone()
    }

    pub fn push(&self, record: CarrierRecord) {
        self.0.lock().unwrap().inbox.push_back(record);
    }
}

impl CarrierLink for FakeLink {
    fn connected(&self) -> bool {
        self.0.lock().unwrap().connected
    }

    fn send(&self, record: CarrierRecord) -> LinkFuture<'_, ()> {
        Box::pin(async move {
            let mut state = self.0.lock().unwrap();
            if state.fail_send {
                return Err(LinkError("edge_offline.link_failed"));
            }
            state.sent.push(record);
            Ok(())
        })
    }

    fn receive(&self) -> LinkFuture<'_, Vec<CarrierRecord>> {
        Box::pin(async move { Ok(self.0.lock().unwrap().inbox.drain(..).collect()) })
    }
}

pub fn node() -> NodeId {
    NodeId::new("edge-1#branch_1").unwrap()
}

pub fn policies(block: u64) -> Vec<SeriesPolicy> {
    vec![SeriesPolicy {
        name: "INV".to_owned(),
        block,
    }]
}

pub fn book(block: u64, store: Arc<dyn HighWaterStore>) -> Arc<RangeBook> {
    Arc::new(RangeBook::open(node(), policies(block), store, LowWater::half()).unwrap())
}

pub fn memory_store() -> Arc<dyn HighWaterStore> {
    Arc::new(MemorySeriesStore::new())
}

pub fn granted(request_id: &str, from: u64, to: u64, lease_id: u64) -> CarrierRecord {
    CarrierRecord {
        kind: LEASE_ANSWER_KIND.to_owned(),
        payload: carrier::encode(&LeaseAnswerMsg::Granted {
            request_id: request_id.to_owned(),
            series: "INV".to_owned(),
            from,
            to,
            lease_id,
        })
        .unwrap(),
    }
}

pub fn refused(request_id: &str) -> CarrierRecord {
    CarrierRecord {
        kind: LEASE_ANSWER_KIND.to_owned(),
        payload: carrier::encode(&LeaseAnswerMsg::Refused {
            request_id: request_id.to_owned(),
            code: "lease_grants.not_entitled".to_owned(),
        })
        .unwrap(),
    }
}

pub fn at(secs: u64) -> Instants {
    Instants {
        mono: Duration::from_secs(secs),
        node: NodeClock {
            mono_ms: secs * 1_000,
            boot_id: 1,
        },
    }
}

pub struct Key(pub ed25519_dalek::SigningKey);

impl Key {
    pub fn new(seed: u8) -> Self {
        Self(ed25519_dalek::SigningKey::from_bytes(&[seed; 32]))
    }
}

impl Signer for Key {
    fn public_key(&self) -> PublicKey {
        PublicKey::from_bytes(&self.0.verifying_key().to_bytes()).unwrap()
    }

    fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.0.sign(message).to_bytes().to_vec()
    }
}

pub struct World {
    pub root: Key,
    pub snap: Key,
    pub delegation: SignedDelegation,
}

pub fn tenant() -> spot_offline_identity::TenantId {
    spot_offline_identity::TenantId::new("acme").unwrap()
}

pub fn ident() -> spot_offline_identity::NodeId {
    spot_offline_identity::NodeId::new("edge-1#branch_1").unwrap()
}

impl World {
    pub fn new() -> Self {
        let (root, snap, ts) = (Key::new(1), Key::new(2), Key::new(3));
        let delegation = sign_delegation(&root, &tenant(), 1, &snap.public_key(), &ts.public_key());
        Self {
            root,
            snap,
            delegation,
        }
    }

    /// A world whose root is another key (its delegation is not the node's trusted one).
    pub fn with_root(seed: u8) -> Self {
        let (root, snap, ts) = (Key::new(seed), Key::new(2), Key::new(3));
        let delegation = sign_delegation(&root, &tenant(), 1, &snap.public_key(), &ts.public_key());
        Self {
            root,
            snap,
            delegation,
        }
    }

    pub fn anchor(&self) -> TrustAnchor {
        TrustAnchor {
            tenant: tenant(),
            node: ident(),
            root: self.root.public_key(),
        }
    }

    pub fn snapshot(&self, epoch: u64, issued_at: u64, leases: [u64; 3]) -> Snapshot {
        Snapshot {
            tenant: tenant(),
            node: ident(),
            epoch,
            issued_at,
            valid_until: TierDeadlines(leases.map(|l| issued_at + l)),
            grants: vec![Grant {
                user: "clerk".to_owned(),
                assignment_id: "a-clerk".to_owned(),
                role: "clerk".to_owned(),
                permissions: vec!["invoice:write".to_owned()],
            }],
            directory: vec![DirectoryEntry {
                user: "clerk".to_owned(),
                enabled: true,
                offline_allowed: true,
                highest_tier: Tier::Three,
            }],
            sync_classes: vec![SyncClassEntry {
                group: "bank_details".to_owned(),
                class: SyncClass::Encrypted,
            }],
            key_epoch: 1,
            revocations: Vec::new(),
        }
    }

    pub fn message(&self, snapshot: Snapshot) -> Message {
        Message::Snapshot(sign_snapshot(snapshot, &self.snap, self.delegation.clone()))
    }
}

pub fn open_store(world: &World) -> SnapshotStore<spot_offline_identity::MemoryPersistence> {
    SnapshotStore::open(
        world.anchor(),
        spot_offline_identity::MemoryPersistence::default(),
        NodeClock {
            mono_ms: 0,
            boot_id: 1,
        },
    )
}
