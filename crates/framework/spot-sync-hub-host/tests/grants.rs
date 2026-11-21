//! The hub's grants host: lease requests answered from the library's service over a grant log
//! (never overlapping, idempotent by request id, refusals with their codes, audited), signed
//! snapshot requests answered against the replay ledger and verified by a real edge keeper, a
//! router that keeps tenants apart, and retirement accounting. The PostgreSQL grant log is
//! tested at the end (`SPOT_TEST_PG_URL`, skipped with a printed reason when unset).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

use ed25519_dalek::Signer as _;
use spot_audit_chain::AuditRecord;
use spot_bundle_signing::PublicKey;
use spot_edge_offline_host::carrier::{self, LeaseAnswerMsg, LeaseRequestMsg, SnapshotMsg};
use spot_edge_offline_host::carrier_loop::CarrierRecord;
use spot_edge_offline_host::identity::{NodeClock, SnapshotKeeper};
use spot_edge_offline_host::kinds::{LEASE_ANSWER_KIND, LEASE_REQUEST_KIND, SNAPSHOT_KIND};
use spot_lease_grants::{
    GrantService, LiveRegistryStanding, MemoryGrantLog, MemoryLedgerStore, ReplayGuard,
};
use spot_offline_identity::{
    AuditSink, AuditSinkError, DirectoryEntry, Grant, Message, NodeId as IdentityNodeId,
    NodeRegistry, SignedSnapshot, Signer, Snapshot, SyncClass, SyncClassEntry, TenantId, Tier,
    TierDeadlines, sign_delegation, sign_snapshot,
};
use spot_storage_contract::SeriesKey;
use spot_sync_hub_host::grants::{
    CarrierHandler, CarrierRouter, Grants, NodeKeys, NodeTenants, PolicySettings, ReplayIssuer,
    SnapshotSource, UsedMarks,
};
use spot_sync_protocol::NodeId;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use time::OffsetDateTime;

const NODE_A: &str = "edge-khi#branch_07";
const NODE_B: &str = "edge-lhe#branch_09";
const HOUR: u64 = 3_600_000;

fn at(offset_secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000 + offset_secs).unwrap()
}

fn hub_ms(offset_secs: i64) -> u64 {
    u64::try_from(at(offset_secs).unix_timestamp()).unwrap() * 1_000
}

fn node(s: &str) -> NodeId {
    NodeId::new(s).unwrap()
}

struct Key(ed25519_dalek::SigningKey);

impl Key {
    fn new(seed: u8) -> Self {
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

/// An audit sink the test can read, and make fail.
#[derive(Clone, Default)]
struct Audit {
    records: Arc<Mutex<Vec<AuditRecord>>>,
    fail: Arc<AtomicBool>,
}

impl AuditSink for Audit {
    fn record(&mut self, record: AuditRecord) -> Result<(), AuditSinkError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(AuditSinkError);
        }
        self.records.lock().unwrap().push(record);
        Ok(())
    }
}

impl Audit {
    fn kinds(&self) -> Vec<String> {
        self.records
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.kind.clone())
            .collect()
    }
}

fn registry(tenant: &str, nodes: &[&str]) -> Arc<Mutex<NodeRegistry>> {
    let mut registry = NodeRegistry::new(TenantId::new(tenant).unwrap());
    for n in nodes {
        registry.register(IdentityNodeId::new(n).unwrap());
    }
    Arc::new(Mutex::new(registry))
}

fn policy_for(nodes: &[&str]) -> spot_lease_grants::GrantPolicy {
    let settings = PolicySettings {
        default_max: Some(1_000),
        min_interval_secs: Some(0),
        entitle: nodes
            .iter()
            .map(|n| spot_sync_hub_host::grants::Entitlement {
                node: (*n).to_owned(),
                series: "INV".to_owned(),
            })
            .collect(),
        ..PolicySettings::default()
    };
    settings.build().unwrap()
}

struct Setup {
    grants: Grants,
    audit: Audit,
    registry: Arc<Mutex<NodeRegistry>>,
}

fn lease_grants(nodes: &[&str]) -> Setup {
    let registry = registry("acme", nodes);
    let audit = Audit::default();
    let service = GrantService::open(
        MemoryGrantLog::new(),
        LiveRegistryStanding::new(Arc::clone(&registry)),
        audit.clone(),
        policy_for(nodes),
    );
    Setup {
        grants: Grants::new(Box::new(service), None),
        audit,
        registry,
    }
}

fn lease_request(id: &str, series: &str, size: u64) -> CarrierRecord {
    CarrierRecord {
        kind: LEASE_REQUEST_KIND.to_owned(),
        payload: carrier::encode(&LeaseRequestMsg {
            request_id: id.to_owned(),
            series: series.to_owned(),
            size,
        })
        .unwrap(),
    }
}

fn answer(records: Vec<CarrierRecord>) -> LeaseAnswerMsg {
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, LEASE_ANSWER_KIND);
    carrier::decode(&records[0].payload).unwrap()
}

fn range_of(answer: &LeaseAnswerMsg) -> (u64, u64, u64) {
    match answer {
        LeaseAnswerMsg::Granted {
            from, to, lease_id, ..
        } => (*from, *to, *lease_id),
        LeaseAnswerMsg::Refused { code, .. } => panic!("refused: {code}"),
    }
}

fn code_of(answer: &LeaseAnswerMsg) -> String {
    match answer {
        LeaseAnswerMsg::Refused { code, .. } => code.clone(),
        LeaseAnswerMsg::Granted { .. } => panic!("granted"),
    }
}

#[test]
fn two_nodes_get_disjoint_ranges_and_every_grant_is_audited() {
    let s = lease_grants(&[NODE_A, NODE_B]);
    let a = range_of(&answer(s.grants.handle(
        &node(NODE_A),
        &lease_request("lease:INV@0", "INV", 1_000),
        at(0),
    )));
    let b = range_of(&answer(s.grants.handle(
        &node(NODE_B),
        &lease_request("lease:INV@0", "INV", 1_000),
        at(1),
    )));
    assert_eq!((a.0, a.1), (1, 1_000));
    assert_eq!((b.0, b.1), (1_001, 2_000));
    assert_ne!(a.2, b.2);
    assert_eq!(
        s.audit.kinds(),
        vec!["lease_grants.granted", "lease_grants.granted"]
    );
}

#[test]
fn a_repeated_request_id_gets_the_original_range_not_a_second_one() {
    let s = lease_grants(&[NODE_A]);
    let first = range_of(&answer(s.grants.handle(
        &node(NODE_A),
        &lease_request("lease:INV@0", "INV", 1_000),
        at(0),
    )));
    let again = range_of(&answer(s.grants.handle(
        &node(NODE_A),
        &lease_request("lease:INV@0", "INV", 1_000),
        at(5),
    )));
    assert_eq!(first, again);
    let next = range_of(&answer(s.grants.handle(
        &node(NODE_A),
        &lease_request("lease:INV@1000", "INV", 1_000),
        at(10),
    )));
    assert_eq!((next.0, next.1), (1_001, 2_000));
    assert_eq!(
        s.audit.kinds(),
        vec![
            "lease_grants.granted",
            "lease_grants.replayed",
            "lease_grants.granted"
        ]
    );
}

#[test]
fn refusals_carry_their_stable_codes() {
    let s = lease_grants(&[NODE_A]);
    // Not entitled to the series.
    let refused = answer(
        s.grants
            .handle(&node(NODE_A), &lease_request("r1", "PO", 10), at(0)),
    );
    assert_eq!(code_of(&refused), "lease_grants.series_not_entitled");
    // A size above the maximum.
    let refused = answer(
        s.grants
            .handle(&node(NODE_A), &lease_request("r2", "INV", 5_000), at(1)),
    );
    assert_eq!(code_of(&refused), "lease_grants.size_refused");
    // A node the registry does not know.
    let refused = answer(s.grants.handle(
        &node("edge-xx#unknown"),
        &lease_request("r3", "INV", 10),
        at(2),
    ));
    assert_eq!(code_of(&refused), "lease_grants.node_not_active");
    // A suspended node.
    s.registry
        .lock()
        .unwrap()
        .suspend(&IdentityNodeId::new(NODE_A).unwrap(), 1)
        .unwrap();
    let refused = answer(
        s.grants
            .handle(&node(NODE_A), &lease_request("r4", "INV", 10), at(3)),
    );
    assert_eq!(code_of(&refused), "lease_grants.node_not_active");
}

#[test]
fn an_unreadable_payload_gets_no_answer_and_is_counted() {
    let s = lease_grants(&[NODE_A]);
    let junk = CarrierRecord {
        kind: LEASE_REQUEST_KIND.to_owned(),
        payload: "not json".to_owned(),
    };
    assert!(s.grants.handle(&node(NODE_A), &junk, at(0)).is_empty());
    assert_eq!(s.grants.ignored(), 1);
    // An empty request id cannot be made idempotent: refused, not granted a fresh range.
    let refused = answer(
        s.grants
            .handle(&node(NODE_A), &lease_request("", "INV", 10), at(1)),
    );
    assert_eq!(code_of(&refused), "edge_offline.bad_message");
    // A kind the hub does not take is ignored silently.
    let other = CarrierRecord {
        kind: "sys_other".to_owned(),
        payload: String::new(),
    };
    assert!(s.grants.handle(&node(NODE_A), &other, at(2)).is_empty());
}

#[test]
fn a_failed_audit_withholds_the_grant() {
    let s = lease_grants(&[NODE_A]);
    s.audit.fail.store(true, Ordering::SeqCst);
    let refused = answer(
        s.grants
            .handle(&node(NODE_A), &lease_request("r1", "INV", 10), at(0)),
    );
    assert_eq!(code_of(&refused), "lease_grants.audit_failed");
}

struct Marks(BTreeMap<String, u64>);

impl UsedMarks for Marks {
    fn highest_used(&self, series: &SeriesKey, _node: &NodeId) -> Option<u64> {
        self.0.get(series.as_str()).copied()
    }
}

#[test]
fn retirement_accounts_the_unused_remainder_and_audits_it() {
    let s = lease_grants(&[NODE_A]);
    s.grants.handle(
        &node(NODE_A),
        &lease_request("lease:INV@0", "INV", 1_000),
        at(0),
    );
    let used = Marks(BTreeMap::from([("INV".to_owned(), 300)]));
    let released = s.grants.retire_node(&node(NODE_A), &used, at(100)).unwrap();
    assert_eq!(released.len(), 1);
    assert_eq!((released[0].from, released[0].to), (301, 1_000));
    assert!(
        s.audit
            .kinds()
            .contains(&"lease_grants.range_released".to_owned())
    );
    // Nothing seen used: the whole range is unused; everything used: nothing to account.
    let none = Marks(BTreeMap::new());
    assert_eq!(
        s.grants.retire_node(&node(NODE_A), &none, at(101)).unwrap()[0].from,
        1
    );
    let all = Marks(BTreeMap::from([("INV".to_owned(), 1_000)]));
    assert!(
        s.grants
            .retire_node(&node(NODE_A), &all, at(102))
            .unwrap()
            .is_empty()
    );
    // A node with no grants has nothing to account.
    assert!(
        s.grants
            .retire_node(&node(NODE_B), &none, at(103))
            .unwrap()
            .is_empty()
    );
    // A failing audit withholds the report.
    s.audit.fail.store(true, Ordering::SeqCst);
    assert_eq!(
        s.grants
            .retire_node(&node(NODE_A), &none, at(104))
            .unwrap_err(),
        "lease_grants.audit_failed"
    );
}

// ---------------------------------------------------------------------------------------
// Snapshots, with a real edge keeper on the other end
// ---------------------------------------------------------------------------------------

struct Keys(BTreeMap<String, PublicKey>);

impl NodeKeys for Keys {
    fn key(&self, node: &IdentityNodeId) -> Option<PublicKey> {
        self.0.get(node.as_str()).copied()
    }
}

struct Source {
    tenant: TenantId,
    snapshot: SignedSnapshot,
}

impl SnapshotSource for Source {
    fn current(&self, tenant: &TenantId, node: &IdentityNodeId) -> Option<SignedSnapshot> {
        (tenant == &self.tenant && node == &self.snapshot.snapshot.node)
            .then(|| self.snapshot.clone())
    }
}

struct World {
    root: Key,
    snap: Key,
    ts: Key,
    node_key: Key,
}

impl World {
    fn new() -> Self {
        Self {
            root: Key::new(1),
            snap: Key::new(2),
            ts: Key::new(3),
            node_key: Key::new(4),
        }
    }

    fn delegation(&self, tenant: &str) -> spot_offline_identity::SignedDelegation {
        sign_delegation(
            &self.root,
            &TenantId::new(tenant).unwrap(),
            1,
            &self.snap.public_key(),
            &self.ts.public_key(),
        )
    }

    fn snapshot(&self, tenant: &str, node: &str, epoch: u64, issued_at: u64) -> SignedSnapshot {
        let body = Snapshot {
            tenant: TenantId::new(tenant).unwrap(),
            node: IdentityNodeId::new(node).unwrap(),
            epoch,
            issued_at,
            valid_until: TierDeadlines([
                issued_at + HOUR,
                issued_at + 12 * HOUR,
                issued_at + 24 * HOUR,
            ]),
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
        };
        sign_snapshot(body, &self.snap, self.delegation(tenant))
    }

    fn issuer(&self, tenant: &str, snapshot: SignedSnapshot, audit: &Audit) -> Grants {
        let guard = ReplayGuard::new(
            TenantId::new(tenant).unwrap(),
            MemoryLedgerStore::new(),
            audit.clone(),
        );
        let issuer = ReplayIssuer::new(
            TenantId::new(tenant).unwrap(),
            guard,
            Arc::new(Keys(BTreeMap::from([(
                NODE_A.to_owned(),
                self.node_key.public_key(),
            )]))),
            Arc::new(Source {
                tenant: TenantId::new(tenant).unwrap(),
                snapshot,
            }),
            Box::new(Key::new(3)),
            self.delegation(tenant),
        );
        let service = GrantService::open(
            MemoryGrantLog::new(),
            LiveRegistryStanding::new(registry(tenant, &[NODE_A])),
            audit.clone(),
            policy_for(&[NODE_A]),
        );
        Grants::new(Box::new(service), Some(Box::new(issuer)))
    }

    fn keeper(&self, tenant: &str) -> SnapshotKeeper<spot_offline_identity::MemoryPersistence> {
        SnapshotKeeper::open(
            spot_offline_identity::TrustAnchor {
                tenant: TenantId::new(tenant).unwrap(),
                node: IdentityNodeId::new(NODE_A).unwrap(),
                root: self.root.public_key(),
            },
            spot_offline_identity::MemoryPersistence::default(),
            NodeClock {
                mono_ms: 0,
                boot_id: 1,
            },
        )
        .with_node_key(Arc::new(Key::new(4)))
    }
}

fn snapshot_record(request: &SnapshotMsg) -> CarrierRecord {
    CarrierRecord {
        kind: SNAPSHOT_KIND.to_owned(),
        payload: carrier::encode(request).unwrap(),
    }
}

fn snapshot_answer(records: Vec<CarrierRecord>) -> SnapshotMsg {
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, SNAPSHOT_KIND);
    carrier::decode(&records[0].payload).unwrap()
}

#[test]
fn a_snapshot_request_is_answered_and_the_edge_keeper_accepts_it() {
    let world = World::new();
    let audit = Audit::default();
    let grants = world.issuer(
        "acme",
        world.snapshot("acme", NODE_A, 1, hub_ms(-60)),
        &audit,
    );
    let mut keeper = world.keeper("acme");
    let request = keeper.request(keeper.next_request_id()).unwrap();

    let SnapshotMsg::Issued {
        message, companion, ..
    } = snapshot_answer(grants.handle(&node(NODE_A), &snapshot_record(&request), at(0)))
    else {
        panic!("not issued");
    };
    assert!(matches!(message, Message::Snapshot(_)));
    let time = companion.expect("a snapshot comes with signed hub time");
    let now = NodeClock {
        mono_ms: 10,
        boot_id: 1,
    };
    keeper.accept(&message, now).unwrap();
    keeper.accept(&time, now).unwrap();
    assert_eq!(keeper.store().highest_epoch(), 1);

    // The node now holds the current snapshot: the next request is answered with a timestamp.
    let later = keeper.request(keeper.next_request_id()).unwrap();
    let SnapshotMsg::Issued {
        message, companion, ..
    } = snapshot_answer(grants.handle(&node(NODE_A), &snapshot_record(&later), at(600)))
    else {
        panic!("not issued");
    };
    assert!(matches!(message, Message::Timestamp(_)));
    assert!(companion.is_none());
    keeper
        .accept(
            &message,
            NodeClock {
                mono_ms: 600_000,
                boot_id: 1,
            },
        )
        .unwrap();
}

#[test]
fn a_request_for_another_nodes_identity_or_signed_by_another_key_is_refused() {
    let world = World::new();
    let audit = Audit::default();
    let grants = world.issuer(
        "acme",
        world.snapshot("acme", NODE_A, 1, hub_ms(-60)),
        &audit,
    );
    let keeper = world.keeper("acme");
    let request = keeper.request("snapshot:0@0").unwrap();

    // The connection authenticated another node: the request names NODE_A, so it is refused.
    let refused = snapshot_answer(grants.handle(&node(NODE_B), &snapshot_record(&request), at(0)));
    let SnapshotMsg::Refused { code, .. } = refused else {
        panic!("not refused")
    };
    assert_eq!(code, "lease_grants.node_mismatch");

    // A request signed with a key that is not the node's.
    let forger = keeper.with_node_key(Arc::new(Key::new(99)));
    let forged = forger.request("snapshot:0@0").unwrap();
    let SnapshotMsg::Refused { code, .. } =
        snapshot_answer(grants.handle(&node(NODE_A), &snapshot_record(&forged), at(1)))
    else {
        panic!("not refused")
    };
    assert_eq!(code, "offline_identity.request_bad_signature");
}

#[test]
fn a_node_that_reports_less_than_it_was_given_is_refused() {
    let world = World::new();
    let audit = Audit::default();
    let grants = world.issuer(
        "acme",
        world.snapshot("acme", NODE_A, 3, hub_ms(-60)),
        &audit,
    );
    let mut keeper = world.keeper("acme");
    let request = keeper.request(keeper.next_request_id()).unwrap();
    let SnapshotMsg::Issued {
        message, companion, ..
    } = snapshot_answer(grants.handle(&node(NODE_A), &snapshot_record(&request), at(0)))
    else {
        panic!("not issued");
    };
    let now = NodeClock {
        mono_ms: 10,
        boot_id: 1,
    };
    keeper.accept(&message, now).unwrap();
    keeper.accept(&companion.unwrap(), now).unwrap();

    // A copy of the node from before it held anything (a restored old disk image) asks again.
    let restored = world.keeper("acme");
    let old = restored.request("snapshot:0@0").unwrap();
    let SnapshotMsg::Refused { code, .. } =
        snapshot_answer(grants.handle(&node(NODE_A), &snapshot_record(&old), at(60)))
    else {
        panic!("not refused")
    };
    assert_eq!(code, "lease_grants.node_regressed");
    // The flagged node gets no range lease either: the standing is the registry's, but the
    // flag is the guard's, so a later snapshot request is refused while flagged.
    let again = restored.request("snapshot:0@1").unwrap();
    let SnapshotMsg::Refused { code, .. } =
        snapshot_answer(grants.handle(&node(NODE_A), &snapshot_record(&again), at(120)))
    else {
        panic!("not refused")
    };
    assert_eq!(code, "lease_grants.node_regressed");
}

#[test]
fn tenants_are_kept_apart_by_the_router() {
    struct Tenants;
    impl NodeTenants for Tenants {
        fn tenant_of(&self, n: &NodeId) -> Option<String> {
            match n.as_str() {
                NODE_A => Some("acme".to_owned()),
                NODE_B => Some("globex".to_owned()),
                _ => None,
            }
        }
    }
    let acme = lease_grants(&[NODE_A]);
    let globex = lease_grants(&[NODE_B]);
    let router = CarrierRouter::new(
        BTreeMap::from([
            ("acme".to_owned(), Arc::new(acme.grants)),
            ("globex".to_owned(), Arc::new(globex.grants)),
        ]),
        Arc::new(Tenants),
    );
    // Each tenant's allocator starts at 1: the ranges are per tenant.
    let a = range_of(&answer(router.handle(
        &node(NODE_A),
        &lease_request("lease:INV@0", "INV", 1_000),
        at(0),
    )));
    let b = range_of(&answer(router.handle(
        &node(NODE_B),
        &lease_request("lease:INV@0", "INV", 1_000),
        at(0),
    )));
    assert_eq!((a.0, b.0), (1, 1));
    // Node A is only known to acme: acme's grants refuse NODE_B, and globex's refuse NODE_A.
    assert_eq!(
        acme.registry
            .lock()
            .unwrap()
            .state(&IdentityNodeId::new(NODE_B).unwrap()),
        None
    );
    // A node the hub does not know in any tenant gets no answer at all.
    assert!(
        router
            .handle(
                &node("edge-zz#stranger"),
                &lease_request("x", "INV", 10),
                at(1)
            )
            .is_empty()
    );
}

// ---------------------------------------------------------------------------------------
// The PostgreSQL grant log
// ---------------------------------------------------------------------------------------

mod pg {
    use std::time::Duration;

    use spot_lease_grants::{GrantLog, GrantRecord, RequestId};
    use spot_partition::{LeaseId, RangeLease};
    use spot_pg_bridge::{Bridge, BridgeConfig};
    use spot_postgres::testing::TestDb;
    use spot_postgres::{PgBackend, PgConfig};
    use spot_sync_hub_host::grants::{DDL, PgGrantLog, STATEMENTS};

    use super::*;

    struct Fixture {
        rt: tokio::runtime::Runtime,
        db: TestDb,
    }

    fn fixture() -> Option<Fixture> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let db = rt.block_on(TestDb::create())?;
        Some(Fixture { rt, db })
    }

    fn bridge(config: PgConfig) -> Arc<Bridge> {
        let mut config = config;
        config.pool_max_size = 6;
        let backend = PgBackend::connect_unchecked(config).expect("backend");
        Arc::new(
            Bridge::for_backend(
                BridgeConfig {
                    name: "grants_test".to_owned(),
                    workers: 2,
                    max_in_flight: 4,
                    queue_capacity: 64,
                    default_deadline: Duration::from_secs(10),
                    max_deadline: Duration::from_secs(30),
                },
                &backend,
            )
            .expect("bridge"),
        )
    }

    fn record(from: u64, to: u64, id: u64, node_name: &str, request: &str) -> GrantRecord {
        let series = SeriesKey::new("INV").unwrap();
        GrantRecord {
            lease: RangeLease::new(series, from, to, node(node_name), LeaseId::new(id)).unwrap(),
            node: node(node_name),
            requested_size: to - from + 1,
            granted_at: at(i64::try_from(id).unwrap()),
            request_id: RequestId::new(request),
        }
    }

    #[test]
    fn every_statement_carries_the_tenant() {
        for sql in STATEMENTS {
            if !sql.contains("to_regclass") {
                assert!(
                    sql.contains("tenant = $1") || sql.trim_start().starts_with("INSERT"),
                    "{sql}"
                );
            }
        }
        assert!(DDL.starts_with("CREATE TABLE IF NOT EXISTS spot_lease_grant_log"));
    }

    #[test]
    fn a_restart_keeps_every_grant_and_the_service_continues_above() {
        let Some(f) = fixture() else {
            eprintln!("skipped: SPOT_TEST_PG_URL is not set");
            return;
        };
        let bridge = bridge(f.db.config());
        let missing = PgGrantLog::open(Arc::clone(&bridge), "acme").err().unwrap();
        assert!(missing.contains("spot migrate"));
        f.rt.block_on(async {
            f.db.raw_client().await.batch_execute(DDL).await.unwrap();
        });

        // First life: grant through the real service over the PostgreSQL log.
        let registry = registry("acme", &[NODE_A]);
        let service = GrantService::open(
            PgGrantLog::open(Arc::clone(&bridge), "acme").unwrap(),
            LiveRegistryStanding::new(Arc::clone(&registry)),
            Audit::default(),
            policy_for(&[NODE_A]),
        );
        let grants = Grants::new(Box::new(service), None);
        let first = range_of(&answer(grants.handle(
            &node(NODE_A),
            &lease_request("lease:INV@0", "INV", 1_000),
            at(0),
        )));
        assert_eq!((first.0, first.1), (1, 1_000));
        drop(grants);

        // The hub restarts: the service is rebuilt from the log. A re-request (the lost answer)
        // gets the original range; a new request continues above it.
        let service = GrantService::open(
            PgGrantLog::open(Arc::clone(&bridge), "acme").unwrap(),
            LiveRegistryStanding::new(registry),
            Audit::default(),
            policy_for(&[NODE_A]),
        );
        let grants = Grants::new(Box::new(service), None);
        let again = range_of(&answer(grants.handle(
            &node(NODE_A),
            &lease_request("lease:INV@0", "INV", 1_000),
            at(10),
        )));
        assert_eq!(again, first);
        let next = range_of(&answer(grants.handle(
            &node(NODE_A),
            &lease_request("lease:INV@1000", "INV", 1_000),
            at(20),
        )));
        assert_eq!((next.0, next.1), (1_001, 2_000));
    }

    #[test]
    fn the_database_refuses_an_overlap_even_from_a_stale_process() {
        let Some(f) = fixture() else {
            eprintln!("skipped: SPOT_TEST_PG_URL is not set");
            return;
        };
        let bridge = bridge(f.db.config());
        f.rt.block_on(async {
            f.db.raw_client().await.batch_execute(DDL).await.unwrap();
        });
        let mut one = PgGrantLog::open(Arc::clone(&bridge), "acme").unwrap();
        let mut two = PgGrantLog::open(Arc::clone(&bridge), "acme").unwrap();
        one.append(record(1, 1_000, 1, NODE_A, "r1")).unwrap();
        // `two` read the empty log before `one` appended: its cache is stale, the database is not.
        assert!(two.append(record(1, 1_000, 2, NODE_B, "r2")).is_err());
        assert!(two.append(record(500, 1_500, 3, NODE_B, "r3")).is_err());
        two.append(record(1_001, 2_000, 4, NODE_B, "r4")).unwrap();
        // A reused lease id is refused.
        assert!(two.append(record(2_001, 3_000, 4, NODE_B, "r5")).is_err());
        // Another tenant has its own cursor and ids.
        let mut globex = PgGrantLog::open(bridge, "globex").unwrap();
        globex.append(record(1, 1_000, 1, NODE_A, "g1")).unwrap();
        assert_eq!(globex.cursor(&SeriesKey::new("INV").unwrap()), Some(1_000));
        assert_eq!(one.all().len(), 1);
    }
}
