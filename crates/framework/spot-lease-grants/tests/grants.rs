//! The hub-side grant service: identity, standing, entitlement, size and rate, idempotent
//! re-requests, atomicity, audit, restart.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use spot_audit_chain::PayloadValue;
use spot_lease_grants::{
    GrantError, GrantLog, GrantPolicy, GrantRecord, GrantService, KIND_GRANTED, KIND_REFUSED,
    KIND_REPLAYED, LeaseRequest, LiveRegistryStanding, MemoryGrantLog, RegistryStanding, RequestId,
    Standing, StandingPort, StandingUnavailable, TransportNode, contract,
};
use spot_leased_sequences::{LeaseRefusal, LeasedSequences, LeasedSequencesError};
use spot_offline_identity::{MemoryAuditSink, NodeRegistry, TenantId};
use spot_partition::{
    LeaseId, LowWater, MemorySeriesStore, PartitionError, RangeAllocator, RangeGrant, RangeLease,
};
use spot_storage_contract::SeriesKey;
use spot_sync_protocol::NodeId;
use time::{Date, Duration, OffsetDateTime};

const A: &str = "edge-khi#branch_07";
const B: &str = "edge-lhe#branch_09";
const C: &str = "edge-isb#branch_11";

/// The series the tests below lease, entitled to each test node.
const KEYS: &[&str] = &["INV-", "PO-", "INV-2026-", "PO-2026-", "BIG-"];

fn node(s: &str) -> NodeId {
    NodeId::new(s).unwrap()
}

fn series(s: &str) -> SeriesKey {
    SeriesKey::new(s).unwrap()
}

fn t0() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_790_000_000).unwrap()
}

fn minutes(n: i64) -> OffsetDateTime {
    t0() + Duration::minutes(n)
}

fn registry() -> NodeRegistry {
    let mut r = NodeRegistry::new(TenantId::new("acme").unwrap());
    for n in [A, B, C] {
        r.register(spot_offline_identity::NodeId::new(n).unwrap());
    }
    r
}

/// Entitles every test node to `keys`: the policy grants nothing a hub has not named.
fn entitle(mut policy: GrantPolicy, keys: &[&str]) -> GrantPolicy {
    for who in [A, B, C] {
        for key in keys {
            policy = policy.with_node_series(&node(who), &series(key));
        }
    }
    policy
}

fn policy() -> GrantPolicy {
    entitle(GrantPolicy::new(), KEYS)
}

type Service = GrantService<
    spot_lease_grants::RebuildableAllocator,
    MemoryGrantLog,
    RegistryStanding,
    MemoryAuditSink,
>;

fn service_with(policy: GrantPolicy) -> Service {
    GrantService::open(
        MemoryGrantLog::new(),
        RegistryStanding::new(registry()),
        MemoryAuditSink::default(),
        policy,
    )
}

fn service() -> Service {
    service_with(policy())
}

fn ask(who: &str, key: &SeriesKey, size: u64) -> (TransportNode, LeaseRequest) {
    (
        TransportNode::authenticated(node(who)),
        LeaseRequest {
            series: key.clone(),
            size,
            claimed_node: None,
            request_id: None,
        },
    )
}

fn ask_id(who: &str, key: &SeriesKey, size: u64, id: &str) -> (TransportNode, LeaseRequest) {
    (
        TransportNode::authenticated(node(who)),
        LeaseRequest {
            series: key.clone(),
            size,
            claimed_node: None,
            request_id: Some(RequestId::new(id).unwrap()),
        },
    )
}

/// A log record, for seeding a service through `open` (a restart, or a corrupt log).
fn record_at(
    key: &SeriesKey,
    from: u64,
    to: u64,
    id: u64,
    granted_at: OffsetDateTime,
    request_id: Option<&str>,
) -> GrantRecord {
    GrantRecord {
        lease: RangeLease::new(key.clone(), from, to, node(A), LeaseId::new(id)).unwrap(),
        node: node(A),
        requested_size: to - from + 1,
        granted_at,
        request_id: request_id.map(|id| RequestId::new(id).unwrap()),
    }
}

fn text(r: &spot_audit_chain::AuditRecord, key: &str) -> String {
    match r.payload.get(key) {
        Some(PayloadValue::Text(t)) => t.clone(),
        other => panic!("no text `{key}`: {other:?}"),
    }
}

fn int_of(r: &spot_audit_chain::AuditRecord, key: &str) -> i64 {
    match r.payload.get(key) {
        Some(PayloadValue::Int(v)) => *v,
        other => panic!("no int `{key}`: {other:?}"),
    }
}

/// The assertions every refusal record must satisfy.
fn assert_refusal(r: &spot_audit_chain::AuditRecord, reason: &str, code: &str) {
    assert_eq!(r.kind, KIND_REFUSED);
    assert_eq!(text(r, "reason"), reason);
    assert_eq!(text(r, "code"), code);
    assert_eq!(r.payload["aggregate"], PayloadValue::Bool(false));
    assert_eq!(int_of(r, "count"), 1);
    assert!(r.masked);
}

#[test]
fn a_grant_to_an_active_node_is_inside_the_series_logged_and_audited() {
    let mut s = service();
    let key = series("INV-2026-");
    let (who, req) = ask(A, &key, 500);
    let lease = s.request(&who, &req, t0()).unwrap();
    assert_eq!((lease.from, lease.to, lease.size()), (1, 500, 500));
    assert_eq!(lease.node, node(A));
    assert_eq!(lease.series, key);
    // The log holds it, and the audit sink has one record of the grant.
    let logged = s.log().all();
    assert_eq!(logged.len(), 1);
    assert_eq!(logged[0].lease, lease);
    assert_eq!(
        (logged[0].node.clone(), logged[0].requested_size),
        (node(A), 500)
    );
    assert_eq!(logged[0].granted_at, t0());
    let records = &s.audit().records;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].kind, KIND_GRANTED);
    assert!(records[0].actor.ends_with(A), "{}", records[0].actor);
    assert_eq!(records[0].payload["from"], PayloadValue::Int(1));
    assert_eq!(records[0].payload["to"], PayloadValue::Int(500));
    assert_eq!(
        records[0].payload["lease_id"],
        PayloadValue::Int(i64::try_from(lease.lease_id.get()).unwrap())
    );
    assert!(records[0].masked);
}

#[test]
fn two_nodes_asking_in_turn_get_disjoint_ranges() {
    let mut s = service();
    let key = series("INV-2026-");
    let (wa, ra) = ask(A, &key, 100);
    let (wb, rb) = ask(B, &key, 100);
    let a = s.request(&wa, &ra, t0()).unwrap();
    let b = s.request(&wb, &rb, t0()).unwrap();
    assert!(a.to < b.from);
    assert_ne!(a.lease_id, b.lease_id);
    // Another series starts at 1 again.
    let other = series("PO-2026-");
    let (wa2, ra2) = ask(A, &other, 10);
    assert_eq!(s.request(&wa2, &ra2, t0()).unwrap().from, 1);
}

#[test]
fn a_payload_naming_another_node_is_refused_and_audited_with_both_ids() {
    let mut s = service();
    let key = series("INV-2026-");
    let who = TransportNode::authenticated(node(A));
    let req = LeaseRequest {
        series: key.clone(),
        size: 10,
        claimed_node: Some(node(B)),
        request_id: None,
    };
    let err = s.request(&who, &req, t0()).unwrap_err();
    assert_eq!(err.code(), "lease_grants.node_mismatch");
    assert_eq!(
        err,
        GrantError::NodeMismatch {
            authenticated: node(A),
            claimed: node(B)
        }
    );
    assert!(s.log().all().is_empty(), "nothing was granted");
    let r = &s.audit().records[0];
    assert_refusal(r, "node_mismatch", "lease_grants.node_mismatch");
    assert!(r.actor.ends_with(A));
    assert_eq!(text(r, "claimed_node"), B);
    // The same claim as the authenticated node is fine.
    let ok = LeaseRequest {
        claimed_node: Some(node(A)),
        ..req
    };
    assert!(s.request(&who, &ok, t0()).is_ok());
}

#[test]
fn a_suspended_decommissioned_or_unknown_node_gets_no_range_and_no_hint() {
    let key = series("INV-2026-");
    let mut reg = registry();
    reg.suspend(&spot_offline_identity::NodeId::new(B).unwrap(), 1)
        .unwrap();
    reg.decommission(&spot_offline_identity::NodeId::new(C).unwrap(), 1)
        .unwrap();
    let mut s = GrantService::open(
        MemoryGrantLog::new(),
        RegistryStanding::new(reg),
        MemoryAuditSink::default(),
        policy(),
    );
    let mut messages = BTreeSet::new();
    for who in [B, C, "edge-unknown#nobody"] {
        let (w, r) = ask(who, &key, 10);
        let err = s.request(&w, &r, t0()).unwrap_err();
        assert_eq!(err.code(), "lease_grants.node_not_active");
        messages.insert(err.to_string());
    }
    assert_eq!(
        messages.len(),
        1,
        "the message does not say which: {messages:?}"
    );
    assert!(s.log().all().is_empty());
    assert_eq!(s.audit().records.len(), 3, "every refusal is audited");
    // An active node still gets one.
    let (w, r) = ask(A, &key, 10);
    assert!(s.request(&w, &r, t0()).is_ok());
    // A suspension takes effect on the next request.
    let (w, r) = ask(A, &key, 10);
    s.standing_mut()
        .registry_mut()
        .suspend(&spot_offline_identity::NodeId::new(A).unwrap(), 2)
        .unwrap();
    assert_eq!(
        s.request(&w, &r, minutes(10)).unwrap_err().code(),
        "lease_grants.node_not_active"
    );
}

struct Broken;
impl StandingPort for Broken {
    fn standing(&self, _node: &NodeId) -> Result<Standing, StandingUnavailable> {
        Err(StandingUnavailable)
    }
}

#[test]
fn a_registry_that_cannot_answer_fails_closed() {
    let mut s = GrantService::open(
        MemoryGrantLog::new(),
        Broken,
        MemoryAuditSink::default(),
        policy(),
    );
    let (w, r) = ask(A, &series("INV-"), 10);
    assert_eq!(
        s.request(&w, &r, t0()).unwrap_err().code(),
        "lease_grants.node_not_active"
    );
    assert!(s.log().all().is_empty());
    assert_refusal(
        &s.audit().records[0],
        "standing_unavailable",
        "lease_grants.node_not_active",
    );
}

#[test]
fn a_suspension_on_the_shared_registry_reaches_the_very_next_request() {
    let live = Arc::new(Mutex::new(registry()));
    let mut s = GrantService::open(
        MemoryGrantLog::new(),
        LiveRegistryStanding::new(Arc::clone(&live)),
        MemoryAuditSink::default(),
        policy(),
    );
    let key = series("INV-");
    let (w, r) = ask_id(A, &key, 10, "before-the-suspension");
    let first = s.request(&w, &r, t0()).unwrap();
    // The hub suspends the node on the one registry it holds.
    live.lock()
        .unwrap()
        .suspend(&spot_offline_identity::NodeId::new(A).unwrap(), 3)
        .unwrap();
    // The next request is refused, and so is a replay of the range it still holds.
    let (w, r) = ask(A, &key, 10);
    assert_eq!(
        s.request(&w, &r, minutes(2)).unwrap_err().code(),
        "lease_grants.node_not_active"
    );
    let (w, r) = ask_id(A, &key, 10, "before-the-suspension");
    assert_eq!(
        s.request(&w, &r, minutes(2)).unwrap_err().code(),
        "lease_grants.node_not_active"
    );
    assert_eq!(s.log().all().len(), 1, "only the first grant was written");
    assert_eq!(s.log().all()[0].lease, first);
    // Reinstating it lets it ask again.
    live.lock()
        .unwrap()
        .reinstate(&spot_offline_identity::NodeId::new(A).unwrap())
        .unwrap();
    let (w, r) = ask(A, &key, 10);
    assert!(s.request(&w, &r, minutes(3)).is_ok());
}

#[test]
fn size_is_bounded_per_series() {
    let big = series("BIG-");
    let policy = policy().with_series_max(&big, 50_000);
    let mut s = service_with(policy);
    let key = series("INV-");
    for size in [0, 10_001, u64::MAX] {
        let (w, r) = ask(A, &key, size);
        let err = s.request(&w, &r, t0()).unwrap_err();
        assert_eq!(err, GrantError::SizeRefused { max: 10_000 });
    }
    let (w, r) = ask(A, &key, 10_000);
    assert_eq!(s.request(&w, &r, t0()).unwrap().size(), 10_000);
    let (w, r) = ask(A, &big, 50_000);
    assert_eq!(s.request(&w, &r, t0()).unwrap().size(), 50_000);
    let (w, r) = ask(A, &big, 50_001);
    assert_eq!(
        s.request(&w, &r, minutes(5)).unwrap_err(),
        GrantError::SizeRefused { max: 50_000 }
    );
}

#[test]
fn a_node_cannot_drain_a_series_by_asking_in_a_loop() {
    let mut s = service();
    let key = series("INV-");
    let (w, r) = ask(A, &key, 10_000);
    s.request(&w, &r, t0()).unwrap();
    // Within the minimum interval, however often it asks (and at an earlier instant too).
    for secs in [0, 1, 30, 59] {
        let err = s
            .request(&w, &r, t0() + Duration::seconds(secs))
            .unwrap_err();
        assert_eq!(err, GrantError::TooSoon, "after {secs}s");
    }
    assert_eq!(
        s.request(&w, &r, t0() - Duration::seconds(5)).unwrap_err(),
        GrantError::TooSoon
    );
    assert_eq!(s.log().all().len(), 1);
    // After the interval it is granted, the next range above the first.
    let next = s.request(&w, &r, t0() + Duration::seconds(60)).unwrap();
    assert_eq!(next.from, 10_001);
    // The limit is per node and per series: B, and A in another series, are not held back.
    let (wb, rb) = ask(B, &key, 10);
    assert!(s.request(&wb, &rb, t0() + Duration::seconds(61)).is_ok());
    let (wa, ra) = ask(A, &series("PO-"), 10);
    assert!(s.request(&wa, &ra, t0() + Duration::seconds(61)).is_ok());
}

#[test]
fn a_failing_log_returns_no_lease_and_the_lost_range_is_never_reissued() {
    let mut s = service();
    let key = series("INV-");
    let (w, r) = ask(A, &key, 100);
    let first = s.request(&w, &r, t0()).unwrap();
    s.log_mut().fail = true;
    let err = s.request(&w, &r, minutes(2)).unwrap_err();
    assert_eq!(err, GrantError::LogFailed);
    assert_eq!(s.log().all().len(), 1, "nothing was written");
    // The refusal is audited with the log failure.
    let last = s.audit().records.last().unwrap();
    assert_eq!(text(last, "reason"), "log_failed");
    s.log_mut().fail = false;
    // Even asking at once after it, the interval counts from the lost attempt.
    assert_eq!(
        s.request(&w, &r, minutes(2)).unwrap_err(),
        GrantError::TooSoon
    );
    let next = s.request(&w, &r, minutes(4)).unwrap();
    assert!(
        next.from > first.to + 100,
        "the lost range 101..=200 is skipped: {}..={}",
        next.from,
        next.to
    );
}

#[test]
fn a_failing_audit_sink_returns_no_lease() {
    let mut s = service();
    let key = series("INV-");
    let (w, r) = ask(A, &key, 100);
    s.audit_mut().fail = true;
    assert_eq!(
        s.request(&w, &r, t0()).unwrap_err(),
        GrantError::AuditFailed
    );
    s.audit_mut().fail = false;
    let lease = s.request(&w, &r, minutes(2)).unwrap();
    assert!(
        lease.from > 100,
        "the range of the unaudited grant is lost: {}",
        lease.from
    );
    // A refusal is still a refusal when its audit cannot be written.
    s.audit_mut().fail = true;
    let (wb, rb) = ask(B, &key, 0);
    assert_eq!(
        s.request(&wb, &rb, minutes(2)).unwrap_err().code(),
        "lease_grants.size_refused"
    );
}

#[test]
fn a_series_that_has_run_out_says_so() {
    let key = series("INV-");
    let mut s2 = service_with(policy().with_default_max(u64::MAX));
    let (w, r) = ask(A, &key, u64::MAX);
    assert_eq!(
        s2.request(&w, &r, t0()).unwrap_err(),
        GrantError::SeriesExhausted
    );
    assert_eq!(
        s2.request(&w, &r, minutes(5)).unwrap_err(),
        GrantError::SeriesExhausted
    );
}

/// A tiny deterministic generator, so the seeded loop needs no dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

#[test]
fn a_restart_never_reissues_a_number_across_seeded_failures() {
    for seed in [1_u64, 7, 42, 1_000_003, 0xdead_beef, 987_654_321] {
        let mut rng = Rng(seed);
        let mut s = service();
        let keys = [series("INV-"), series("PO-")];
        let mut returned: Vec<RangeLease> = Vec::new();
        let mut at = t0();
        for _ in 0..300 {
            at += Duration::seconds(61);
            match rng.below(10) {
                0 => {
                    // Restart: keep the log, standing and audit stores; rebuild the rest.
                    let (_, log, standing, audit) = s.into_parts();
                    s = GrantService::open(log, standing, audit, policy());
                }
                1 => {
                    let fail = rng.below(2) == 0;
                    s.log_mut().fail = fail;
                }
                2 => {
                    let fail = rng.below(2) == 0;
                    s.audit_mut().fail = fail;
                }
                _ => {}
            }
            let who = [A, B, C][rng.below(3) as usize];
            let key = &keys[rng.below(2) as usize];
            let (w, r) = ask(who, key, 1 + rng.below(50));
            if let Ok(lease) = s.request(&w, &r, at) {
                returned.push(lease);
            }
        }
        assert!(
            returned.len() > 20,
            "seed {seed}: only {} grants",
            returned.len()
        );
        for key in &keys {
            let mut ranges: Vec<(u64, u64)> = returned
                .iter()
                .filter(|l| &l.series == key)
                .map(|l| (l.from, l.to))
                .collect();
            ranges.sort_unstable();
            for pair in ranges.windows(2) {
                assert!(pair[0].1 < pair[1].0, "seed {seed}: {pair:?} overlap");
            }
        }
        // Every returned lease is in the log (persisted before use), with distinct ids.
        let logged: BTreeSet<u64> = s
            .log()
            .all()
            .iter()
            .map(|r| r.lease.lease_id.get())
            .collect();
        assert!(
            returned.iter().all(|l| logged.contains(&l.lease_id.get())),
            "seed {seed}"
        );
        let ids: BTreeSet<u64> = returned.iter().map(|l| l.lease_id.get()).collect();
        assert_eq!(
            ids.len(),
            returned.len(),
            "seed {seed}: a lease id was reused"
        );
    }
}

#[test]
fn a_restart_keeps_the_rate_limit() {
    let mut s = service();
    let key = series("INV-");
    let (w, r) = ask(A, &key, 10);
    s.request(&w, &r, t0()).unwrap();
    let (_, log, standing, audit) = s.into_parts();
    let mut s = GrantService::open(log, standing, audit, policy());
    assert_eq!(
        s.request(&w, &r, t0() + Duration::seconds(5)).unwrap_err(),
        GrantError::TooSoon,
        "a restart is not a way round the interval"
    );
    assert_eq!(s.request(&w, &r, minutes(2)).unwrap().from, 11);
}

#[test]
fn a_granted_lease_is_accepted_by_the_node_for_itself_and_refused_for_another() {
    let mut s = service();
    let key = series("INV-");
    let store = Arc::new(MemorySeriesStore::default());
    let (wa, ra) = ask(A, &key, 100);
    let first = s.request(&wa, &ra, t0()).unwrap();
    let mut seq = LeasedSequences::new();
    seq.renew(first, LowWater::half(), store.clone()).unwrap();
    assert_eq!(seq.allocate_block(&key, 5).unwrap(), 1);
    // The next grant to the same node is strictly above and is accepted.
    let second = s.request(&wa, &ra, minutes(2)).unwrap();
    seq.renew(second, LowWater::half(), store.clone()).unwrap();
    // A range granted to another node is not accepted by this node's sequences.
    let (wb, rb) = ask(B, &key, 100);
    let other = s.request(&wb, &rb, minutes(3)).unwrap();
    match seq.renew(other, LowWater::half(), store) {
        Err(LeasedSequencesError::LeaseRefused { reason, .. }) => {
            assert_eq!(reason, LeaseRefusal::OtherNode);
        }
        other => panic!("expected a refusal, got {:?}", other.map(|_| ())),
    }
}

#[test]
fn concurrent_requests_yield_disjoint_ranges() {
    let service = Mutex::new(service());
    let key = series("INV-");
    let results: Mutex<Vec<RangeLease>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for (i, who) in [A, B, C].into_iter().enumerate() {
            let (service, results, key) = (&service, &results, &key);
            scope.spawn(move || {
                for round in 0..20_i64 {
                    let (w, r) = ask(who, key, 25);
                    let at = t0() + Duration::minutes(round * 3 + i64::try_from(i).unwrap());
                    let lease = service.lock().unwrap().request(&w, &r, at).unwrap();
                    results.lock().unwrap().push(lease);
                }
            });
        }
    });
    let mut ranges: Vec<(u64, u64)> = results
        .into_inner()
        .unwrap()
        .iter()
        .map(|l| (l.from, l.to))
        .collect();
    assert_eq!(ranges.len(), 60);
    ranges.sort_unstable();
    for pair in ranges.windows(2) {
        assert!(pair[0].1 < pair[1].0, "{pair:?}");
    }
}

#[test]
fn the_memory_log_passes_the_contract() {
    contract::run(MemoryGrantLog::new);
}

#[test]
fn nothing_printed_holds_key_material() {
    let mut s = service();
    let key = series("INV-");
    let who = TransportNode::authenticated(node(A));
    let req = LeaseRequest {
        series: key.clone(),
        size: 10,
        claimed_node: Some(node(B)),
        request_id: Some(RequestId::new("req-1").unwrap()),
    };
    let err = s.request(&who, &req, t0()).unwrap_err();
    let (w, r) = ask(A, &key, 10);
    let lease = s.request(&w, &r, t0()).unwrap();
    let shown = format!(
        "{who:?} {req:?} {err:?} {err} {lease:?} {:?} {:?} {:?}",
        s.log().all(),
        s.audit().records,
        policy()
    )
    .to_lowercase();
    for word in ["secret", "private", "password", "token", "-----begin"] {
        assert!(!shown.contains(word), "`{word}` in {shown}");
    }
}

#[test]
fn every_code_has_a_catalogue_entry_and_every_entry_is_raised() {
    let catalogue = spot_errors::Catalogue::load();
    assert_eq!(catalogue.load_error(), None);
    let raised: BTreeSet<&str> = [
        GrantError::NodeMismatch {
            authenticated: node(A),
            claimed: node(B),
        },
        GrantError::NodeNotActive,
        GrantError::SeriesNotEntitled,
        GrantError::SeriesCapReached { max: 1 },
        GrantError::SizeRefused { max: 1 },
        GrantError::TimeInvalid,
        GrantError::TooSoon,
        GrantError::LogFailed,
        GrantError::AuditFailed,
        GrantError::SeriesExhausted,
        GrantError::AllocatorFailed,
    ]
    .iter()
    .map(GrantError::code)
    .collect();
    for code in &raised {
        assert!(catalogue.lookup(code).is_some(), "no entry for `{code}`");
    }
    let file = include_str!("../../spot-errors/catalogue/lease_grants.toml");
    let listed: BTreeSet<&str> = file
        .lines()
        .filter_map(|l| l.strip_prefix("code = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .collect();
    let mut expected = raised;
    expected.extend([
        // The replay guard's refusals (src/replay.rs).
        "lease_grants.node_regressed",
        "lease_grants.report_ahead",
        "lease_grants.reply_refused",
        "lease_grants.granted",
        "lease_grants.refused",
        "lease_grants.replayed",
    ]);
    assert_eq!(listed, expected);
}

/// Every `lease_grants.*` literal in the crate's source must be a catalogue entry, so a new
/// code cannot ship without one.
#[test]
fn every_lease_grants_code_in_source_is_in_the_catalogue() {
    let catalogue = spot_errors::Catalogue::load();
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut found = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        for part in text.split("\"lease_grants.").skip(1) {
            let Some(end) = part.find('"') else { continue };
            let name = &part[..end];
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                continue;
            }
            let code = format!("lease_grants.{name}");
            assert!(
                catalogue.lookup(&code).is_some(),
                "{} uses `{code}` with no catalogue entry",
                path.display()
            );
            found += 1;
        }
    }
    assert!(found >= 14, "the scan found only {found} codes");
}

#[test]
fn every_user_visible_code_has_help() {
    let catalogue = spot_errors::Catalogue::load();
    let help = include_str!("../../spot-unattended-support/help/lease_grants.toml");
    let helped: BTreeSet<&str> = help
        .lines()
        .filter_map(|l| l.strip_prefix("code = \""))
        .filter_map(|l| l.strip_suffix('"'))
        .collect();
    for code in [
        "lease_grants.node_mismatch",
        "lease_grants.node_not_active",
        "lease_grants.series_not_entitled",
        "lease_grants.series_cap_reached",
        "lease_grants.size_refused",
        "lease_grants.time_invalid",
        "lease_grants.too_soon",
        "lease_grants.log_failed",
        "lease_grants.audit_failed",
        "lease_grants.series_exhausted",
        "lease_grants.allocator_failed",
        "lease_grants.node_regressed",
        "lease_grants.report_ahead",
        "lease_grants.reply_refused",
    ] {
        assert!(
            catalogue.lookup(code).is_some(),
            "no catalogue entry {code}"
        );
        assert!(helped.contains(code), "no help for `{code}`");
    }
}

// ---- WP-EDG-03.T6: checked time arithmetic ------------------------------------------------

/// The last instant the `time` crate can represent, which is where the interval overflows.
fn max_instant() -> OffsetDateTime {
    Date::MAX
        .midnight()
        .assume_utc()
        .saturating_add(Duration::days(1))
}

#[test]
fn a_grant_instant_near_the_end_of_time_is_refused_without_panicking() {
    let max = max_instant();
    assert!(
        max.checked_add(Duration::seconds(60)).is_none(),
        "the premise: adding the interval to the last instant overflows"
    );
    let key = series("INV-");
    let mut log = MemoryGrantLog::new();
    log.append(record_at(&key, 1, 10, 0, max, None)).unwrap();
    let mut s = GrantService::open(
        log,
        RegistryStanding::new(registry()),
        MemoryAuditSink::default(),
        policy(),
    );
    let (w, r) = ask(A, &key, 10);
    let err = s.request(&w, &r, t0()).unwrap_err();
    assert_eq!(err, GrantError::TimeInvalid);
    assert_eq!(err.code(), "lease_grants.time_invalid");
    assert_eq!(s.log().all().len(), 1, "nothing was granted");
    assert_refusal(
        s.audit().records.last().unwrap(),
        "time_invalid",
        "lease_grants.time_invalid",
    );
}

#[test]
fn a_series_near_the_end_of_the_number_space_gives_what_fits_and_then_no_more() {
    let key = series("INV-");
    let mut log = MemoryGrantLog::new();
    // The last grant stopped ten numbers below the largest, so nine numbers are left.
    log.append(record_at(
        &key,
        1,
        u64::MAX - 10,
        0,
        t0() - Duration::days(1),
        None,
    ))
    .unwrap();
    let mut s = GrantService::open(
        log,
        RegistryStanding::new(registry()),
        MemoryAuditSink::default(),
        policy().with_min_interval(Duration::ZERO),
    );
    // A request that would need the largest number itself does not fit.
    let (w, r) = ask(A, &key, 10);
    assert_eq!(
        s.request(&w, &r, t0()).unwrap_err(),
        GrantError::SeriesExhausted
    );
    // The nine that fit are granted, and are the last ones.
    let (w, r) = ask(A, &key, 9);
    let last = s.request(&w, &r, t0()).unwrap();
    assert_eq!((last.from, last.to), (u64::MAX - 9, u64::MAX - 1));
    let (w, r) = ask(A, &key, 1);
    assert_eq!(
        s.request(&w, &r, t0()).unwrap_err(),
        GrantError::SeriesExhausted
    );
    assert_eq!(s.log().all().len(), 2);
}

// ---- WP-EDG-03.T6: per-node series entitlement --------------------------------------------

#[test]
fn a_series_the_policy_does_not_name_is_refused_before_anything_is_allocated() {
    let key = series("INV-");
    // The default policy entitles nobody to anything: nothing widens by accident.
    let mut s = GrantService::open(
        MemoryGrantLog::new(),
        RegistryStanding::new(registry()),
        MemoryAuditSink::default(),
        GrantPolicy::new(),
    );
    let (w, r) = ask(A, &key, 10);
    let err = s.request(&w, &r, t0()).unwrap_err();
    assert_eq!(err, GrantError::SeriesNotEntitled);
    assert_eq!(err.code(), "lease_grants.series_not_entitled");
    assert!(s.log().all().is_empty(), "nothing was allocated");
    assert_eq!(s.tracked_series(), 0, "no series was remembered");
    assert_refusal(
        s.audit().records.last().unwrap(),
        "series_not_entitled",
        "lease_grants.series_not_entitled",
    );
}

#[test]
fn entitlement_is_per_node_and_per_series() {
    let key = series("INV-");
    let policy = GrantPolicy::new()
        .with_node_series(&node(A), &key)
        .with_node_series(&node(B), &series("PO-"));
    let mut s = GrantService::open(
        MemoryGrantLog::new(),
        RegistryStanding::new(registry()),
        MemoryAuditSink::default(),
        policy,
    );
    let (w, r) = ask(A, &key, 10);
    assert!(s.request(&w, &r, t0()).is_ok(), "A may lease INV-");
    // The same node in another series, and another node in A's series, are refused.
    let (w, r) = ask(A, &series("PO-"), 10);
    assert_eq!(
        s.request(&w, &r, minutes(2)).unwrap_err().code(),
        "lease_grants.series_not_entitled"
    );
    let (w, r) = ask(B, &key, 10);
    assert_eq!(
        s.request(&w, &r, minutes(2)).unwrap_err().code(),
        "lease_grants.series_not_entitled"
    );
    // B's own series is fine.
    let (w, r) = ask(B, &series("PO-"), 10);
    assert!(s.request(&w, &r, minutes(2)).is_ok());
    assert_eq!(s.log().all().len(), 2);
}

#[test]
fn the_cap_on_distinct_series_refuses_a_new_one_and_keeps_the_known_ones() {
    let policy = entitle(GrantPolicy::new(), &["S1-", "S2-"]).with_max_series(1);
    let mut s = GrantService::open(
        MemoryGrantLog::new(),
        RegistryStanding::new(registry()),
        MemoryAuditSink::default(),
        policy,
    );
    let (w, r) = ask(A, &series("S1-"), 10);
    assert!(s.request(&w, &r, t0()).is_ok());
    assert_eq!(s.tracked_series(), 1);
    // A second series, entitled but new, is refused: the hub tracks its maximum.
    let (w, r) = ask(A, &series("S2-"), 10);
    let err = s.request(&w, &r, t0()).unwrap_err();
    assert_eq!(err, GrantError::SeriesCapReached { max: 1 });
    assert_eq!(err.code(), "lease_grants.series_cap_reached");
    assert_eq!(s.log().all().len(), 1);
    assert_eq!(s.tracked_series(), 1, "the refusal remembered nothing");
    assert_refusal(
        s.audit().records.last().unwrap(),
        "series_cap_reached",
        "lease_grants.series_cap_reached",
    );
    // The series it already tracks is not affected by the cap.
    let (w, r) = ask(A, &series("S1-"), 10);
    assert!(s.request(&w, &r, minutes(2)).is_ok());
}

// ---- WP-EDG-03.T6: idempotent re-requests ------------------------------------------------

#[test]
fn a_retried_request_returns_the_same_range_instead_of_burning_another() {
    let key = series("INV-");
    let mut s = service();
    let (w, r) = ask_id(A, &key, 100, "req-1");
    let first = s.request(&w, &r, t0()).unwrap();
    // The response was lost; the node asks again at once, inside the minimum interval.
    let again = s.request(&w, &r, t0() + Duration::seconds(1)).unwrap();
    assert_eq!(again, first, "the same range, not a second one");
    assert_eq!(s.log().all().len(), 1, "no second range was logged");
    let replay = s.audit().records.last().unwrap();
    assert_eq!(replay.kind, KIND_REPLAYED);
    assert_eq!(text(replay, "request_id"), "req-1");
    assert_eq!(int_of(replay, "from"), 1);
    assert_eq!(int_of(replay, "to"), 100);
    // The retry is not a rate-limit bypass: a *new* request id still waits its interval.
    let (w2, r2) = ask_id(A, &key, 100, "req-2");
    assert_eq!(
        s.request(&w2, &r2, t0() + Duration::seconds(1))
            .unwrap_err(),
        GrantError::TooSoon
    );
    assert_eq!(s.request(&w2, &r2, minutes(2)).unwrap().from, 101);
    assert_eq!(s.log().all().len(), 2);
}

#[test]
fn another_node_reusing_a_request_id_gets_its_own_range() {
    let key = series("INV-");
    let mut s = service();
    let (wa, ra) = ask_id(A, &key, 100, "shared-id");
    let a = s.request(&wa, &ra, t0()).unwrap();
    let (wb, rb) = ask_id(B, &key, 100, "shared-id");
    let b = s.request(&wb, &rb, t0()).unwrap();
    assert_ne!(a, b, "the key is the node, the series and the id");
    assert!(a.to < b.from);
    assert_eq!(s.log().all().len(), 2);
}

#[test]
fn a_restart_between_the_grant_and_the_retry_returns_the_same_range() {
    let key = series("INV-");
    let mut s = service();
    let (w, r) = ask_id(A, &key, 100, "req-crash");
    let first = s.request(&w, &r, t0()).unwrap();
    // Crash and restart: only the log survives, and it carries the request id.
    let (_, log, standing, audit) = s.into_parts();
    let mut s = GrantService::open(log, standing, audit, policy());
    assert_eq!(s.remembered_requests(), 1, "the id came back from the log");
    let again = s.request(&w, &r, t0() + Duration::seconds(2)).unwrap();
    assert_eq!(again, first);
    assert_eq!(s.log().all().len(), 1);
    assert_eq!(s.audit().records.last().unwrap().kind, KIND_REPLAYED);
}

#[test]
fn a_replay_is_audited_before_it_is_returned() {
    let key = series("INV-");
    let mut s = service();
    let (w, r) = ask_id(A, &key, 10, "req-audit");
    let first = s.request(&w, &r, t0()).unwrap();
    s.audit_mut().fail = true;
    assert_eq!(
        s.request(&w, &r, t0()).unwrap_err(),
        GrantError::AuditFailed
    );
    s.audit_mut().fail = false;
    assert_eq!(s.request(&w, &r, t0()).unwrap(), first);
}

#[test]
fn the_requests_remembered_for_one_node_are_bounded() {
    let key = series("INV-");
    let mut s = service_with(
        policy()
            .with_min_interval(Duration::ZERO)
            .with_max_remembered_requests(4),
    );
    let mut ranges = Vec::new();
    for i in 0..6 {
        let id = format!("req-{i}");
        let (w, r) = ask_id(A, &key, 1, &id);
        ranges.push(s.request(&w, &r, t0() + Duration::seconds(i)).unwrap());
    }
    assert_eq!(s.remembered_requests(), 4, "older requests were evicted");
    // The newest ones are still answered with their own range.
    let (w, r) = ask_id(A, &key, 1, "req-5");
    assert_eq!(
        s.request(&w, &r, t0() + Duration::seconds(10)).unwrap(),
        ranges[5]
    );
    // The oldest is forgotten, so it is a new request and takes a new range.
    let (w, r) = ask_id(A, &key, 1, "req-0");
    let fresh = s.request(&w, &r, t0() + Duration::seconds(11)).unwrap();
    assert_ne!(fresh, ranges[0]);
    assert_eq!(s.remembered_requests(), 4);
    assert_eq!(s.log().all().len(), 7);
}

// ---- WP-EDG-03.T6: refusal audit contents, failures and aggregation -----------------------

#[test]
fn the_refusal_of_a_size_records_its_reason_code_and_numbers() {
    let key = series("INV-");
    let mut s = service();
    let (w, r) = ask(A, &key, 0);
    assert_eq!(
        s.request(&w, &r, t0()).unwrap_err(),
        GrantError::SizeRefused { max: 10_000 }
    );
    let record = s.audit().records.last().unwrap();
    assert_refusal(record, "size_refused", "lease_grants.size_refused");
    assert_eq!(int_of(record, "requested_size"), 0);
    assert_eq!(text(record, "series"), "INV-");
    assert_eq!(text(record, "claimed_node"), "");
    // A size above the largest signed integer is recorded as the number it is, not saturated.
    // (Another node, because the first node's refusals are already in an open window.)
    let (w, r) = ask(B, &key, u64::MAX);
    assert_eq!(
        s.request(&w, &r, t0()).unwrap_err(),
        GrantError::SizeRefused { max: 10_000 }
    );
    assert_eq!(
        text(s.audit().records.last().unwrap(), "requested_size"),
        u64::MAX.to_string()
    );
}

#[test]
fn the_refusal_of_a_rate_records_its_reason_and_code() {
    let key = series("INV-");
    let mut s = service();
    let (w, r) = ask(A, &key, 10);
    assert!(s.request(&w, &r, t0()).is_ok());
    assert_eq!(
        s.request(&w, &r, t0() + Duration::seconds(59)).unwrap_err(),
        GrantError::TooSoon
    );
    assert_refusal(
        s.audit().records.last().unwrap(),
        "too_soon",
        "lease_grants.too_soon",
    );
}

#[test]
fn the_refusal_of_an_exhausted_series_records_its_reason_and_code() {
    let key = series("INV-");
    let mut s = service_with(policy().with_default_max(u64::MAX));
    let (w, r) = ask(A, &key, u64::MAX);
    assert_eq!(
        s.request(&w, &r, t0()).unwrap_err(),
        GrantError::SeriesExhausted
    );
    assert_refusal(
        s.audit().records.last().unwrap(),
        "series_exhausted",
        "lease_grants.series_exhausted",
    );
}

/// An allocator that always fails for a reason other than exhaustion.
struct BrokenAllocator;

impl RangeAllocator for BrokenAllocator {
    fn lease(
        &mut self,
        _series: &SeriesKey,
        _node: &NodeId,
        _size: u64,
        _at: OffsetDateTime,
    ) -> Result<RangeLease, PartitionError> {
        Err(PartitionError::InvalidArgument {
            reason: "this allocator is broken on purpose".to_owned(),
        })
    }

    fn grants(&self) -> Vec<RangeGrant> {
        Vec::new()
    }
}

#[test]
fn the_refusal_of_an_allocator_failure_records_its_reason_and_code() {
    let key = series("INV-");
    let mut s = GrantService::new(
        BrokenAllocator,
        MemoryGrantLog::new(),
        RegistryStanding::new(registry()),
        MemoryAuditSink::default(),
        policy(),
    );
    let (w, r) = ask(A, &key, 10);
    assert_eq!(
        s.request(&w, &r, t0()).unwrap_err(),
        GrantError::AllocatorFailed
    );
    assert_refusal(
        s.audit().records.last().unwrap(),
        "allocator_failed",
        "lease_grants.allocator_failed",
    );
    assert!(s.log().all().is_empty());
}

#[test]
fn a_refusal_whose_audit_cannot_be_written_is_counted() {
    let key = series("INV-");
    let mut s = service();
    s.audit_mut().fail = true;
    let (w, r) = ask(A, &key, 0);
    assert_eq!(
        s.request(&w, &r, t0()).unwrap_err().code(),
        "lease_grants.size_refused"
    );
    assert_eq!(s.refusal_audit_failures(), 1);
    assert!(s.audit().records.is_empty());
    // Once the sink is back, a refusal is written again and the counter does not move.
    s.audit_mut().fail = false;
    let (w, r) = ask(B, &key, 0);
    assert!(s.request(&w, &r, t0()).is_err());
    assert_eq!(s.refusal_audit_failures(), 1);
    assert_eq!(s.audit().records.len(), 1);
}

#[test]
fn a_flood_of_refusals_from_one_node_is_aggregated() {
    let key = series("INV-");
    let mut s = service_with(
        policy()
            .with_min_interval(Duration::hours(1))
            .with_refusal_window(Duration::seconds(200)),
    );
    let (w, r) = ask(A, &key, 10);
    assert!(s.request(&w, &r, t0()).is_ok());
    let after_grant = s.audit().records.len();
    // A hundred attempts inside one window: the first opens it, the rest are counted.
    for i in 0..100 {
        let (w, r) = ask(A, &key, 10);
        assert_eq!(
            s.request(&w, &r, t0() + Duration::seconds(i)).unwrap_err(),
            GrantError::TooSoon
        );
    }
    assert_eq!(
        s.audit().records.len(),
        after_grant + 1,
        "one record opens the window, not a hundred"
    );
    assert_eq!(int_of(&s.audit().records[after_grant], "count"), 1);
    // The window closes on the next refusal: one aggregate record carries its total.
    let (w, r) = ask(A, &key, 10);
    assert_eq!(
        s.request(&w, &r, t0() + Duration::seconds(300))
            .unwrap_err(),
        GrantError::TooSoon
    );
    let records = &s.audit().records;
    assert_eq!(records.len(), after_grant + 3);
    let aggregate = &records[after_grant + 1];
    assert_eq!(text(aggregate, "reason"), "too_soon");
    assert_eq!(int_of(aggregate, "count"), 100);
    assert_eq!(aggregate.payload["aggregate"], PayloadValue::Bool(true));
    // And the new window opens with its own first record.
    assert_eq!(int_of(&records[after_grant + 2], "count"), 1);
    assert_eq!(
        records[after_grant + 2].payload["aggregate"],
        PayloadValue::Bool(false)
    );
    // One node's flood costs one window, not one window per request.
    assert_eq!(s.open_refusal_windows(), 1);
    // A refusal at an instant inside the window is counted, even one earlier than its start.
    let (w, r) = ask(A, &key, 10);
    assert_eq!(
        s.request(&w, &r, t0() - Duration::hours(1)).unwrap_err(),
        GrantError::TooSoon
    );
    assert_eq!(
        s.audit().records.len(),
        after_grant + 3,
        "an earlier instant cannot manufacture a window and a record"
    );
}

// ---- WP-EDG-03.T6: audit honesty ---------------------------------------------------------

#[test]
fn a_number_above_the_largest_signed_integer_is_audited_as_its_text() {
    let key = series("INV-");
    let big = u64::try_from(i64::MAX).unwrap() + 5;
    let mut log = MemoryGrantLog::new();
    log.append(record_at(&key, 1, big, 0, t0() - Duration::days(1), None))
        .unwrap();
    let mut s = GrantService::open(
        log,
        RegistryStanding::new(registry()),
        MemoryAuditSink::default(),
        policy().with_default_max(10),
    );
    let (w, r) = ask(A, &key, 3);
    let lease = s.request(&w, &r, t0()).unwrap();
    assert_eq!((lease.from, lease.to), (big + 1, big + 3));
    let record = s.audit().records.last().unwrap();
    assert_eq!(record.kind, KIND_GRANTED);
    assert_eq!(
        record.payload["from"],
        PayloadValue::Text((big + 1).to_string())
    );
    assert_eq!(
        record.payload["to"],
        PayloadValue::Text((big + 3).to_string()),
        "never the saturated number it is not"
    );
}
