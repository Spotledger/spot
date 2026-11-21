//! The access surface end to end over the in-memory grant store and the in-memory journal store:
//! grant rules, authorization, the manifest gate, ranges, limits and the one-record-per-call
//! audit rule.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use ed25519_dalek::SigningKey;
use spot_journal_access::{
    Access, AccessAudit, AccessError, AccessObject, AccessOutcome, AccessRecord, AdminDirectory,
    DAY_MILLIS, Fut, GrantStore, IdSource, JournalMeta, JournalReader, Limits, MemoryGrantStore,
    ReadError, ReaderParts, SegmentStatus, Settings, TimeSource,
};
use spot_journal_manifest::{Manifest, SegmentSummary, VerifyingKeys, append_segments, sign};
use spot_journal_store::{JournalStore, Layout, MemoryStore, etag};

const T0: i64 = 1_800_000_000_000;

#[derive(Default)]
struct Admins(Mutex<HashSet<String>>);
impl Admins {
    fn set(&self, who: &str, admin: bool) {
        let mut set = self.0.lock().unwrap();
        if admin {
            set.insert(who.to_owned());
        } else {
            set.remove(who);
        }
    }
}
impl AdminDirectory for Admins {
    fn is_tenant_administrator<'a>(&'a self, principal: &'a str) -> Fut<'a, bool> {
        Box::pin(async move { Ok(self.0.lock().unwrap().contains(principal)) })
    }
}

#[derive(Default)]
struct Ids(AtomicU64);
impl IdSource for Ids {
    fn new_id(&self) -> String {
        format!("grant-{}", self.0.fetch_add(1, Ordering::SeqCst))
    }
}

#[derive(Default)]
struct Audit {
    records: Mutex<Vec<AccessRecord>>,
    broken: AtomicBool,
}
impl Audit {
    fn all(&self) -> Vec<AccessRecord> {
        self.records.lock().unwrap().clone()
    }
}
impl AccessAudit for Audit {
    fn record<'a>(&'a self, record: &'a AccessRecord) -> Fut<'a, ()> {
        Box::pin(async move {
            if self.broken.load(Ordering::SeqCst) {
                return Err(AccessError::Store);
            }
            self.records.lock().unwrap().push(record.clone());
            Ok(())
        })
    }
}

struct Clock {
    now: AtomicI64,
    readable: AtomicBool,
}
impl TimeSource for Clock {
    fn now_millis(&self) -> Option<i64> {
        self.readable
            .load(Ordering::SeqCst)
            .then(|| self.now.load(Ordering::SeqCst))
    }
}

fn hash_bytes(bytes: &[u8]) -> [u8; 32] {
    let hex = etag(bytes);
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

struct World {
    grants: Arc<MemoryGrantStore>,
    admins: Arc<Admins>,
    audit: Arc<Audit>,
    clock: Arc<Clock>,
    access: Arc<Access>,
    reader: JournalReader,
    segments: Vec<Vec<u8>>,
}

fn segment_bytes(n: usize) -> Vec<u8> {
    (0..300_000usize).map(|i| ((i + n) % 251) as u8).collect()
}

async fn world_for(tenant: &str, store: Arc<MemoryStore>, put: bool) -> World {
    let signing = SigningKey::from_bytes(&[7u8; 32]);
    let mut keys = VerifyingKeys::new();
    keys.add_key("k1", signing.verifying_key(), 1, 10).unwrap();
    let segments = vec![segment_bytes(1), segment_bytes(2)];
    let mut manifest = Manifest::first(tenant, T0).unwrap();
    let mut previous = [0u8; 32];
    let mut first = 1u64;
    let mut summaries = Vec::new();
    for bytes in &segments {
        let hash = hash_bytes(bytes);
        let last = first + 99;
        summaries.push(SegmentSummary::new(1, first, last, 100, hash, previous));
        if put {
            let key = Layout::segment(tenant, 1, first, last).unwrap().to_key();
            store.put_new(&key, bytes.clone()).await.unwrap();
        }
        previous = hash;
        first = last + 1;
    }
    manifest = append_segments(&manifest, &summaries).unwrap();
    let signed = sign(&manifest, &signing, "k1");
    if put {
        let key = Layout::manifest(tenant, 1).unwrap().to_key();
        store.put_new(&key, signed.to_bytes()).await.unwrap();
    }
    let grants = Arc::new(MemoryGrantStore::new());
    let admins = Arc::new(Admins::default());
    admins.set("admin", true);
    let access = Arc::new(
        Access::new(
            grants.clone() as Arc<dyn GrantStore>,
            admins.clone(),
            Arc::new(Ids::default()),
            Settings::default(),
        )
        .unwrap(),
    );
    let audit = Arc::new(Audit::default());
    let clock = Arc::new(Clock {
        now: AtomicI64::new(T0),
        readable: AtomicBool::new(true),
    });
    let reader = JournalReader::new(ReaderParts {
        tenant: tenant.to_owned(),
        store: store.clone(),
        keys: Arc::new(keys),
        access: access.clone(),
        audit: audit.clone(),
        limits: Arc::new(Limits::new(2, 10_000_000)),
        clock: clock.clone(),
        meta: JournalMeta {
            policy: "standard".to_owned(),
            region: "eu".to_owned(),
        },
    })
    .unwrap();
    World {
        grants,
        admins,
        audit,
        clock,
        access,
        reader,
        segments,
    }
}

async fn world() -> World {
    world_for("acme", Arc::new(MemoryStore::new()), true).await
}

async fn grant_external(w: &World, who: &str) -> String {
    w.access
        .grant("admin", who, true, None, T0)
        .await
        .unwrap()
        .id
}

fn last(w: &World) -> AccessRecord {
    w.audit.all().last().cloned().unwrap()
}

#[tokio::test]
async fn off_by_default_and_denied_calls_are_audited() {
    let w = world().await;
    let error = w.reader.info("auditor").await.unwrap_err();
    assert_eq!(error, ReadError::Denied { expired: false });
    assert_eq!(w.audit.all().len(), 1);
    assert_eq!(last(&w).outcome, AccessOutcome::Denied);
    assert_eq!(last(&w).reason.as_deref(), Some("no_grant"));
}

#[tokio::test]
async fn a_granted_external_reads_everything_and_the_bytes_hash_to_the_manifest() {
    let w = world().await;
    let id = grant_external(&w, "auditor").await;
    let info = w.reader.info("auditor").await.unwrap();
    assert_eq!(info.generations.len(), 1);
    assert_eq!(info.generations[0].last_seq, 200);
    assert_eq!(info.policy, "standard");
    let manifest = w.reader.manifest("auditor", "1").await.unwrap();
    assert_eq!(manifest["segments"].as_array().unwrap().len(), 2);
    for (i, bytes) in w.segments.iter().enumerate() {
        let range = format!("{}-{}", i * 100 + 1, i * 100 + 100);
        let mut read = w
            .reader
            .segment("auditor", "1", &range, None)
            .await
            .unwrap();
        assert_eq!(read.status, SegmentStatus::Full);
        assert_eq!(read.etag, etag(bytes));
        assert_eq!(read.content_length, bytes.len() as u64);
        let mut body = Vec::new();
        let mut stream = read.stream.take().unwrap();
        while let Some(chunk) = stream.next_chunk() {
            assert!(chunk.len() <= spot_journal_access::CHUNK_BYTES);
            body.extend(chunk);
        }
        assert_eq!(&body, bytes);
        stream.finish().await.unwrap();
    }
    // info + manifest + two segments: exactly one record each, naming the grant.
    let records = w.audit.all();
    assert_eq!(records.len(), 4);
    assert!(records.iter().all(|r| r.grant == id));
    assert!(records.iter().all(|r| r.outcome == AccessOutcome::Served));
    assert_eq!(records[3].bytes, Some(w.segments[1].len() as u64));
}

#[tokio::test]
async fn the_administrator_reads_through_the_same_audited_path() {
    let w = world().await;
    w.reader.manifest("admin", "1").await.unwrap();
    assert_eq!(last(&w).grant, "admin");
    assert_eq!(last(&w).object, AccessObject::Manifest);
}

#[tokio::test]
async fn grant_rules() {
    let w = world().await;
    assert_eq!(
        w.access
            .grant("admin", "x", true, Some(8), T0)
            .await
            .unwrap_err(),
        AccessError::LifetimeExceeded { limit_days: 7 }
    );
    assert_eq!(
        w.access
            .grant("admin", "x", false, Some(31), T0)
            .await
            .unwrap_err(),
        AccessError::LifetimeExceeded { limit_days: 30 }
    );
    assert_eq!(
        w.access
            .grant("nobody", "x", false, None, T0)
            .await
            .unwrap_err(),
        AccessError::NotAdministrator
    );
    let granted = w
        .access
        .grant("admin", "reader", false, Some(30), T0)
        .await
        .unwrap();
    assert_eq!(granted.expires_at_millis, T0 + 30 * DAY_MILLIS);
    // A journal_reader cannot grant journal_reader.
    assert_eq!(
        w.access
            .grant("reader", "friend", false, None, T0)
            .await
            .unwrap_err(),
        AccessError::NotAdministrator
    );
    assert_eq!(
        w.access
            .grant("admin", "reader", false, None, T0)
            .await
            .unwrap_err(),
        AccessError::GrantExists
    );
    assert!(w.access.list("reader").await.is_err());
    assert_eq!(w.access.list("admin").await.unwrap().len(), 1);
    // The events: granted, refused (lifetime), refused (lifetime), refused (not admin) x2 ...
    let actions: Vec<&str> = w
        .grants
        .events()
        .iter()
        .map(|e| e.action.as_str())
        .collect();
    assert_eq!(actions.iter().filter(|a| **a == "granted").count(), 1);
    assert!(actions.iter().filter(|a| **a == "refused").count() >= 3);
}

#[tokio::test]
async fn renewal_extends_and_expiry_is_refused_with_the_expired_reason() {
    let w = world().await;
    let id = grant_external(&w, "auditor").await;
    w.clock.now.store(T0 + 8 * DAY_MILLIS, Ordering::SeqCst);
    let error = w.reader.info("auditor").await.unwrap_err();
    assert_eq!(error, ReadError::Denied { expired: true });
    assert_eq!(last(&w).outcome, AccessOutcome::Expired);
    let renewed = w
        .access
        .renew("admin", &id, 14, T0 + 8 * DAY_MILLIS)
        .await
        .unwrap();
    assert_eq!(renewed.expires_at_millis, T0 + 22 * DAY_MILLIS);
    w.reader.info("auditor").await.unwrap();
}

#[tokio::test]
async fn revocation_takes_effect_at_the_next_request_even_when_cached() {
    let w = world().await;
    let id = grant_external(&w, "auditor").await;
    w.reader.info("auditor").await.unwrap();
    w.access.revoke("admin", &id, T0 + 1).await.unwrap();
    w.clock.now.store(T0 + 2, Ordering::SeqCst);
    let error = w.reader.info("auditor").await.unwrap_err();
    // The wire shows no oracle: a revoked grant looks like no grant.
    assert_eq!(error, ReadError::Denied { expired: false });
    assert_eq!(last(&w).reason.as_deref(), Some("grant_revoked"));
}

#[tokio::test]
async fn a_grant_is_void_when_the_grantor_loses_the_administrator_role() {
    let w = world().await;
    grant_external(&w, "auditor").await;
    w.reader.info("auditor").await.unwrap();
    w.admins.set("admin", false);
    // Past the 5 s cache.
    w.clock.now.store(T0 + 6_000, Ordering::SeqCst);
    assert_eq!(
        w.reader.info("auditor").await.unwrap_err(),
        ReadError::Denied { expired: false }
    );
    assert_eq!(
        last(&w).reason.as_deref(),
        Some("grantor_not_administrator")
    );
}

#[tokio::test]
async fn unreadable_clock_fails_closed() {
    let w = world().await;
    grant_external(&w, "auditor").await;
    w.clock.readable.store(false, Ordering::SeqCst);
    assert_eq!(
        w.reader.info("auditor").await.unwrap_err(),
        ReadError::Denied { expired: false }
    );
    assert_eq!(w.audit.all().len(), 1);
}

#[tokio::test]
async fn only_segments_listed_in_the_manifest_are_served_and_hostile_paths_are_404() {
    let w = world().await;
    grant_external(&w, "auditor").await;
    for (generation, range) in [
        ("1", "1-200"),   // a span of two segments
        ("1", "1-99"),    // not a listed segment
        ("1", "201-300"), // beyond the manifest
        ("2", "1-100"),   // unknown generation
        ("0", "1-100"),
        ("..", "1-100"),
        ("%2e%2e", "1-100"),
        ("1", "..-100"),
        ("1", "01-100"),
        ("1", "00000000000000000001-00000000000000000100"),
        ("1", "1-100/../x"),
        ("1", "100-1"),
    ] {
        let before = w.audit.all().len();
        let error = w
            .reader
            .segment("auditor", generation, range, None)
            .await
            .err();
        assert_eq!(error, Some(ReadError::NotFound), "{generation} {range}");
        assert_eq!(w.audit.all().len(), before + 1, "exactly one record");
        assert_eq!(last(&w).outcome, AccessOutcome::NotFound);
    }
    assert_eq!(
        w.reader.manifest("auditor", "9").await.unwrap_err(),
        ReadError::NotFound
    );
}

#[tokio::test]
async fn a_reader_of_one_site_cannot_name_the_other_sites_keys() {
    let shared = Arc::new(MemoryStore::new());
    let a = world_for("site-a", shared.clone(), true).await;
    let b = world_for("site-b", shared.clone(), false).await;
    a.access
        .grant("admin", "auditor", true, None, T0)
        .await
        .unwrap();
    b.access
        .grant("admin", "auditor", true, None, T0)
        .await
        .unwrap();
    // The shared store holds A's journal; B has none: B's reader finds no manifest and no key
    // of A's tenant is reachable through B's tenant prefix.
    assert!(
        a.reader
            .segment("auditor", "1", "1-100", None)
            .await
            .is_ok()
    );
    assert_eq!(
        b.reader.segment("auditor", "1", "1-100", None).await.err(),
        Some(ReadError::NotFound)
    );
    assert_eq!(b.reader.info("auditor").await.unwrap().generations.len(), 0);
    // A hand-built tenant id with a separator is refused at construction.
    assert!(
        JournalReader::new(ReaderParts {
            tenant: "site-a/../site-b".to_owned(),
            store: shared,
            keys: Arc::new(VerifyingKeys::new()),
            access: b.access.clone(),
            audit: b.audit.clone(),
            limits: Arc::new(Limits::new(1, 1)),
            clock: b.clock.clone(),
            meta: JournalMeta {
                policy: String::new(),
                region: String::new()
            },
        })
        .is_err()
    );
}

#[tokio::test]
async fn a_corrupted_segment_is_not_served() {
    let store = Arc::new(MemoryStore::new());
    let w = world_for("acme", store.clone(), true).await;
    // A second world on another store, whose segment bytes differ but whose manifest is the
    // one we publish: put the manifest of `w` with a tampered first segment.
    let tampered = Arc::new(MemoryStore::new());
    let signing = SigningKey::from_bytes(&[7u8; 32]);
    let mut keys = VerifyingKeys::new();
    keys.add_key("k1", signing.verifying_key(), 1, 10).unwrap();
    let good = segment_bytes(1);
    let manifest = append_segments(
        &Manifest::first("acme", T0).unwrap(),
        &[SegmentSummary::new(
            1,
            1,
            100,
            100,
            hash_bytes(&good),
            [0u8; 32],
        )],
    )
    .unwrap();
    tampered
        .put_new(
            &Layout::manifest("acme", 1).unwrap().to_key(),
            sign(&manifest, &signing, "k1").to_bytes(),
        )
        .await
        .unwrap();
    let mut bad = good.clone();
    bad[10] ^= 0xff;
    tampered
        .put_new(&Layout::segment("acme", 1, 1, 100).unwrap().to_key(), bad)
        .await
        .unwrap();
    let reader = JournalReader::new(ReaderParts {
        tenant: "acme".to_owned(),
        store: tampered,
        keys: Arc::new(keys),
        access: w.access.clone(),
        audit: w.audit.clone(),
        limits: Arc::new(Limits::new(2, 10_000_000)),
        clock: w.clock.clone(),
        meta: JournalMeta {
            policy: String::new(),
            region: String::new(),
        },
    })
    .unwrap();
    w.access
        .grant("admin", "auditor", true, None, T0)
        .await
        .unwrap();
    assert_eq!(
        reader.segment("auditor", "1", "1-100", None).await.err(),
        Some(ReadError::Unavailable)
    );
    assert_eq!(last(&w).outcome, AccessOutcome::Failed);
    assert_eq!(last(&w).reason.as_deref(), Some("hash_mismatch"));
}

#[tokio::test]
async fn a_failing_audit_refuses_the_call() {
    let w = world().await;
    grant_external(&w, "auditor").await;
    w.audit.broken.store(true, Ordering::SeqCst);
    assert_eq!(
        w.reader.manifest("auditor", "1").await.unwrap_err(),
        ReadError::AuditUnavailable
    );
    assert_eq!(
        w.reader.info("auditor").await.unwrap_err(),
        ReadError::AuditUnavailable
    );
    // A name that is not listed needs its not_found record first.
    assert_eq!(
        w.reader.segment("auditor", "1", "1-99", None).await.err(),
        Some(ReadError::AuditUnavailable)
    );
    // A denial is refused as an audit failure too: no unaudited answer about a stranger.
    assert_eq!(
        w.reader.info("stranger").await.unwrap_err(),
        ReadError::AuditUnavailable
    );
    // A listed segment is served only if its record can be written at the end.
    let mut read = w
        .reader
        .segment("auditor", "1", "1-100", None)
        .await
        .unwrap();
    let mut stream = read.stream.take().unwrap();
    while stream.next_chunk().is_some() {}
    assert_eq!(
        stream.finish().await.unwrap_err(),
        ReadError::AuditUnavailable
    );
    assert!(w.audit.all().is_empty());
}

#[tokio::test]
async fn ranges_resume_and_an_unsatisfiable_range_is_416() {
    let w = world().await;
    grant_external(&w, "auditor").await;
    let total = w.segments[0].len() as u64;
    let mut cut = w
        .reader
        .segment("auditor", "1", "1-100", Some("bytes=0-99999"))
        .await
        .unwrap();
    assert_eq!(cut.status, SegmentStatus::Partial);
    assert_eq!(
        cut.content_range.as_deref(),
        Some(&*format!("bytes 0-99999/{total}"))
    );
    let mut first_part = Vec::new();
    let mut stream = cut.stream.take().unwrap();
    while let Some(chunk) = stream.next_chunk() {
        first_part.extend(chunk);
    }
    stream.finish().await.unwrap();
    let mut rest = w
        .reader
        .segment("auditor", "1", "1-100", Some("bytes=100000-"))
        .await
        .unwrap();
    let mut stream = rest.stream.take().unwrap();
    let mut second = Vec::new();
    while let Some(chunk) = stream.next_chunk() {
        second.extend(chunk);
    }
    stream.finish().await.unwrap();
    first_part.extend(second);
    assert_eq!(first_part, w.segments[0]);
    let refused = w
        .reader
        .segment("auditor", "1", "1-100", Some("bytes=999999999-"))
        .await
        .unwrap();
    assert_eq!(refused.status, SegmentStatus::RangeNotSatisfiable);
    assert_eq!(refused.content_range, Some(format!("bytes */{total}")));
    assert!(refused.stream.is_none());
}

#[tokio::test]
async fn an_abandoned_stream_is_recorded_as_failed_and_frees_its_slot() {
    let w = world().await;
    grant_external(&w, "auditor").await;
    let mut a = w
        .reader
        .segment("auditor", "1", "1-100", None)
        .await
        .unwrap();
    let mut stream = a.stream.take().unwrap();
    let _ = stream.next_chunk();
    stream.finish().await.unwrap();
    assert_eq!(last(&w).outcome, AccessOutcome::Failed);
    assert_eq!(last(&w).reason.as_deref(), Some("aborted"));
    assert_eq!(
        last(&w).bytes,
        Some(spot_journal_access::CHUNK_BYTES as u64)
    );
}

#[tokio::test]
async fn concurrency_and_budget_limits() {
    let w = world().await;
    grant_external(&w, "auditor").await;
    let a = w
        .reader
        .segment("auditor", "1", "1-100", Some("bytes=0-9"))
        .await
        .unwrap();
    let b = w
        .reader
        .segment("auditor", "1", "1-100", Some("bytes=0-9"))
        .await
        .unwrap();
    let third = w
        .reader
        .segment("auditor", "1", "1-100", Some("bytes=0-9"))
        .await
        .err();
    assert!(matches!(third, Some(ReadError::RateLimited { .. })));
    assert_eq!(last(&w).reason.as_deref(), Some("rate_limited"));
    drop((a, b));
    // Slots are free again once the streams are dropped.
    assert!(
        w.reader
            .segment("auditor", "1", "1-100", Some("bytes=0-9"))
            .await
            .is_ok()
    );
    assert!(w.reader.limits().bytes_today("auditor", T0) >= 30);
}
