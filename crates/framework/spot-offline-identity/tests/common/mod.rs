//! Shared fixtures: test keys, a world with a root and two online keys, a snapshot builder.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use ed25519_dalek::Signer as _;
use spot_bundle_signing::PublicKey;
use spot_offline_identity::*;

pub const HOUR: u64 = 3_600_000;
pub const T0: u64 = 1_000_000_000_000;

pub struct Key(pub ed25519_dalek::SigningKey);

impl Key {
    pub fn new(seed: u8) -> Self {
        Self(ed25519_dalek::SigningKey::from_bytes(&[seed; 32]))
    }
}

impl spot_offline_identity::Signer for Key {
    fn public_key(&self) -> PublicKey {
        PublicKey::from_bytes(&self.0.verifying_key().to_bytes()).unwrap()
    }

    fn sign(&self, message: &[u8]) -> Vec<u8> {
        self.0.sign(message).to_bytes().to_vec()
    }
}

pub fn tenant() -> TenantId {
    TenantId::new("acme").unwrap()
}

pub fn node() -> NodeId {
    NodeId::new("branch-1").unwrap()
}

pub struct World {
    pub root: Key,
    pub snap: Key,
    pub ts: Key,
    pub delegation: SignedDelegation,
}

impl World {
    pub fn new() -> Self {
        let root = Key::new(1);
        let snap = Key::new(2);
        let ts = Key::new(3);
        let delegation = sign_delegation(&root, &tenant(), 1, &snap.public_key(), &ts.public_key());
        Self {
            root,
            snap,
            ts,
            delegation,
        }
    }

    /// A world whose root is a different key, so its delegation is not the node's trusted one.
    pub fn new_with_root(seed: u8) -> Self {
        let root = Key::new(seed);
        let snap = Key::new(2);
        let ts = Key::new(3);
        let delegation = sign_delegation(&root, &tenant(), 1, &snap.public_key(), &ts.public_key());
        Self {
            root,
            snap,
            ts,
            delegation,
        }
    }

    pub fn anchor(&self) -> TrustAnchor {
        TrustAnchor {
            tenant: tenant(),
            node: node(),
            root: self.root.public_key(),
        }
    }

    pub fn signed(&self, snapshot: Snapshot) -> Message {
        Message::Snapshot(sign_snapshot(snapshot, &self.snap, self.delegation.clone()))
    }

    pub fn timestamp(&self, snapshot: &Snapshot, signed_at: u64, leases: [u64; 3]) -> Message {
        self.timestamp_with_nonce(snapshot, signed_at, leases, signed_at)
    }

    pub fn timestamp_with_nonce(
        &self,
        snapshot: &Snapshot,
        signed_at: u64,
        leases: [u64; 3],
        nonce: u64,
    ) -> Message {
        let ts = Timestamp {
            tenant: snapshot.tenant.clone(),
            node: snapshot.node.clone(),
            epoch: snapshot.epoch,
            snapshot_hash: snapshot.hash(),
            signed_at,
            valid_until: TierDeadlines(leases.map(|l| signed_at + l)),
            nonce,
        };
        Message::Timestamp(sign_timestamp(ts, &self.ts, self.delegation.clone()))
    }

    pub fn hub_time(&self, epoch: u64, hub_ms: u64) -> Message {
        self.hub_time_signed_by(&self.ts, epoch, hub_ms)
    }

    pub fn hub_time_signed_by(&self, key: &Key, epoch: u64, hub_ms: u64) -> Message {
        self.hub_time_full(key, epoch, hub_ms, hub_ms)
    }

    pub fn hub_time_full(&self, key: &Key, epoch: u64, hub_ms: u64, nonce: u64) -> Message {
        let time = HubTime {
            tenant: tenant(),
            node: node(),
            epoch,
            hub_ms,
            nonce,
        };
        Message::HubTime(sign_hub_time(time, key, self.delegation.clone()))
    }

    /// A store at boot 1, mono 0, with a valid certificate and `snapshot` accepted at T0.
    pub fn store_with(&self, snapshot: Snapshot) -> SnapshotStore<MemoryPersistence> {
        let mut store =
            SnapshotStore::open(self.anchor(), MemoryPersistence::default(), clock(0, 1));
        store
            .renew_certificate(1, MAX_CERT_TTL_MS, true, clock(0, 1))
            .unwrap();
        store.accept(&self.signed(snapshot), rx(0, 1)).unwrap();
        store
    }
}

pub fn clock(mono_ms: u64, boot_id: u64) -> NodeClock {
    NodeClock { mono_ms, boot_id }
}

pub fn rx(mono_ms: u64, boot_id: u64) -> Received {
    Received {
        clock: clock(mono_ms, boot_id),
    }
}

pub fn dir(user: &str, tier: Tier) -> DirectoryEntry {
    DirectoryEntry {
        user: user.to_owned(),
        enabled: true,
        offline_allowed: true,
        highest_tier: tier,
    }
}

pub fn grant(user: &str, assignment: &str, perms: &[&str]) -> Grant {
    Grant {
        user: user.to_owned(),
        assignment_id: assignment.to_owned(),
        role: "role".to_owned(),
        permissions: perms.iter().map(|p| (*p).to_owned()).collect(),
    }
}

/// A snapshot issued at `T0` with the default tier bounds, users: clerk (tier 3), reader
/// (tier 2), payer (tier 1), sup (tier 3, may override), admin (not offline), gone (disabled).
pub fn snapshot(epoch: u64) -> Snapshot {
    snapshot_at(epoch, T0, [HOUR, 12 * HOUR, 24 * HOUR])
}

pub fn snapshot_at(epoch: u64, issued_at: u64, leases: [u64; 3]) -> Snapshot {
    let mut admin = dir("admin", Tier::One);
    admin.offline_allowed = false;
    let mut gone = dir("gone", Tier::Three);
    gone.enabled = false;
    Snapshot {
        tenant: tenant(),
        node: node(),
        epoch,
        issued_at,
        valid_until: TierDeadlines(leases.map(|l| issued_at + l)),
        grants: vec![
            grant("clerk", "a-clerk", &["invoice:read", "invoice:write"]),
            grant("sup", "a-sup", &[OVERRIDE_PERMISSION]),
            grant("payer", "a-payer", &["payment:post"]),
        ],
        directory: vec![
            dir("clerk", Tier::Three),
            dir("reader", Tier::Two),
            dir("payer", Tier::One),
            dir("sup", Tier::Three),
            admin,
            gone,
        ],
        sync_classes: vec![SyncClassEntry {
            group: "bank_details".to_owned(),
            class: SyncClass::Encrypted,
        }],
        key_epoch: 4,
        revocations: vec![],
    }
}
