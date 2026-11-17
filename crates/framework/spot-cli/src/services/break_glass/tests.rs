#![allow(clippy::unwrap_used, clippy::expect_used)]
//! End-to-end tests of the customer side over fakes: a real in-memory audit chain, an
//! append-only object store, a reversible sealer, a duty table and an agent bridge that can be
//! made unreachable.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_audit_chain::{
    AuditChain, AuditRecord, AuditStore, CheckpointPolicy, InMemoryStore, LocalSigner,
    PayloadValue, Subject,
};
use spot_break_glass::audit::ACTIVE;
use spot_break_glass::{
    BreakGlassError, BreakGlassEvent, FileRecorder, FrameKind, Principal, Recorder,
};
use spot_core::ManualClock;
use time::OffsetDateTime;

use super::*;
use std::result::Result;

const TENANT: &str = "acme";
const T0: i64 = 1_800_000_000;

struct Chain {
    chain: AuditChain<InMemoryStore>,
    down: AtomicBool,
}

impl EventLog for Chain {
    fn append<'a>(&'a self, event: BreakGlassEvent) -> BoxFuture<'a, Result<(), HostError>> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                return Err(HostError::Crate(BreakGlassError::AuditUnavailable));
            }
            let mut payload = BTreeMap::new();
            payload.insert(
                "event".to_owned(),
                PayloadValue::Text(serde_json::to_string(&event).unwrap()),
            );
            let record = AuditRecord {
                kind: format!("break_glass.{}", event.kind),
                actor: "system".to_owned(),
                on_behalf_of: Some(format!("user:{}", event.who)),
                subject: Some(Subject {
                    doctype: "BreakGlassSession".to_owned(),
                    id: event.session,
                }),
                at: event.at * 1_000_000,
                payload,
                masked: true,
            };
            self.chain
                .append(record)
                .await
                .map(|_| ())
                .map_err(|_| HostError::Crate(BreakGlassError::AuditUnavailable))
        })
    }

    fn entries<'a>(
        &'a self,
    ) -> BoxFuture<'a, Result<Vec<spot_audit_chain::ChainEntry>, HostError>> {
        Box::pin(async move {
            self.chain
                .store()
                .range(TENANT, 1, u64::MAX)
                .await
                .map_err(|_| HostError::Crate(BreakGlassError::AuditUnavailable))
        })
    }
}

#[derive(Default)]
struct Objects(Mutex<BTreeMap<String, Vec<u8>>>);

impl Objects {
    fn map(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<u8>>> {
        self.0.lock().unwrap()
    }
}

impl ObjectStore for Objects {
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), HostError>> {
        Box::pin(async move {
            let mut m = self.map();
            if m.contains_key(key) {
                return Err(HostError::Crate(BreakGlassError::AppendOnly));
            }
            m.insert(key.to_owned(), bytes);
            Ok(())
        })
    }
    fn get<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<Vec<u8>>, HostError>> {
        Box::pin(async move { Ok(self.map().get(key).cloned()) })
    }
    fn list<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Vec<String>, HostError>> {
        Box::pin(async move {
            Ok(self
                .map()
                .keys()
                .filter(|k| k.starts_with(prefix))
                .cloned()
                .collect())
        })
    }
}

impl Retire for Objects {
    fn remove<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), HostError>> {
        Box::pin(async move {
            self.map().remove(key);
            Ok(())
        })
    }
}

/// A reversible stand-in for the secrets service: the context is bound as a prefix.
struct Xor;

impl Sealer for Xor {
    fn seal(&self, context: &str, plain: &[u8]) -> Result<Vec<u8>, HostError> {
        let mut out = context.as_bytes().to_vec();
        out.push(0);
        out.extend(plain.iter().map(|b| b ^ 0x5a));
        Ok(out)
    }
    fn open(&self, context: &str, sealed: &[u8]) -> Result<Vec<u8>, HostError> {
        let mut prefix = context.as_bytes().to_vec();
        prefix.push(0);
        let rest = sealed
            .strip_prefix(prefix.as_slice())
            .ok_or(HostError::Seal)?;
        Ok(rest.iter().map(|b| b ^ 0x5a).collect())
    }
}

#[derive(Default)]
struct DutyTable(Mutex<BTreeSet<(String, String)>>);

impl Duties for DutyTable {
    fn holds<'a>(
        &'a self,
        tenant: &'a str,
        user: &'a str,
        duty: &'a str,
    ) -> BoxFuture<'a, Result<bool, HostError>> {
        Box::pin(async move {
            if tenant != TENANT {
                return Err(HostError::Denied);
            }
            Ok(self
                .0
                .lock()
                .unwrap()
                .contains(&(user.to_owned(), duty.to_owned())))
        })
    }
}

struct Bridge {
    reachable: AtomicBool,
    kills: Mutex<Vec<String>>,
}

impl AgentBridge for Bridge {
    fn kill<'a>(
        &'a self,
        _tenant: &'a str,
        session: &'a str,
    ) -> BoxFuture<'a, Result<(), HostError>> {
        Box::pin(async move {
            if !self.reachable.load(Ordering::SeqCst) {
                return Err(HostError::Store);
            }
            self.kills.lock().unwrap().push(session.to_owned());
            Ok(())
        })
    }
}

struct Rig {
    host: BreakGlassHost,
    clock: Arc<ManualClock>,
    chain: Arc<Chain>,
    objects: Arc<Objects>,
    bridge: Arc<Bridge>,
}

fn caller(user: &str) -> Caller {
    Caller {
        tenant: TENANT.to_owned(),
        user: user.to_owned(),
    }
}

fn window(session: &str, from: i64, to: i64) -> Window {
    Window {
        tenant: TENANT.to_owned(),
        session: session.to_owned(),
        requester: "support-agent".to_owned(),
        starts_at: from,
        expires_at: to,
    }
}

async fn rig() -> Rig {
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(T0).unwrap(),
    ));
    let signer = Arc::new(LocalSigner::from_seed([7; 32]));
    let chain = Arc::new(Chain {
        chain: AuditChain::new(
            InMemoryStore::new(),
            TENANT,
            signer,
            CheckpointPolicy { every: 1 },
        ),
        down: AtomicBool::new(false),
    });
    let objects = Arc::new(Objects::default());
    let duties = Arc::new(DutyTable::default());
    duties
        .0
        .lock()
        .unwrap()
        .insert(("admin".into(), ADMIN_DUTY.into()));
    duties
        .0
        .lock()
        .unwrap()
        .insert(("auditor".into(), AUDITOR_DUTY.into()));
    let bridge = Arc::new(Bridge {
        reachable: AtomicBool::new(true),
        kills: Mutex::new(Vec::new()),
    });
    let parts = Parts {
        log: Some(Arc::clone(&chain) as Arc<dyn EventLog>),
        store: Some(Arc::clone(&objects) as Arc<dyn ObjectStore>),
        retire: Some(Arc::clone(&objects) as Arc<dyn Retire>),
        sealer: Some(Arc::new(Xor)),
        duties: Some(duties as Arc<dyn Duties>),
        bridge: Some(Arc::clone(&bridge) as Arc<dyn AgentBridge>),
        retention: None,
    };
    let host = BreakGlassHost::new(
        TENANT,
        Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
        parts,
        Settings::default(),
    );
    host.load().await.unwrap();
    Rig {
        host,
        clock,
        chain,
        objects,
        bridge,
    }
}

/// The operator side's events for a session that requested and went active.
async fn open_session(rig: &Rig, session: &str, from: i64, to: i64) {
    let requested = BreakGlassEvent::new(
        "requested",
        session,
        TENANT,
        "support-agent",
        "fix a stuck workflow",
        "allow",
        T0,
    );
    rig.host.record_event(requested).await.unwrap();
    rig.host.register(window(session, from, to)).await.unwrap();
    let active = BreakGlassEvent::new(
        ACTIVE,
        session,
        TENANT,
        "support-agent",
        "started",
        "allow",
        from,
    );
    rig.host.record_event(active).await.unwrap();
}

fn recording_bytes(session: &str, tamper: bool) -> Vec<u8> {
    // Unique per call: tests run in parallel and may ask for the same session.
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "spot-bg-{}-{n}-{session}-{tamper}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut rec = FileRecorder::create(&dir, session, TENANT, T0, 1 << 20).unwrap();
    rec.append(FrameKind::Input, T0, b"ls -l\n", true).unwrap();
    rec.append(FrameKind::Output, T0, b"total 0\n", true)
        .unwrap();
    rec.append(FrameKind::Input, T0, b"hunter2\n", false)
        .unwrap();
    rec.finish().unwrap();
    let mut bytes = std::fs::read(FileRecorder::recording_path(&dir, session)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    if tamper {
        let at = bytes.len() - 3;
        bytes[at] ^= 0xff;
    }
    bytes
}

#[tokio::test]
async fn a_session_appears_for_the_customer_and_authorises_inside_its_window() {
    let r = rig().await;
    open_session(&r, "s1", T0, T0 + 600).await;
    r.host.authorise("s1", T0 + 10).await.unwrap();
    assert_eq!(
        r.host.authorise("s1", T0 + 600).await.unwrap_err().code(),
        "break_glass.expired"
    );
    assert_eq!(
        r.host.authorise("nope", T0).await.unwrap_err().code(),
        code::NOT_FOUND
    );
    let list = r
        .host
        .list(
            &caller("auditor"),
            Page {
                after: None,
                limit: 10,
            },
        )
        .await
        .unwrap();
    assert_eq!(list.rows.len(), 1);
    assert_eq!(list.rows[0].who, "support-agent");
    assert_eq!(list.rows[0].state, "active");
    let one = r.host.summary(&caller("admin"), "s1").await.unwrap();
    assert_eq!(one.reason, "fix a stuck workflow");
}

#[tokio::test]
async fn a_revoke_ends_the_session_and_later_actions_are_refused() {
    let r = rig().await;
    open_session(&r, "s1", T0, T0 + 600).await;
    let out = r.host.revoke(&caller("admin"), "s1").await.unwrap();
    assert_eq!(
        out,
        RevokeOutcome {
            delivered: true,
            audited: true
        }
    );
    assert_eq!(r.bridge.kills.lock().unwrap().as_slice(), ["s1"]);
    assert_eq!(
        r.host.authorise("s1", T0 + 1).await.unwrap_err().code(),
        "break_glass.revoked"
    );
    let state = r.host.summary(&caller("admin"), "s1").await.unwrap().state;
    assert_eq!(state, "revoked");
    // Final: a second revoke is idempotent, an ended session cannot be revoked.
    assert!(r.host.revoke(&caller("admin"), "s1").await.is_ok());
}

#[tokio::test]
async fn an_unreachable_bridge_still_revokes_and_the_sweep_retries_the_kill() {
    let r = rig().await;
    open_session(&r, "s1", T0, T0 + 600).await;
    r.bridge.reachable.store(false, Ordering::SeqCst);
    let out = r.host.revoke(&caller("admin"), "s1").await.unwrap();
    assert_eq!(
        out,
        RevokeOutcome {
            delivered: false,
            audited: true
        }
    );
    assert_eq!(
        r.host.authorise("s1", T0 + 1).await.unwrap_err().code(),
        "break_glass.revoked"
    );
    assert!(r.bridge.kills.lock().unwrap().is_empty());
    r.bridge.reachable.store(true, Ordering::SeqCst);
    let report = r.host.sweep_at(T0 + 5).await;
    assert_eq!(report.kills, 1);
    assert_eq!(r.bridge.kills.lock().unwrap().as_slice(), ["s1"]);
}

#[tokio::test]
async fn a_revoke_whose_event_cannot_be_written_still_stops_the_session_and_is_flushed_later() {
    let r = rig().await;
    open_session(&r, "s1", T0, T0 + 600).await;
    r.chain.down.store(true, Ordering::SeqCst);
    let out = r.host.revoke(&caller("admin"), "s1").await.unwrap();
    assert!(!out.audited);
    assert_eq!(
        r.host.authorise("s1", T0 + 1).await.unwrap_err().code(),
        "break_glass.revoked"
    );
    r.chain.down.store(false, Ordering::SeqCst);
    assert_eq!(r.host.sweep_at(T0 + 2).await.flushed, 1);
    assert_eq!(
        r.host.summary(&caller("admin"), "s1").await.unwrap().state,
        "revoked"
    );
}

#[tokio::test]
async fn the_sweep_kills_and_marks_a_session_past_its_window() {
    let r = rig().await;
    open_session(&r, "s1", T0, T0 + 60).await;
    open_session(&r, "s2", T0, T0 + 6000).await;
    r.clock.advance(Duration::from_secs(61));
    let report = r.host.sweep().await;
    assert_eq!((report.expired, report.kills), (1, 1));
    assert_eq!(r.bridge.kills.lock().unwrap().as_slice(), ["s1"]);
    assert_eq!(
        r.host.summary(&caller("admin"), "s1").await.unwrap().state,
        "expired"
    );
    assert_eq!(
        r.host.summary(&caller("admin"), "s2").await.unwrap().state,
        "active"
    );
    assert_eq!(r.host.sweep().await.expired, 0);
}

#[tokio::test]
async fn the_recording_hash_matches_the_chain_and_every_read_is_audited() {
    let r = rig().await;
    open_session(&r, "s1", T0, T0 + 600).await;
    let digest = r
        .host
        .archive_recording("s1", recording_bytes("s1", false))
        .await
        .unwrap();
    let sealed = r.objects.map().get("recordings/s1.rec").cloned().unwrap();
    assert!(!sealed.windows(5).any(|w| w == b"ls -l"), "stored sealed");
    let view = r.host.recording(&caller("admin"), "s1").await.unwrap();
    assert_eq!(view.digest, digest);
    assert_eq!(view.summary.commands, 2);
    assert!(
        view.frames.iter().any(|f| f.redacted),
        "no-echo input stays redacted"
    );
    assert!(!view.frames.iter().any(|f| f.data == b"hunter2\n"));
    let reads = r
        .chain
        .entries()
        .await
        .unwrap()
        .iter()
        .filter(|e| e.record.kind == "break_glass.recording_read")
        .count();
    assert_eq!(reads, 1);
    // If the read cannot be audited nothing is returned.
    r.chain.down.store(true, Ordering::SeqCst);
    let err = r.host.recording(&caller("admin"), "s1").await;
    assert!(err.is_err());
}

#[tokio::test]
async fn a_tampered_archive_object_is_detected_and_a_foreign_one_is_refused() {
    let r = rig().await;
    open_session(&r, "s1", T0, T0 + 600).await;
    open_session(&r, "s2", T0, T0 + 600).await;
    assert!(
        r.host
            .archive_recording("s1", recording_bytes("s1", true))
            .await
            .is_err()
    );
    r.host
        .archive_recording("s1", recording_bytes("s1", false))
        .await
        .unwrap();
    // Replace the stored object with another session's sealed recording: the context differs.
    let other = Xor
        .seal("break_glass:acme:s2", &recording_bytes("s1", false))
        .unwrap();
    r.objects
        .map()
        .insert("recordings/s1.rec".to_owned(), other);
    assert_eq!(
        r.host
            .recording(&caller("admin"), "s1")
            .await
            .unwrap_err()
            .code(),
        code::SEAL_FAILED
    );
    // A valid recording of another content under the right context: the digest differs from the chain.
    let mut forged = recording_bytes("s1", false);
    let n = forged.len();
    forged.truncate(n - 20);
    let sealed = Xor.seal("break_glass:acme:s1", &forged).unwrap();
    r.objects
        .map()
        .insert("recordings/s1.rec".to_owned(), sealed);
    assert!(r.host.recording(&caller("admin"), "s1").await.is_err());
}

#[tokio::test]
async fn an_archive_is_append_only_and_the_operator_cannot_erase_it() {
    let r = rig().await;
    open_session(&r, "s1", T0, T0 + 600).await;
    r.host
        .archive_recording("s1", recording_bytes("s1", false))
        .await
        .unwrap();
    let again = r
        .host
        .archive_recording("s1", recording_bytes("s1", false))
        .await
        .unwrap_err();
    assert_eq!(again.code(), "break_glass.append_only");
    let vendor = r
        .host
        .erase(&Principal::vendor("support-agent"), "s1")
        .unwrap_err();
    assert_eq!(vendor.code(), "break_glass.vendor_delete_refused");
    let customer = r
        .host
        .erase(&Principal::customer("admin"), "s1")
        .unwrap_err();
    assert_eq!(customer.code(), "break_glass.append_only");
    assert!(r.objects.map().contains_key("recordings/s1.rec"));
}

#[tokio::test]
async fn only_the_security_administrator_reads_recordings_and_revokes() {
    let r = rig().await;
    open_session(&r, "s1", T0, T0 + 600).await;
    r.host
        .archive_recording("s1", recording_bytes("s1", false))
        .await
        .unwrap();
    for who in ["auditor", "nobody"] {
        assert_eq!(
            r.host
                .recording(&caller(who), "s1")
                .await
                .unwrap_err()
                .code(),
            code::DENIED
        );
        assert_eq!(
            r.host.revoke(&caller(who), "s1").await.unwrap_err().code(),
            code::DENIED
        );
    }
    assert_eq!(
        r.host
            .list(
                &caller("nobody"),
                Page {
                    after: None,
                    limit: 5
                }
            )
            .await
            .unwrap_err()
            .code(),
        code::DENIED
    );
    // Another tenant's caller is refused before any lookup.
    let foreign = Caller {
        tenant: "other".into(),
        user: "admin".into(),
    };
    assert_eq!(
        r.host.recording(&foreign, "s1").await.unwrap_err().code(),
        code::DENIED
    );
    // Still authorised afterwards: the refusals changed nothing.
    r.host.authorise("s1", T0 + 1).await.unwrap();
}

#[tokio::test]
async fn a_cross_tenant_window_and_a_missing_session_answer_alike() {
    let r = rig().await;
    let mut w = window("s9", T0, T0 + 60);
    w.tenant = "other".into();
    assert_eq!(r.host.register(w).await.unwrap_err().code(), code::DENIED);
    let missing = r.host.summary(&caller("admin"), "ghost").await.unwrap_err();
    let bad_id = r.host.summary(&caller("admin"), "../x").await.unwrap_err();
    assert_eq!(missing.code(), code::NOT_FOUND);
    assert_eq!(bad_id.code(), code::NOT_FOUND);
}

#[tokio::test]
async fn paging_is_newest_first_and_the_cursor_is_checked() {
    let r = rig().await;
    for id in ["a1", "a2", "a3"] {
        open_session(&r, id, T0, T0 + 600).await;
    }
    let first = r
        .host
        .list(
            &caller("admin"),
            Page {
                after: None,
                limit: 2,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        first
            .rows
            .iter()
            .map(|s| s.session.as_str())
            .collect::<Vec<_>>(),
        ["a3", "a2"]
    );
    let second = r
        .host
        .list(
            &caller("admin"),
            Page {
                after: first.next.clone(),
                limit: 2,
            },
        )
        .await
        .unwrap();
    assert_eq!(second.rows.len(), 1);
    assert!(second.next.is_none());
    let bad = r
        .host
        .list(
            &caller("admin"),
            Page {
                after: Some("zz".into()),
                limit: 2,
            },
        )
        .await;
    assert_eq!(bad.unwrap_err().code(), code::INVALID);
    assert!(
        r.host
            .list(
                &caller("admin"),
                Page {
                    after: None,
                    limit: 0
                }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_restart_keeps_the_registry_and_retention_removes_old_recordings_of_finished_sessions() {
    let r = rig().await;
    open_session(&r, "s1", T0, T0 + 600).await;
    r.host
        .archive_recording("s1", recording_bytes("s1", false))
        .await
        .unwrap();
    r.host.revoke(&caller("admin"), "s1").await.unwrap();
    // A new host over the same chain and objects knows the session is over.
    let parts = Parts {
        log: Some(Arc::clone(&r.chain) as Arc<dyn EventLog>),
        store: Some(Arc::clone(&r.objects) as Arc<dyn ObjectStore>),
        retire: Some(Arc::clone(&r.objects) as Arc<dyn Retire>),
        sealer: Some(Arc::new(Xor)),
        duties: None,
        bridge: None,
        retention: None,
    };
    let again = BreakGlassHost::new(
        TENANT,
        Arc::clone(&r.clock) as Arc<dyn spot_core::Clock>,
        parts,
        Settings::default(),
    );
    again.load().await.unwrap();
    assert_eq!(
        again.authorise("s1", T0 + 1).await.unwrap_err().code(),
        "break_glass.revoked"
    );
    // No duty check wired: every read is refused.
    assert_eq!(
        again
            .summary(&caller("admin"), "s1")
            .await
            .unwrap_err()
            .code(),
        code::DENIED
    );
    // Retention: nothing before the period, the recording after it.
    assert_eq!(r.host.sweep_at(T0 + 3600).await.retired, 0);
    let later = T0 + 366 * 86_400;
    assert_eq!(r.host.sweep_at(later).await.retired, 1);
    assert!(!r.objects.map().contains_key("recordings/s1.rec"));
}

#[tokio::test]
async fn an_unloaded_or_unwired_host_refuses_to_authorise() {
    let clock = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(T0).unwrap(),
    ));
    let host = BreakGlassHost::new(
        TENANT,
        clock as Arc<dyn spot_core::Clock>,
        Parts::default(),
        Settings::default(),
    );
    assert_eq!(
        host.missing(),
        ["event_log", "object_store", "sealer", "duties"]
    );
    assert_eq!(
        host.authorise("s1", T0).await.unwrap_err().code(),
        code::NOT_WIRED
    );
    assert_eq!(host.load().await.unwrap_err().code(), code::NOT_WIRED);
}

#[test]
fn settings_refuse_unknown_keys_and_out_of_bounds_values() {
    let ok: Settings = toml::from_str("sweep_secs = 5").unwrap();
    assert!(ok.validate().is_ok());
    assert!(toml::from_str::<Settings>("sweep = 5").is_err());
    let bad = Settings {
        retention_days: 0,
        ..Settings::default()
    };
    assert!(bad.validate().unwrap_err().0.contains("retention_days"));
}

#[test]
fn an_error_never_carries_a_value() {
    let shown = HostError::Invalid("the cursor is not valid").to_string();
    assert!(shown.starts_with("break_glass_host.invalid"));
    assert_eq!(
        HostError::from(BreakGlassError::UnknownSession),
        HostError::NotFound
    );
}
