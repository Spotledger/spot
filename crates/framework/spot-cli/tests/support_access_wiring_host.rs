//! The support-access and break-glass wiring (WP-ADM-11.T4): every port the break-glass host reads
//! is provided from the real component of the site (the audit chain, the file store, the secrets
//! vault, the permission engine) or refuses with a catalogued code; and the break-glass host works
//! end to end over those adapters.
//!
//! The stores are the real in-memory implementations of the same traits the site's PostgreSQL,
//! local-file and key-file components implement (`InMemoryStore`, `MemoryFileStore`,
//! `SecretBox<MemoryKeyring>`); only the permission engine is a table, because its decision core
//! is built from installed apps. No real clock, no sleep.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use spot_app_api::prelude::RecordId;
use spot_audit_chain::InMemoryStore;
use spot_break_glass::{BreakGlassEvent, FileRecorder, FrameKind, Recorder};
use spot_cli::services::SiteCtx;
use spot_cli::services::break_glass::{
    AgentBridge, BreakGlassHost, Caller, Duties, EventLog, HostError, ObjectStore, Page, Parts,
    Retention, Retire, Sealer, Settings as BreakGlassSettings, Window,
};
use spot_cli::services::support_access_wiring::{
    self as wiring, ChainEventLog, Settings, Sources, build_parts, scope_of,
};
use spot_core::{Clock, ManualClock};
use spot_document::engine::{AppPermissionCheck, PrincipalSource};
use spot_files::memory::MemoryFileStore;
use spot_files::store::FileStore;
use spot_permissions::Principal;
use spot_secrets::{MemoryKeyring, SecretBox};
use time::OffsetDateTime;

const TENANT: &str = "acme";
const T0: i64 = 1_800_000_000;
const DAY: i64 = 86_400;

// ---- the permission table (the decision core is built from installed apps) ---------------------

/// Who holds which duty, keyed by the user's record id text.
#[derive(Default)]
struct Table(BTreeSet<(String, String)>);

impl AppPermissionCheck for Table {
    fn holds<'a>(
        &'a self,
        principal: &'a Principal,
        permission: &'a str,
    ) -> spot_app_api::BoxFuture<'a, bool> {
        let held = self
            .0
            .contains(&(principal.user.clone(), permission.to_owned()));
        Box::pin(async move { held })
    }
}

/// Every id resolves to a principal named by it (an unknown user holds nothing).
struct Everyone;

impl PrincipalSource for Everyone {
    fn resolve<'a>(&'a self, user: &'a RecordId) -> spot_app_api::BoxFuture<'a, Principal> {
        let name = user.to_string();
        Box::pin(async move { Principal::user(&name, Vec::new()) })
    }
}

struct Rig {
    host: BreakGlassHost,
    clock: Arc<ManualClock>,
    files: Arc<MemoryFileStore>,
    admin: String,
    auditor: String,
    nobody: String,
    log: Arc<ChainEventLog<InMemoryStore>>,
}

fn new_user() -> String {
    RecordId::new_v7().to_string()
}

fn log() -> ChainEventLog<InMemoryStore> {
    ChainEventLog::new(InMemoryStore::new(), &scope_of(TENANT))
}

fn vault() -> Arc<dyn spot_document::secrets::SecretVault> {
    Arc::new(SecretBox::new(MemoryKeyring::new("test-keyring").unwrap()))
}

fn clock() -> Arc<ManualClock> {
    Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(T0).unwrap(),
    ))
}

/// A full site: all four real parts, retention from this module's setting when `days` is set.
async fn rig(days: Option<u32>) -> Rig {
    let (admin, auditor, nobody) = (new_user(), new_user(), new_user());
    let mut table = Table::default();
    table
        .0
        .insert((admin.clone(), "security_administrator".to_owned()));
    table
        .0
        .insert((auditor.clone(), "security_auditor".to_owned()));
    let files = Arc::new(MemoryFileStore::new());
    let log = Arc::new(log());
    let sources = Sources {
        tenant: TENANT.to_owned(),
        log: Some(Arc::clone(&log) as Arc<dyn EventLog>),
        files: Some(Arc::clone(&files) as Arc<dyn FileStore>),
        vault: Some(vault()),
        permissions: Some((
            Arc::new(table) as Arc<dyn AppPermissionCheck>,
            Arc::new(Everyone) as Arc<dyn PrincipalSource>,
        )),
    };
    let settings = Settings {
        retention_days: days,
        ..Settings::default()
    };
    let (parts, report) = build_parts(sources, &settings);
    assert!(report.missing.is_empty(), "{report:?}");
    let clock = clock();
    let host = BreakGlassHost::new(
        TENANT,
        Arc::clone(&clock) as Arc<dyn Clock>,
        parts,
        BreakGlassSettings::default(),
    );
    host.load().await.unwrap();
    Rig {
        host,
        clock,
        files,
        admin,
        auditor,
        nobody,
        log,
    }
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
        "active",
        session,
        TENANT,
        "support-agent",
        "started",
        "allow",
        from,
    );
    rig.host.record_event(active).await.unwrap();
}

fn recording_bytes(session: &str) -> Vec<u8> {
    // Two tests record the same session id at the same time: the directory is per call.
    static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let call = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir =
        std::env::temp_dir().join(format!("spot-saw-{}-{call}-{session}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let mut rec = FileRecorder::create(&dir, session, TENANT, T0, 1 << 20).unwrap();
    rec.append(FrameKind::Input, T0, b"ls -l\n", true).unwrap();
    rec.append(FrameKind::Output, T0, b"total 0\n", true)
        .unwrap();
    rec.finish().unwrap();
    let bytes = std::fs::read(FileRecorder::recording_path(&dir, session)).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

fn page() -> Page {
    Page {
        after: None,
        limit: 10,
    }
}

// ---- each port from its real component, or a catalogued refusal -------------------------------

fn sources_with(log: bool, files: bool, vault_present: bool, permissions: bool) -> Sources {
    Sources {
        tenant: TENANT.to_owned(),
        log: log.then(|| Arc::new(self::log()) as Arc<dyn EventLog>),
        files: files.then(|| Arc::new(MemoryFileStore::new()) as Arc<dyn FileStore>),
        vault: vault_present.then(vault),
        permissions: permissions.then(|| {
            (
                Arc::new(Table::default()) as Arc<dyn AppPermissionCheck>,
                Arc::new(Everyone) as Arc<dyn PrincipalSource>,
            )
        }),
    }
}

#[test]
fn every_port_is_provided_from_a_real_component_or_named_missing() {
    let (parts, report) = build_parts(sources_with(true, true, true, true), &Settings::default());
    assert!(parts.log.is_some() && parts.store.is_some() && parts.retire.is_some());
    assert!(parts.sealer.is_some() && parts.duties.is_some());
    assert_eq!(
        report.real,
        ["audit chain", "object store", "sealer", "duty check"]
    );
    assert_eq!(report.refusing, ["agent bridge"]);
    assert!(report.missing.is_empty());
    // The bridge is always provided, as a refusal.
    assert!(parts.bridge.is_some());
    assert!(parts.retention.is_none(), "no retention unless configured");

    let (parts, report) = build_parts(sources_with(false, true, false, true), &Settings::default());
    assert!(parts.log.is_none() && parts.sealer.is_none());
    assert_eq!(report.missing, ["audit chain", "sealer"]);

    let (parts, report) = build_parts(sources_with(true, false, true, false), &Settings::default());
    assert!(parts.store.is_none() && parts.retire.is_none() && parts.duties.is_none());
    assert_eq!(report.missing, ["object store", "duty check"]);
}

#[tokio::test]
async fn a_host_missing_a_required_part_authorises_nothing() {
    let (parts, report) = build_parts(sources_with(true, true, false, true), &Settings::default());
    assert_eq!(report.missing, ["sealer"]);
    let host = BreakGlassHost::new(
        TENANT,
        clock() as Arc<dyn Clock>,
        parts,
        BreakGlassSettings::default(),
    );
    assert_eq!(host.missing(), ["sealer"]);
    assert_eq!(
        host.authorise("s1", T0).await.unwrap_err().code(),
        "break_glass_host.not_wired"
    );
}

#[test]
fn the_configured_retention_is_provided_only_when_set() {
    let settings = Settings {
        retention_days: Some(30),
        ..Settings::default()
    };
    let (parts, report) = build_parts(sources_with(true, true, true, true), &settings);
    assert_eq!(parts.retention.unwrap().recording_days("acme"), Some(30));
    assert!(report.real.contains(&"retention"));
}

// ---- the duty check ---------------------------------------------------------------------------

#[tokio::test]
async fn the_duty_check_refuses_the_unknown_the_unholding_and_other_tenants() {
    let r = rig(None).await;
    open_session(&r, "s1", T0, T0 + 600).await;
    // The administrator and the auditor read; a user holding nothing does not.
    assert!(r.host.summary(&caller(&r.admin), "s1").await.is_ok());
    assert!(r.host.list(&caller(&r.auditor), page()).await.is_ok());
    for who in [r.nobody.as_str(), "not-a-record-id", ""] {
        let err = r.host.summary(&caller(who), "s1").await.unwrap_err();
        assert_eq!(err.code(), "break_glass_host.permission_denied");
    }
    // The auditor may not read a recording or revoke: administrator only.
    assert_eq!(
        r.host
            .recording(&caller(&r.auditor), "s1")
            .await
            .unwrap_err()
            .code(),
        "break_glass_host.permission_denied"
    );
    assert_eq!(
        r.host
            .revoke(&caller(&r.auditor), "s1")
            .await
            .unwrap_err()
            .code(),
        "break_glass_host.permission_denied"
    );
    // Another tenant is refused before the permission engine is asked.
    let foreign = Caller {
        tenant: "other".to_owned(),
        user: r.admin.clone(),
    };
    assert_eq!(
        r.host.summary(&foreign, "s1").await.unwrap_err().code(),
        "break_glass_host.permission_denied"
    );
}

#[tokio::test]
async fn a_duty_is_never_decided_for_another_tenant() {
    let table = Table::default();
    let duties = wiring::PermissionDuties::new(
        TENANT,
        Arc::new(table) as Arc<dyn AppPermissionCheck>,
        Arc::new(Everyone) as Arc<dyn PrincipalSource>,
    );
    let err = duties
        .holds("other", &new_user(), "security_administrator")
        .await
        .unwrap_err();
    assert_eq!(err, HostError::Denied);
}

// ---- grants: no window, no authority; a window ends -------------------------------------------

#[tokio::test]
async fn a_session_with_no_registered_window_is_refused_and_a_grant_expires() {
    let r = rig(None).await;
    // Nothing approved or registered: nothing authorises.
    assert_eq!(
        r.host.authorise("never", T0).await.unwrap_err().code(),
        "break_glass_host.not_found"
    );
    open_session(&r, "s1", T0, T0 + 60).await;
    r.host.authorise("s1", T0 + 10).await.unwrap();
    assert_eq!(
        r.host.authorise("s1", T0 + 60).await.unwrap_err().code(),
        "break_glass.expired"
    );
    // The sweep ends it and the customer sees "expired".
    r.clock.advance(Duration::from_secs(61));
    let report = r.host.sweep().await;
    assert_eq!(report.expired, 1);
    let state = r.host.summary(&caller(&r.admin), "s1").await.unwrap().state;
    assert_eq!(state, "expired");
}

#[tokio::test]
async fn a_revoke_closes_the_session_even_though_no_agent_bridge_exists_yet() {
    let r = rig(None).await;
    open_session(&r, "s1", T0, T0 + 600).await;
    let out = r.host.revoke(&caller(&r.admin), "s1").await.unwrap();
    // The kill is refused (nothing reaches a host agent from here); the session is over.
    assert!(!out.delivered);
    assert!(out.audited);
    assert_eq!(
        r.host.authorise("s1", T0 + 1).await.unwrap_err().code(),
        "break_glass.revoked"
    );
    // The sweep keeps trying and keeps failing closed, without making the session live again.
    let report = r.host.sweep().await;
    assert_eq!(report.kills, 0);
    assert_eq!(
        r.host.authorise("s1", T0 + 2).await.unwrap_err().code(),
        "break_glass.revoked"
    );
}

#[tokio::test]
async fn the_refusing_bridge_answers_with_the_catalogued_code() {
    let err = wiring::RefusingBridge.kill(TENANT, "s1").await.unwrap_err();
    assert_eq!(err.code(), "break_glass_host.not_wired");
}

// ---- recordings: sealed, append-only, retained, then removed ----------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recording_is_sealed_in_the_file_store_and_read_back_audited() {
    let r = rig(None).await;
    open_session(&r, "s1", T0, T0 + 600).await;
    let digest = r
        .host
        .archive_recording("s1", recording_bytes("s1"))
        .await
        .unwrap();
    // It is in the site's private file store, under the break-glass root, and not in the clear.
    let key = spot_files::key::FileKey::new(
        spot_files::key::Visibility::Private,
        "break_glass/recordings/s1.rec",
    )
    .unwrap();
    let site = spot_core::SiteId::new(TENANT).unwrap();
    let (_, mut body) = r.files.get(&site, key).await.unwrap();
    let mut stored = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut body, &mut stored)
        .await
        .unwrap();
    assert!(!stored.windows(5).any(|w| w == b"ls -l"));
    let view = r.host.recording(&caller(&r.admin), "s1").await.unwrap();
    assert_eq!(view.digest, digest);
    // The read is on the chain before the content came back.
    let kinds: Vec<String> = r
        .log
        .entries()
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.record.kind)
        .collect();
    assert!(kinds.contains(&"break_glass.recording_read".to_owned()));
    assert!(kinds.iter().all(|k| k.starts_with("break_glass.")));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_archive_never_overwrites_and_a_recording_outlives_its_session_until_retention() {
    let r = rig(Some(30)).await;
    open_session(&r, "s1", T0, T0 + 600).await;
    r.host
        .archive_recording("s1", recording_bytes("s1"))
        .await
        .unwrap();
    let again = r
        .host
        .archive_recording("s1", recording_bytes("s1"))
        .await
        .unwrap_err();
    assert_eq!(again.code(), "break_glass.append_only");
    r.host.revoke(&caller(&r.admin), "s1").await.unwrap();
    // Within the configured 30 days nothing is removed; after it, the finished session's
    // recording goes.
    assert_eq!(r.host.sweep_at(T0 + 3600).await.retired, 0);
    assert!(r.host.recording(&caller(&r.admin), "s1").await.is_ok());
    assert_eq!(r.host.sweep_at(T0 + 31 * DAY).await.retired, 1);
    assert!(r.host.recording(&caller(&r.admin), "s1").await.is_err());
    // The chain still says the session happened and was revoked.
    let state = r.host.summary(&caller(&r.admin), "s1").await.unwrap().state;
    assert_eq!(state, "revoked");
}

#[tokio::test]
async fn the_sealer_refuses_off_a_multi_thread_runtime_instead_of_running_unsealed() {
    // `#[tokio::test]` is a current-thread runtime: the vault cannot be waited for here.
    let sealer = wiring::VaultSealer::new(vault());
    assert_eq!(
        sealer.seal("ctx", b"recording").unwrap_err(),
        HostError::Seal
    );
    assert_eq!(sealer.open("ctx", b"x").unwrap_err(), HostError::Seal);
}

#[test]
fn the_sealer_refuses_with_no_runtime_at_all() {
    let sealer = wiring::VaultSealer::new(vault());
    assert_eq!(sealer.seal("ctx", b"x").unwrap_err(), HostError::Seal);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sealer_binds_the_context_and_detects_a_changed_byte() {
    let sealer = wiring::VaultSealer::new(vault());
    let sealed = sealer.seal("break_glass:acme:s1", b"top secret\n").unwrap();
    assert!(!sealed.windows(6).any(|w| w == b"secret"));
    assert_eq!(
        sealer.open("break_glass:acme:s1", &sealed).unwrap(),
        b"top secret\n"
    );
    assert_eq!(
        sealer.open("break_glass:acme:s2", &sealed).unwrap_err(),
        HostError::Seal
    );
    let mut changed = sealed.clone();
    let last = changed.len() - 1;
    changed[last] ^= 0xff;
    assert_eq!(
        sealer.open("break_glass:acme:s1", &changed).unwrap_err(),
        HostError::Seal
    );
}

// ---- the object store -------------------------------------------------------------------------

fn objects(max: u64) -> wiring::FileObjects {
    wiring::FileObjects::new(
        Arc::new(MemoryFileStore::new()),
        spot_core::SiteId::new(TENANT).unwrap(),
        max,
    )
}

#[tokio::test]
async fn objects_are_written_once_listed_by_directory_and_removed_only_through_retire() {
    let store = objects(1024);
    store
        .put_new("registry/s1.json", b"one".to_vec())
        .await
        .unwrap();
    store
        .put_new("registry/s2.json", b"two".to_vec())
        .await
        .unwrap();
    store
        .put_new("recordings/s1.rec", b"rec".to_vec())
        .await
        .unwrap();
    // No overwrite, and the old content is untouched.
    let err = store
        .put_new("registry/s1.json", b"changed".to_vec())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "break_glass.append_only");
    assert_eq!(
        store.get("registry/s1.json").await.unwrap().unwrap(),
        b"one"
    );
    assert_eq!(store.get("registry/none.json").await.unwrap(), None);
    // A listing is one directory, in the callers' own spelling.
    let mut registry = store.list("registry/").await.unwrap();
    registry.sort();
    assert_eq!(registry, ["registry/s1.json", "registry/s2.json"]);
    assert_eq!(
        store.list("recordings/").await.unwrap(),
        ["recordings/s1.rec"]
    );
    assert_eq!(
        store.list("registry").await.unwrap_err(),
        HostError::Invalid("a listing names a whole directory")
    );
    store.remove("registry/s1.json").await.unwrap();
    assert_eq!(store.get("registry/s1.json").await.unwrap(), None);
    // Removing what is not there is fine (the retention sweep may run twice).
    store.remove("registry/s1.json").await.unwrap();
}

#[tokio::test]
async fn a_session_id_with_capitals_and_underscores_has_its_own_object() {
    let store = objects(1024);
    for key in [
        "registry/Ab_C.json",
        "registry/ab_c.json",
        "registry/AB__C.json",
    ] {
        store.put_new(key, key.as_bytes().to_vec()).await.unwrap();
    }
    let mut listed = store.list("registry/").await.unwrap();
    listed.sort();
    let mut expected = vec![
        "registry/AB__C.json".to_owned(),
        "registry/Ab_C.json".to_owned(),
        "registry/ab_c.json".to_owned(),
    ];
    expected.sort();
    assert_eq!(listed, expected);
    assert_eq!(
        store.get("registry/Ab_C.json").await.unwrap().unwrap(),
        b"registry/Ab_C.json"
    );
}

#[tokio::test]
async fn an_object_over_the_limit_or_an_unstorable_key_is_refused() {
    let store = objects(8);
    let err = store
        .put_new("recordings/big.rec", vec![0; 9])
        .await
        .unwrap_err();
    assert_eq!(err.code(), "break_glass_host.invalid");
    assert_eq!(store.get("recordings/big.rec").await.unwrap(), None);
    for bad in [
        "registry//x",
        "registry/a b",
        "../x",
        "registry/\u{e9}.json",
    ] {
        assert_eq!(
            store.put_new(bad, b"x".to_vec()).await.unwrap_err().code(),
            "break_glass_host.invalid",
            "{bad}"
        );
    }
}

// ---- the audit chain --------------------------------------------------------------------------

#[tokio::test]
async fn events_are_chained_in_order_under_the_break_glass_kinds_and_carry_no_content() {
    let chain = log();
    for (n, kind) in ["requested", "active", "ended"].iter().enumerate() {
        let event = BreakGlassEvent::new(
            kind,
            "s1",
            TENANT,
            "support-agent",
            "fix a stuck workflow",
            "allow",
            T0 + i64::try_from(n).unwrap(),
        );
        chain.append(event).await.unwrap();
    }
    let entries = chain.entries().await.unwrap();
    let kinds: Vec<&str> = entries.iter().map(|e| e.record.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "break_glass.requested",
            "break_glass.active",
            "break_glass.ended"
        ]
    );
    let seqs: Vec<u64> = entries.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, [1, 2, 3]);
    for entry in &entries {
        assert_eq!(entry.record.actor, "system");
        assert_eq!(
            entry.record.on_behalf_of.as_deref(),
            Some("user:support-agent")
        );
        assert!(entry.record.masked);
    }
    assert_eq!(scope_of("acme"), "break_glass:acme");
}

#[tokio::test]
async fn an_event_that_cannot_be_chained_is_refused_with_the_audit_code() {
    let chain = ChainEventLog::new(InMemoryStore::new(), "break_glass:acme");
    // A time too large for the chain's microsecond clock.
    let event = BreakGlassEvent::new("active", "s1", TENANT, "who", "why", "allow", i64::MAX);
    let err = chain.append(event).await.unwrap_err();
    assert_eq!(err.code(), "break_glass.audit_unavailable");
    assert!(chain.entries().await.unwrap().is_empty());
}

// ---- registration and settings ----------------------------------------------------------------

fn ctx_with(toml: &str) -> SiteCtx<'static> {
    let table: toml::Table = toml.parse().unwrap();
    SiteCtx::new(
        TENANT,
        Duration::from_secs(1),
        clock(),
        table.into_iter().collect(),
        true,
    )
}

#[test]
#[allow(clippy::assertions_on_constants)]
fn the_wiring_starts_before_break_glass_reads_its_ports() {
    assert!(wiring::ORDER < spot_cli::services::break_glass::ORDER);
}

#[tokio::test]
async fn a_context_without_a_site_provides_only_the_refusing_bridge() {
    let mut ctx = ctx_with("");
    wiring::register(&mut ctx).await.unwrap();
    assert!(ctx.port::<dyn AgentBridge>().is_some());
    assert!(ctx.port::<dyn EventLog>().is_none());
    assert!(ctx.port::<dyn ObjectStore>().is_none());
    assert!(ctx.port::<dyn Retire>().is_none());
    assert!(ctx.port::<dyn Sealer>().is_none());
    assert!(ctx.port::<dyn Duties>().is_none());
    assert!(ctx.port::<dyn Retention>().is_none());
}

#[tokio::test]
async fn a_disabled_wiring_provides_nothing() {
    let mut ctx = ctx_with("[support_access_wiring]\nenabled = false\n");
    wiring::register(&mut ctx).await.unwrap();
    assert!(ctx.port::<dyn AgentBridge>().is_none());
}

#[tokio::test]
async fn the_configured_retention_reaches_the_port() {
    let mut ctx = ctx_with("[support_access_wiring]\nretention_days = 90\n");
    wiring::register(&mut ctx).await.unwrap();
    let port = ctx.port::<dyn Retention>().unwrap();
    assert_eq!(port.recording_days(TENANT), Some(90));
}

#[tokio::test]
async fn another_provider_of_a_port_is_an_error_naming_both_modules() {
    let mut ctx = ctx_with("");
    ctx.as_module("other", 1100);
    ctx.provide::<dyn AgentBridge>(Arc::new(wiring::RefusingBridge))
        .unwrap();
    ctx.as_module(wiring::NAME, wiring::ORDER);
    let err = wiring::register(&mut ctx).await.unwrap_err();
    assert!(err.0.contains("other") && err.0.contains(wiring::NAME));
}

#[test]
fn settings_refuse_unknown_keys_and_out_of_bounds_values() {
    let ok: Settings = toml::from_str("retention_days = 5\nmax_object_bytes = 1024").unwrap();
    assert!(ok.validate().is_ok());
    assert!(toml::from_str::<Settings>("retention = 5").is_err());
    for bad in [
        Settings {
            retention_days: Some(0),
            ..Settings::default()
        },
        Settings {
            retention_days: Some(3651),
            ..Settings::default()
        },
        Settings {
            max_object_bytes: 0,
            ..Settings::default()
        },
        Settings {
            max_object_bytes: (1 << 30) + 1,
            ..Settings::default()
        },
    ] {
        let err = bad.validate().unwrap_err().0;
        assert!(
            err.starts_with("support_access_wiring.settings_invalid"),
            "{err}"
        );
    }
}

#[tokio::test]
async fn an_invalid_setting_stops_the_registration_without_echoing_a_value() {
    let mut ctx = ctx_with("[support_access_wiring]\nretention_days = 99999\n");
    let err = wiring::register(&mut ctx).await.unwrap_err().0;
    assert!(err.contains("support_access_wiring.settings_invalid"));
    assert!(!err.contains("99999"));
}

#[test]
fn the_unused_parts_type_still_has_every_port() {
    // A guard that the host's `Parts` has exactly the ports this module fills: a new port added
    // there must be wired or named here.
    let parts = Parts::default();
    assert!(
        parts.log.is_none()
            && parts.store.is_none()
            && parts.retire.is_none()
            && parts.sealer.is_none()
            && parts.duties.is_none()
            && parts.bridge.is_none()
            && parts.retention.is_none()
    );
}
