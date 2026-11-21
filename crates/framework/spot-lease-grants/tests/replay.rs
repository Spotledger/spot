//! The hub half of offline-identity replay detection: a strictly increasing nonce per node, the
//! node's report compared with what the hub issued, a snapshot paired with signed hub time,
//! a flag that only re-enrolment clears, and no range lease for a flagged node.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use ed25519_dalek::Signer as _;
use spot_audit_chain::PayloadValue;
use spot_bundle_signing::PublicKey;
use spot_lease_grants::{
    Admitted, GrantError, GrantPolicy, GrantService, KIND_NODE_REENROLLED, KIND_NODE_REGRESSED,
    KIND_NODE_REPORT_AHEAD, LeaseRequest, LedgerStore, LiveRegistryStanding, MemoryGrantLog,
    MemoryLedgerStore, NodeLedger, ReenrolmentStanding, ReplayGuard, ReplayRefusal, Standing,
    StandingPort, TransportNode, ledger_contract,
};
use spot_offline_identity::signing::{
    NodeReport, RequestKind, RequestRefusal, SignedNodeRequest, sign_node_request,
};
use spot_offline_identity::{
    MAX_CERT_TTL_MS, MemoryAuditSink, MemoryPersistence, Message, NodeClock, NodeId, NodeRegistry,
    Received, Signer, Snapshot, SnapshotStore, TenantId, TierDeadlines, TrustAnchor,
    sign_delegation, sign_snapshot,
};
use spot_storage_contract::SeriesKey;
use time::OffsetDateTime;

const NODE: &str = "edge-khi#branch_07";
const OTHER: &str = "edge-lhe#branch_09";
const T0: u64 = 1_000_000_000_000;
const HOUR: u64 = 3_600_000;

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

fn tenant() -> TenantId {
    TenantId::new("acme").unwrap()
}

fn id(s: &str) -> NodeId {
    NodeId::new(s).unwrap()
}

fn at() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

fn transport(s: &str) -> TransportNode {
    TransportNode::authenticated(spot_sync_protocol::NodeId::new(s).unwrap())
}

/// The keys of the world: an offline root, the online snapshot and timestamp keys, the node's
/// own key.
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
            node_key: Key::new(9),
        }
    }

    fn delegation(&self) -> spot_offline_identity::SignedDelegation {
        sign_delegation(
            &self.root,
            &tenant(),
            1,
            &self.snap.public_key(),
            &self.ts.public_key(),
        )
    }

    fn snapshot(&self, node: &str, epoch: u64, tag: &str) -> spot_offline_identity::SignedSnapshot {
        let mut snapshot = Snapshot {
            tenant: tenant(),
            node: id(node),
            epoch,
            issued_at: T0 + epoch,
            valid_until: TierDeadlines([T0 + HOUR; 3]),
            grants: Vec::new(),
            directory: Vec::new(),
            sync_classes: Vec::new(),
            key_epoch: 1,
            revocations: Vec::new(),
        };
        // `tag` changes the content, so two snapshots of one epoch can differ (a fork).
        snapshot.key_epoch += u64::try_from(tag.len()).unwrap();
        sign_snapshot(snapshot, &self.snap, self.delegation())
    }

    fn node_store(&self, node: &str) -> SnapshotStore<MemoryPersistence> {
        let mut store = SnapshotStore::open(
            TrustAnchor {
                tenant: tenant(),
                node: id(node),
                root: self.root.public_key(),
            },
            MemoryPersistence::default(),
            clock(0, 1),
        );
        store
            .renew_certificate(1, MAX_CERT_TTL_MS, true, clock(0, 1))
            .unwrap();
        store
    }

    fn request(&self, report: NodeReport, kind: RequestKind, node: &str) -> SignedNodeRequest {
        sign_node_request(
            spot_offline_identity::signing::NodeRequest {
                tenant: tenant(),
                node: id(node),
                kind,
                report: Some(report),
            },
            &self.node_key,
        )
    }
}

fn clock(mono_ms: u64, boot_id: u64) -> NodeClock {
    NodeClock { mono_ms, boot_id }
}

fn rx(mono_ms: u64, boot_id: u64) -> Received {
    Received {
        clock: clock(mono_ms, boot_id),
    }
}

type Guard = ReplayGuard<MemoryLedgerStore, MemoryAuditSink>;

fn guard() -> Guard {
    ReplayGuard::new(
        tenant(),
        MemoryLedgerStore::new(),
        MemoryAuditSink::default(),
    )
}

/// One round: the node asks for a snapshot, the hub answers with `snapshot` paired with time.
fn snapshot_round(
    w: &World,
    g: &mut Guard,
    store: &mut SnapshotStore<MemoryPersistence>,
    snapshot: spot_offline_identity::SignedSnapshot,
    hub_ms: u64,
    mono: u64,
    boot: u64,
) -> Result<spot_lease_grants::SnapshotReply, ReplayRefusal> {
    let request = w.request(
        store.report(),
        RequestKind::Snapshot,
        snapshot.snapshot.node.as_str(),
    );
    let admitted = g.admit(
        &transport(snapshot.snapshot.node.as_str()),
        &w.node_key.public_key(),
        &request,
        at(),
    )?;
    let reply = g.reply_snapshot(admitted, snapshot, &w.ts, hub_ms)?;
    store
        .accept(&Message::Snapshot(reply.snapshot.clone()), rx(mono, boot))
        .unwrap();
    store
        .accept(&Message::HubTime(reply.time.clone()), rx(mono, boot))
        .unwrap();
    Ok(reply)
}

fn admit(
    w: &World,
    g: &mut Guard,
    report: NodeReport,
    kind: RequestKind,
) -> Result<Admitted, ReplayRefusal> {
    let request = w.request(report, kind, NODE);
    g.admit(&transport(NODE), &w.node_key.public_key(), &request, at())
}

fn report(epoch: u64, hash: Option<[u8; 32]>, attested: u64, nonce: u64) -> NodeReport {
    NodeReport {
        highest_epoch: epoch,
        snapshot_hash: hash,
        last_attested: attested,
        hub_nonce: nonce,
    }
}

fn kinds(g: &Guard) -> Vec<&str> {
    g.audit().records.iter().map(|r| r.kind.as_str()).collect()
}

#[test]
fn nonces_rise_by_one_per_reply_for_each_node_separately() {
    let w = World::new();
    let mut g = guard();
    let mut last = 0;
    for n in 1..=4 {
        let admitted = admit(&w, &mut g, report(0, None, 0, last), RequestKind::HubTime).unwrap();
        let time = g
            .reply_hub_time(admitted, &w.ts, w.delegation(), T0 + n * 10)
            .unwrap();
        assert_eq!(time.time.nonce, n);
        last = time.time.nonce;
    }
    // Another node starts at one: the counter is per node.
    let request = w.request(report(0, None, 0, 0), RequestKind::HubTime, OTHER);
    let admitted = g
        .admit(&transport(OTHER), &w.node_key.public_key(), &request, at())
        .unwrap();
    let time = g
        .reply_hub_time(admitted, &w.ts, w.delegation(), T0 + 1)
        .unwrap();
    assert_eq!(time.time.nonce, 1);
}

#[test]
fn an_honest_sequence_of_epochs_is_accepted_and_the_node_takes_every_reply() {
    let w = World::new();
    let mut g = guard();
    let mut store = w.node_store(NODE);
    let mut nonces = Vec::new();
    for epoch in 1..=3 {
        let snapshot = w.snapshot(NODE, epoch, "");
        let reply = snapshot_round(&w, &mut g, &mut store, snapshot, T0 + epoch * 100, epoch, 1)
            .unwrap_or_else(|e| panic!("epoch {epoch}: {e}"));
        assert_eq!(reply.time.time.epoch, epoch);
        assert_eq!(reply.time.time.hub_ms, T0 + epoch * 100);
        nonces.push(reply.time.time.nonce);
    }
    assert_eq!(nonces, [1, 2, 3]);
    let ledger = g.ledger().get(&id(NODE)).unwrap();
    assert_eq!(ledger.acked_epoch, 3);
    assert_eq!(
        ledger.acked_hash,
        Some(w.snapshot(NODE, 3, "").snapshot.hash())
    );
    assert!(!ledger.requires_reenrolment);
    assert!(
        g.audit().records.is_empty(),
        "an honest node writes no refusal"
    );
}

#[test]
fn a_node_that_reports_n_minus_one_after_n_is_refused_audited_and_stays_refused() {
    let w = World::new();
    let mut g = guard();
    let mut store = w.node_store(NODE);
    snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 1, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    // The attacker keeps the epoch-1 disk image.
    let epoch_one_image = store.into_persistence();
    let mut store = SnapshotStore::open(
        TrustAnchor {
            tenant: tenant(),
            node: id(NODE),
            root: w.root.public_key(),
        },
        epoch_one_image.clone(),
        clock(0, 2),
    );
    snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 2, ""),
        T0 + 200,
        5,
        2,
    )
    .unwrap();

    // The node is switched off and the epoch-1 image is put back.
    let replayed = SnapshotStore::open(
        TrustAnchor {
            tenant: tenant(),
            node: id(NODE),
            root: w.root.public_key(),
        },
        epoch_one_image,
        clock(0, 3),
    );
    let stale = w.request(replayed.report(), RequestKind::HubTime, NODE);
    let refusal = g
        .admit(&transport(NODE), &w.node_key.public_key(), &stale, at())
        .unwrap_err();
    assert_eq!(refusal, ReplayRefusal::NodeRegressed);
    assert_eq!(refusal.code(), "lease_grants.node_regressed");
    assert!(g.requires_reenrolment(&id(NODE)));
    assert_eq!(kinds(&g), [KIND_NODE_REGRESSED]);

    // The record names the node and both sides, and nothing secret.
    let record = &g.audit().records[0];
    assert_eq!(record.kind, "offline_identity.node_regressed");
    assert_eq!(record.payload["node"], PayloadValue::Text(NODE.to_owned()));
    assert_eq!(record.payload["acknowledged_epoch"], PayloadValue::Int(2));
    assert_eq!(record.payload["reported_epoch"], PayloadValue::Int(1));
    assert_eq!(
        record.payload["acknowledged_hash"],
        PayloadValue::Text(hex(w.snapshot(NODE, 2, "").snapshot.hash()))
    );
    assert_eq!(
        record.payload["reported_hash"],
        PayloadValue::Text(hex(w.snapshot(NODE, 1, "").snapshot.hash()))
    );
    let shown = format!("{record:?}");
    for key in [&w.node_key, &w.ts, &w.snap, &w.root] {
        assert!(
            !shown.contains(&hex(key.0.to_bytes())),
            "key bytes in audit"
        );
        assert!(!shown.contains(&format!("{:?}", key.0.to_bytes())));
    }
    for word in ["secret", "private", "password", "signature", "-----begin"] {
        assert!(!shown.to_lowercase().contains(word), "`{word}` in audit");
    }

    // A later honest request from the same node is still refused, with no new audit record.
    let honest = w.request(
        report(
            2,
            Some(w.snapshot(NODE, 2, "").snapshot.hash()),
            T0 + 200,
            2,
        ),
        RequestKind::Snapshot,
        NODE,
    );
    assert_eq!(
        g.admit(&transport(NODE), &w.node_key.public_key(), &honest, at())
            .unwrap_err(),
        ReplayRefusal::NodeRegressed
    );
    assert_eq!(g.refused_while_flagged(), 1);
    assert_eq!(kinds(&g).len(), 1);
}

#[test]
fn the_same_epoch_with_another_hash_is_refused() {
    let w = World::new();
    let mut g = guard();
    let mut store = w.node_store(NODE);
    snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 1, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    let forked = w.snapshot(NODE, 1, "fork").snapshot.hash();
    assert_ne!(forked, w.snapshot(NODE, 1, "").snapshot.hash());
    let refusal = admit(
        &w,
        &mut g,
        report(1, Some(forked), T0 + 100, 1),
        RequestKind::HubTime,
    )
    .unwrap_err();
    assert_eq!(refusal, ReplayRefusal::NodeRegressed);
    assert!(g.requires_reenrolment(&id(NODE)));
    // A node that says it holds no snapshot at an epoch the hub acknowledged is the same.
    let mut g = guard();
    let mut store = w.node_store(NODE);
    snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 1, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    assert_eq!(
        admit(
            &w,
            &mut g,
            report(1, None, T0 + 100, 1),
            RequestKind::HubTime
        )
        .unwrap_err(),
        ReplayRefusal::NodeRegressed
    );
}

#[test]
fn an_older_nonce_or_hub_clock_than_the_node_reported_before_is_a_replay() {
    let w = World::new();
    let mut g = guard();
    let mut store = w.node_store(NODE);
    let first = snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 1, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    let hash = Some(first.snapshot.snapshot.hash());
    // Two time replies; the node takes both and reports the second.
    for (n, ms) in [(2_u64, T0 + 200), (3, T0 + 300)] {
        let admitted = admit(
            &w,
            &mut g,
            report(1, hash, ms - 100, n - 1),
            RequestKind::HubTime,
        )
        .unwrap();
        let time = g
            .reply_hub_time(admitted, &w.ts, w.delegation(), ms)
            .unwrap();
        assert_eq!(time.time.nonce, n);
    }
    // The node reports nonce 3 / clock T0+300 (it took everything).
    admit(
        &w,
        &mut g,
        report(1, hash, T0 + 300, 3),
        RequestKind::HubTime,
    )
    .unwrap();
    // An older nonce than it reported, though still below what the hub issued: refused.
    assert_eq!(
        admit(
            &w,
            &mut g,
            report(1, hash, T0 + 300, 2),
            RequestKind::HubTime
        )
        .unwrap_err(),
        ReplayRefusal::NodeRegressed
    );

    // The clock alone going back is refused too.
    let mut g = guard();
    let mut store = w.node_store(NODE);
    snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 1, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    admit(
        &w,
        &mut g,
        report(1, hash, T0 + 100, 1),
        RequestKind::HubTime,
    )
    .unwrap();
    assert_eq!(
        admit(
            &w,
            &mut g,
            report(1, hash, T0 + 99, 1),
            RequestKind::HubTime
        )
        .unwrap_err(),
        ReplayRefusal::NodeRegressed
    );
}

#[test]
fn a_reply_lost_on_the_way_back_is_not_a_replay() {
    let w = World::new();
    let mut g = guard();
    let mut store = w.node_store(NODE);
    let first = snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 1, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    let hash = Some(first.snapshot.snapshot.hash());
    // The hub issues nonce 2; the node never gets it and asks again with nonce 1.
    let admitted = admit(
        &w,
        &mut g,
        report(1, hash, T0 + 100, 1),
        RequestKind::HubTime,
    )
    .unwrap();
    g.reply_hub_time(admitted, &w.ts, w.delegation(), T0 + 200)
        .unwrap();
    let again = admit(
        &w,
        &mut g,
        report(1, hash, T0 + 100, 1),
        RequestKind::HubTime,
    )
    .unwrap();
    let time = g
        .reply_hub_time(again, &w.ts, w.delegation(), T0 + 300)
        .unwrap();
    assert_eq!(time.time.nonce, 3, "the burnt nonce is never reused");
    assert!(!g.requires_reenrolment(&id(NODE)));
}

#[test]
fn a_report_the_hub_never_issued_is_refused_and_audited_without_a_flag() {
    let w = World::new();
    let mut g = guard();
    let mut store = w.node_store(NODE);
    let first = snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 1, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    let hash = Some(first.snapshot.snapshot.hash());
    for ahead in [
        report(2, hash, T0 + 100, 1),
        report(1, hash, T0 + 100, 2),
        report(1, hash, T0 + 101, 1),
    ] {
        assert_eq!(
            admit(&w, &mut g, ahead, RequestKind::HubTime).unwrap_err(),
            ReplayRefusal::ReportAhead
        );
    }
    assert!(!g.requires_reenrolment(&id(NODE)));
    assert_eq!(kinds(&g), [KIND_NODE_REPORT_AHEAD; 3]);
    // The honest report still works.
    admit(
        &w,
        &mut g,
        report(1, hash, T0 + 100, 1),
        RequestKind::HubTime,
    )
    .unwrap();
}

#[test]
fn a_request_must_match_the_connection_the_tenant_and_its_signature() {
    let w = World::new();
    let mut g = guard();
    let key = w.node_key.public_key();
    let good = w.request(report(0, None, 0, 0), RequestKind::HubTime, NODE);

    // Another node's connection.
    assert_eq!(
        g.admit(&transport(OTHER), &key, &good, at()).unwrap_err(),
        ReplayRefusal::NodeMismatch
    );
    // Another tenant.
    let mut other_tenant = good.clone();
    other_tenant.request.tenant = TenantId::new("rival").unwrap();
    assert_eq!(
        g.admit(&transport(NODE), &key, &other_tenant, at())
            .unwrap_err(),
        ReplayRefusal::NodeMismatch
    );
    // A different key.
    assert_eq!(
        g.admit(&transport(NODE), &Key::new(77).public_key(), &good, at())
            .unwrap_err(),
        ReplayRefusal::Request(RequestRefusal::BadSignature)
    );
    // An altered report.
    let mut altered = good.clone();
    altered.request.report = Some(report(0, None, 0, 5));
    assert_eq!(
        g.admit(&transport(NODE), &key, &altered, at()).unwrap_err(),
        ReplayRefusal::Request(RequestRefusal::BadSignature)
    );
    // No report (signed as such).
    let missing = sign_node_request(
        spot_offline_identity::signing::NodeRequest {
            tenant: tenant(),
            node: id(NODE),
            kind: RequestKind::HubTime,
            report: None,
        },
        &w.node_key,
    );
    assert_eq!(
        g.admit(&transport(NODE), &key, &missing, at()).unwrap_err(),
        ReplayRefusal::Request(RequestRefusal::ReportMissing)
    );
    // None of that touched the ledger.
    assert_eq!(g.ledger().get(&id(NODE)).unwrap(), NodeLedger::default());
}

#[test]
fn a_snapshot_is_paired_with_a_time_and_the_hub_never_forks_or_goes_back() {
    let w = World::new();
    let mut g = guard();
    let mut store = w.node_store(NODE);
    snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 3, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    let hash = Some(w.snapshot(NODE, 3, "").snapshot.hash());

    let ask =
        |g: &mut Guard| admit(&w, g, report(3, hash, T0 + 100, 1), RequestKind::Snapshot).unwrap();

    // An older epoch, a fork of the same epoch, another node's snapshot, a hub time that does
    // not advance and one before the snapshot was issued are all refused.
    let before = g.ledger().get(&id(NODE)).unwrap();
    for (snapshot, ms) in [
        (w.snapshot(NODE, 2, ""), T0 + 200),
        (w.snapshot(NODE, 3, "fork"), T0 + 200),
        (w.snapshot(OTHER, 4, ""), T0 + 200),
        (w.snapshot(NODE, 4, ""), T0 + 100),
        (w.snapshot(NODE, 4, ""), T0 + 3),
    ] {
        let admitted = ask(&mut g);
        assert_eq!(
            g.reply_snapshot(admitted, snapshot, &w.ts, ms).unwrap_err(),
            ReplayRefusal::ReplyRefused
        );
    }
    // The same snapshot again (a resend) is allowed and takes a new nonce.
    let admitted = ask(&mut g);
    let again = g
        .reply_snapshot(admitted, w.snapshot(NODE, 3, ""), &w.ts, T0 + 200)
        .unwrap();
    assert_eq!(again.time.time.nonce, before.issued_nonce + 1);
    assert_eq!(again.time.time.epoch, 3);
    // Nothing else moved.
    let after = g.ledger().get(&id(NODE)).unwrap();
    assert_eq!(after.acked_epoch, 3);
    assert_eq!(after.acked_hash, hash);
}

#[test]
fn a_reply_needs_the_matching_request_and_one_request_answers_once() {
    let w = World::new();
    let mut g = guard();
    let mut store = w.node_store(NODE);
    snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 1, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    let hash = Some(w.snapshot(NODE, 1, "").snapshot.hash());
    let rep = report(1, hash, T0 + 100, 1);

    // A time request cannot be answered with a snapshot or timestamp, and the reverse.
    let t = admit(&w, &mut g, rep, RequestKind::HubTime).unwrap();
    assert_eq!(
        g.reply_snapshot(t, w.snapshot(NODE, 2, ""), &w.ts, T0 + 200)
            .unwrap_err(),
        ReplayRefusal::ReplyRefused
    );
    let s = admit(&w, &mut g, rep, RequestKind::Snapshot).unwrap();
    assert_eq!(
        g.reply_hub_time(s, &w.ts, w.delegation(), T0 + 200)
            .unwrap_err(),
        ReplayRefusal::ReplyRefused
    );

    // Two admitted copies of the same state: the second reply finds the ledger moved.
    let first = admit(&w, &mut g, rep, RequestKind::HubTime).unwrap();
    let second = admit(&w, &mut g, rep, RequestKind::HubTime).unwrap();
    g.reply_hub_time(first, &w.ts, w.delegation(), T0 + 200)
        .unwrap();
    assert_eq!(
        g.reply_hub_time(second, &w.ts, w.delegation(), T0 + 300)
            .unwrap_err(),
        ReplayRefusal::LedgerFailed
    );
}

#[test]
fn a_timestamp_restates_what_the_hub_acknowledged() {
    let w = World::new();
    let mut g = guard();
    // Nothing acknowledged yet: nothing to restate.
    let admitted = admit(&w, &mut g, report(0, None, 0, 0), RequestKind::Snapshot).unwrap();
    assert_eq!(
        g.reply_timestamp(
            admitted,
            TierDeadlines([T0 + HOUR; 3]),
            &w.ts,
            w.delegation(),
            T0 + 1
        )
        .unwrap_err(),
        ReplayRefusal::ReplyRefused
    );
    let mut store = w.node_store(NODE);
    snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 2, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    let hash = Some(w.snapshot(NODE, 2, "").snapshot.hash());
    let admitted = admit(
        &w,
        &mut g,
        report(2, hash, T0 + 100, 1),
        RequestKind::Snapshot,
    )
    .unwrap();
    let ts = g
        .reply_timestamp(
            admitted,
            TierDeadlines([T0 + 2 * HOUR; 3]),
            &w.ts,
            w.delegation(),
            T0 + 200,
        )
        .unwrap();
    assert_eq!(ts.timestamp.epoch, 2);
    assert_eq!(Some(ts.timestamp.snapshot_hash), hash);
    assert_eq!(ts.timestamp.nonce, 2);
    // The node accepts it.
    store.accept(&Message::Timestamp(ts), rx(10, 1)).unwrap();
}

#[test]
fn nothing_is_issued_when_the_ledger_fails_and_an_exhausted_nonce_is_refused() {
    let w = World::new();
    let mut g = guard();
    let admitted = admit(&w, &mut g, report(0, None, 0, 0), RequestKind::HubTime).unwrap();
    g.ledger().set_failing(true);
    assert_eq!(
        g.reply_hub_time(admitted, &w.ts, w.delegation(), T0 + 1)
            .unwrap_err(),
        ReplayRefusal::LedgerFailed
    );
    // An unreadable ledger refuses requests and reads as flagged.
    assert_eq!(
        admit(&w, &mut g, report(0, None, 0, 0), RequestKind::HubTime).unwrap_err(),
        ReplayRefusal::LedgerFailed
    );
    assert!(g.requires_reenrolment(&id(NODE)));
    g.ledger().set_failing(false);
    assert_eq!(g.ledger().get(&id(NODE)).unwrap(), NodeLedger::default());

    // The counter at its end: no wrap to zero.
    let end = NodeLedger {
        issued_nonce: u64::MAX,
        issued_hub_ms: 5,
        reported_nonce: u64::MAX,
        ..NodeLedger::default()
    };
    g.ledger()
        .replace(&id(NODE), &NodeLedger::default(), end)
        .unwrap();
    let admitted = admit(
        &w,
        &mut g,
        report(0, None, 0, u64::MAX),
        RequestKind::HubTime,
    )
    .unwrap();
    assert_eq!(
        g.reply_hub_time(admitted, &w.ts, w.delegation(), T0)
            .unwrap_err(),
        ReplayRefusal::ReplyRefused
    );
}

#[test]
fn a_regression_holds_even_when_the_audit_sink_is_down() {
    let w = World::new();
    let mut g = guard();
    let mut store = w.node_store(NODE);
    snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 2, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    g.audit_mut().fail = true;
    assert_eq!(
        admit(&w, &mut g, report(1, None, 0, 0), RequestKind::HubTime).unwrap_err(),
        ReplayRefusal::NodeRegressed
    );
    assert!(g.requires_reenrolment(&id(NODE)));
    assert_eq!(g.audit_failures(), 1);
}

#[test]
fn re_enrolment_clears_the_flag_keeps_the_nonces_and_the_audit_trail() {
    let w = World::new();
    let mut g = guard();
    let mut store = w.node_store(NODE);
    snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 2, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    assert!(admit(&w, &mut g, report(1, None, 0, 0), RequestKind::HubTime).is_err());
    let issued_before = g.ledger().get(&id(NODE)).unwrap().issued_nonce;

    // An audit sink that is down stops the re-enrolment: the decision is audited first.
    g.audit_mut().fail = true;
    assert_eq!(
        g.reenrol(&id(NODE), "admin-1", at()).unwrap_err(),
        ReplayRefusal::AuditFailed
    );
    assert!(g.requires_reenrolment(&id(NODE)));
    g.audit_mut().fail = false;

    g.reenrol(&id(NODE), "admin-1", at()).unwrap();
    assert!(!g.requires_reenrolment(&id(NODE)));
    // The regression record is still there, then the re-enrolment.
    assert_eq!(kinds(&g), [KIND_NODE_REGRESSED, KIND_NODE_REENROLLED]);
    let record = &g.audit().records[1];
    assert_eq!(record.actor, "offline-identity:user:admin-1");
    assert_eq!(record.payload["acknowledged_epoch"], PayloadValue::Int(2));

    // A reinstalled node holds nothing and is taken at epoch zero, and its next reply carries a
    // nonce above everything issued before.
    let ledger = g.ledger().get(&id(NODE)).unwrap();
    assert_eq!((ledger.acked_epoch, ledger.enrolment), (0, 1));
    assert_eq!(ledger.issued_nonce, issued_before);
    let admitted = admit(&w, &mut g, report(0, None, 0, 0), RequestKind::HubTime).unwrap();
    let time = g
        .reply_hub_time(admitted, &w.ts, w.delegation(), T0 + 500)
        .unwrap();
    assert_eq!(time.time.nonce, issued_before + 1);
    // A reply from before the re-enrolment cannot be issued again with an old hub time.
    let admitted = admit(&w, &mut g, report(0, None, 0, 0), RequestKind::HubTime).unwrap();
    assert_eq!(
        g.reply_hub_time(admitted, &w.ts, w.delegation(), T0 + 100)
            .unwrap_err(),
        ReplayRefusal::ReplyRefused
    );
}

#[test]
fn a_flagged_node_is_granted_no_range_until_it_is_re_enrolled() {
    let w = World::new();
    let ledger = MemoryLedgerStore::new();
    let mut registry = NodeRegistry::new(tenant());
    registry.register(id(NODE));
    let registry = Arc::new(Mutex::new(registry));
    let standing = ReenrolmentStanding::new(LiveRegistryStanding::new(registry), ledger.clone());
    let series = SeriesKey::new("INV-").unwrap();
    let sync_node = spot_sync_protocol::NodeId::new(NODE).unwrap();
    let policy = GrantPolicy::new().with_node_series(&sync_node, &series);
    let mut grants = GrantService::open(
        MemoryGrantLog::new(),
        standing,
        MemoryAuditSink::default(),
        policy,
    );
    let ask = LeaseRequest {
        series: series.clone(),
        size: 10,
        claimed_node: None,
        request_id: None,
    };
    let who = transport(NODE);
    assert!(grants.request(&who, &ask, at()).is_ok());

    // The guard flags the node; the very next range request is refused.
    let mut g = ReplayGuard::new(tenant(), ledger, MemoryAuditSink::default());
    let mut store = w.node_store(NODE);
    snapshot_round(
        &w,
        &mut g,
        &mut store,
        w.snapshot(NODE, 2, ""),
        T0 + 100,
        1,
        1,
    )
    .unwrap();
    assert!(admit(&w, &mut g, report(1, None, 0, 0), RequestKind::HubTime).is_err());
    let later = at() + time::Duration::days(1);
    assert_eq!(
        grants.request(&who, &ask, later).unwrap_err(),
        GrantError::NodeNotActive
    );
    assert_eq!(
        grants.standing_mut().standing(&sync_node),
        Ok(Standing::ReenrolmentRequired)
    );

    g.reenrol(&id(NODE), "admin-1", later).unwrap();
    assert!(grants.request(&who, &ask, later).is_ok());
}

#[test]
fn the_memory_ledger_store_passes_the_contract() {
    ledger_contract::run(MemoryLedgerStore::new);
}

#[test]
fn every_refusal_has_a_code_in_the_lease_grants_or_offline_identity_namespace() {
    let codes: BTreeSet<&str> = [
        ReplayRefusal::NodeMismatch,
        ReplayRefusal::Request(RequestRefusal::BadSignature),
        ReplayRefusal::Request(RequestRefusal::ReportMissing),
        ReplayRefusal::NodeRegressed,
        ReplayRefusal::ReportAhead,
        ReplayRefusal::ReplyRefused,
        ReplayRefusal::LedgerFailed,
        ReplayRefusal::AuditFailed,
    ]
    .iter()
    .map(ReplayRefusal::code)
    .collect();
    assert_eq!(codes.len(), 8);
    for code in codes {
        assert!(code.starts_with("lease_grants.") || code.starts_with("offline_identity."));
    }
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
