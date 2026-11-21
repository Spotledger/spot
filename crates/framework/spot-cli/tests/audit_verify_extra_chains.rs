//! WP-SEC-06.T9: `audit_verify` also walks the AI trail (`ai:<site>`) and the quarantine trail
//! (`quarantine:<site>`) over the real chain store (real PostgreSQL; the tests print nothing and
//! pass without `SPOT_TEST_PG_URL`).
//!
//! Both chains are written the way the running host writes them (`AiAudit` over `PgAuditStore`,
//! and an `AuditChain` on the quarantine scope with the same checkpoint policy), then altered
//! through a superuser connection with the maintenance switch on, as `audit_verify_cmd.rs` does for
//! the identity chain. The watchman is built in process over the same store: the host cannot turn
//! `[services.audit_verify]` on yet, because no module provides the `AuditKeys` and `AuditAccess`
//! ports (the l3 case `ai_audit: tampering_with_an_ai_entry_is_found_by_the_audit_verifier` names
//! that).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_audit_chain::{
    AuditChain, AuditRecord, CheckpointPolicy, CheckpointSigner, KeyRing, LocalSigner, PayloadValue,
};
use spot_audit_store::PgAuditStore;
use spot_cli::services::ai_audit::{
    ActionKind, AiAction, AiAudit, DenyAll, Outcome, SecretNameMask, Settings as AiSettings,
};
use spot_cli::services::audit_verify::{
    AnchorSink, AuditAccess, AuditKeys, AuditVerifyService, ExtraChain, Mismatch, Saved, Settings,
    StateStore, TracingLog, Wiring,
};
use spot_core::ManualClock;
use spot_doc_audit::lane_site;
use spot_postgres::{PgBackend, PgConfig};
use support::Env;

const ENTRIES: u64 = 5;

fn pg_of(env: &Env) -> PgConfig {
    let loaded = spot_cli::config::load(&env.root, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    loaded.admin.clone().with_dbname(&site.database)
}

async fn backend_of(env: &Env) -> PgBackend {
    PgBackend::connect(pg_of(env)).await.unwrap()
}

async fn tamper(backend: &PgBackend, sql: &str) {
    let client = backend.pool().get().await.unwrap();
    client
        .batch_execute(&format!(
            "BEGIN; SET LOCAL spot.audit_maintenance = 'on'; {sql}; COMMIT"
        ))
        .await
        .unwrap();
}

struct Keys(Arc<LocalSigner>);
impl AuditKeys for Keys {
    fn signer(&self) -> Arc<dyn CheckpointSigner> {
        self.0.clone()
    }
    fn keys(&self) -> KeyRing {
        let mut ring = KeyRing::new();
        ring.add(&self.0.public_key()).unwrap();
        ring
    }
}

struct NoAnchors;
impl AnchorSink for NoAnchors {
    fn append(&self, _line: &str) -> Result<(), String> {
        Ok(())
    }
    fn last(&self) -> Result<Option<String>, String> {
        Ok(None)
    }
}

#[derive(Default)]
struct MemState(Mutex<Option<Saved>>);
impl StateStore for MemState {
    fn load(&self) -> Result<Option<Saved>, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn save(&self, saved: &Saved) -> Result<(), String> {
        *self.0.lock().unwrap() = Some(saved.clone());
        Ok(())
    }
}

struct Nobody;
impl AuditAccess for Nobody {
    fn may(&self, _caller: &str, _export: bool) -> Result<bool, String> {
        Ok(false)
    }
}

fn signer() -> Arc<LocalSigner> {
    Arc::new(LocalSigner::from_seed([8u8; 32]))
}

fn service(
    backend: &PgBackend,
    site: &str,
    state: &Arc<MemState>,
    extra: &[&str],
    slice: u64,
) -> Arc<AuditVerifyService<PgAuditStore>> {
    AuditVerifyService::new(Wiring {
        site: site.to_owned(),
        store: Arc::new(PgAuditStore::new(backend.pool())),
        keys: Arc::new(Keys(signer())),
        anchors: Arc::new(NoAnchors),
        state: state.clone(),
        appender: None,
        access: Arc::new(Nobody),
        log: Arc::new(TracingLog),
        clock: Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH)),
        settings: Settings {
            full_slice_entries: slice,
            extra_chains: extra.iter().map(|name| (*name).to_owned()).collect(),
            ..Settings::default()
        },
    })
}

fn quarantine_record(n: u64) -> AuditRecord {
    let mut payload = BTreeMap::new();
    payload.insert("n".to_owned(), PayloadValue::Int(i64::try_from(n).unwrap()));
    AuditRecord {
        kind: "quarantine_retry".to_owned(),
        actor: "user:u1".to_owned(),
        on_behalf_of: None,
        subject: None,
        at: 1_700_000_000_000_000 + i64::try_from(n).unwrap(),
        payload,
        masked: true,
    }
}

fn ai_action(n: u64) -> AiAction {
    AiAction {
        kind: ActionKind::ToolCall,
        event_id: format!("extra-chains-event-{n}"),
        agent: "bot1".to_owned(),
        acting_user: "someone".to_owned(),
        tool: "update_task".to_owned(),
        arguments: vec![("title".to_owned(), format!("plan {n}"))],
        outcome: Outcome::Ok,
        subject: Some(("task".to_owned(), format!("T-{n}"))),
        refers_to: None,
        at_micros: 1_700_000_000_000_000 + i64::try_from(n).unwrap(),
    }
}

/// `ENTRIES` entries on the AI chain and on the quarantine chain of the site, and the same number
/// on document lane 0, all written through the real store.
async fn populate(env: &Env, backend: &PgBackend) {
    let ai = AiAudit::new(
        PgAuditStore::new(backend.pool()),
        &env.site,
        Arc::new(SecretNameMask),
        Arc::new(DenyAll),
        AiSettings::default(),
    );
    for n in 1..=ENTRIES {
        ai.append(ai_action(n)).await.unwrap();
    }
    let quarantine = AuditChain::new(
        PgAuditStore::new(backend.pool()),
        ExtraChain::Quarantine.scope(&env.site),
        signer(),
        CheckpointPolicy { every: 0 },
    );
    let lane = AuditChain::new(
        PgAuditStore::new(backend.pool()),
        lane_site(&env.site, 0),
        signer(),
        CheckpointPolicy { every: 0 },
    );
    for n in 1..=ENTRIES {
        quarantine.append(quarantine_record(n)).await.unwrap();
        let mut record = quarantine_record(n);
        record.kind = "doc.update".to_owned();
        lane.append(record).await.unwrap();
    }
}

/// The text each chain's entry 3 carries that an alteration can replace, and its replacement.
fn alteration(chain: ExtraChain) -> (&'static str, &'static str) {
    match chain {
        ExtraChain::Ai => ("update_task", "update_taxk"),
        ExtraChain::Quarantine => ("quarantine_retry", "quarantine_retrx"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_clean_ai_chain_and_a_clean_quarantine_chain_verify() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let backend = backend_of(&env).await;
    populate(&env, &backend).await;
    let state = Arc::new(MemState::default());
    let watchman = service(&backend, &env.site, &state, &["ai", "quarantine"], 100);
    let report = watchman.run_incremental().await.unwrap();
    // Three chains of five entries: the document lane and the two extra chains.
    assert_eq!(report.checked, ENTRIES * 3);
    assert!(!report.mismatch);
    assert_eq!(watchman.extra_verified_head(ExtraChain::Ai), ENTRIES);
    assert_eq!(
        watchman.extra_verified_head(ExtraChain::Quarantine),
        ENTRIES
    );
    assert_eq!(watchman.readiness_text(), "ok");
    assert_eq!(watchman.degraded_chain(), None);
    // The full walk of the clean chains is clean too, and a cycle ends.
    for _ in 0..2 {
        assert!(!watchman.run_full_slice().await.unwrap().mismatch);
    }
    assert_eq!(watchman.readiness_text(), "ok");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_altered_a_swapped_and_a_removed_entry_are_found_in_each_extra_chain() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let backend = backend_of(&env).await;
    populate(&env, &backend).await;

    for chain in ExtraChain::ALL {
        let scope = chain.scope(&env.site);
        let (from, to) = alteration(chain);

        // An altered payload.
        tamper(
            &backend,
            &format!(
                "UPDATE spot_audit_chain SET record = replace(record, '{from}', '{to}') \
                 WHERE site = '{scope}' AND seq = 3"
            ),
        )
        .await;
        let state = Arc::new(MemState::default());
        let watchman = service(&backend, &env.site, &state, &[chain.name()], 100);
        watchman.run_incremental().await.unwrap();
        assert_eq!(
            watchman.degraded(),
            Some(Mismatch {
                lane: None,
                seq: 3,
                kind: "audit.hash_mismatch"
            }),
            "{}",
            chain.name()
        );
        assert_eq!(watchman.degraded_chain(), Some(chain));
        assert_eq!(
            watchman.readiness_text(),
            format!(
                "degraded: audit.hash_mismatch in the {} chain entry 3",
                chain.name()
            )
        );
        assert_eq!(
            watchman.extra_verified_head(chain),
            0,
            "the head of a chain that fails does not advance"
        );
        tamper(
            &backend,
            &format!(
                "UPDATE spot_audit_chain SET record = replace(record, '{to}', '{from}') \
                 WHERE site = '{scope}' AND seq = 3"
            ),
        )
        .await;
        let clean = service(
            &backend,
            &env.site,
            &Arc::new(MemState::default()),
            &[chain.name()],
            100,
        );
        clean.run_incremental().await.unwrap();
        assert_eq!(clean.degraded(), None, "{} is clean again", chain.name());

        // Two entries swapped: a broken link at one of them.
        let swap = format!(
            "UPDATE spot_audit_chain SET seq = -3 WHERE site = '{scope}' AND seq = 3; \
             UPDATE spot_audit_chain SET seq = 3 WHERE site = '{scope}' AND seq = 2; \
             UPDATE spot_audit_chain SET seq = 2 WHERE site = '{scope}' AND seq = -3"
        );
        tamper(&backend, &swap).await;
        let watchman = service(
            &backend,
            &env.site,
            &Arc::new(MemState::default()),
            &[chain.name()],
            100,
        );
        watchman.run_incremental().await.unwrap();
        let found = watchman.degraded().expect("a swap is found");
        assert!(found.seq == 2 || found.seq == 3, "{found:?}");
        assert_eq!(watchman.degraded_chain(), Some(chain));
        tamper(&backend, &swap).await;

        // A missing sequence number: the entry after the gap is named.
        tamper(
            &backend,
            &format!("DELETE FROM spot_audit_chain WHERE site = '{scope}' AND seq = 3"),
        )
        .await;
        let watchman = service(
            &backend,
            &env.site,
            &Arc::new(MemState::default()),
            &[chain.name()],
            100,
        );
        watchman.run_incremental().await.unwrap();
        assert_eq!(
            watchman.degraded(),
            Some(Mismatch {
                lane: None,
                seq: 4,
                kind: "audit.seq_gap"
            }),
            "{}",
            chain.name()
        );
        assert_eq!(watchman.degraded_chain(), Some(chain));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_saved_cursor_resumes_mid_chain_after_a_restart() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let backend = backend_of(&env).await;
    populate(&env, &backend).await;
    let scope = ExtraChain::Ai.scope(&env.site);
    let (from, to) = alteration(ExtraChain::Ai);
    tamper(
        &backend,
        &format!(
            "UPDATE spot_audit_chain SET record = replace(record, '{from}', '{to}') \
             WHERE site = '{scope}' AND seq = 4"
        ),
    )
    .await;

    // Slices of two entries: the first slice (entries 1-2) is clean and its cursor is saved.
    let state = Arc::new(MemState::default());
    let first = service(&backend, &env.site, &state, &["ai", "quarantine"], 2);
    let slice = first.run_full_slice().await.unwrap();
    assert!(!slice.mismatch);
    let saved = state.0.lock().unwrap().clone().unwrap();
    assert_eq!(saved.extra_cursor.get("ai"), Some(&3));
    assert_eq!(saved.extra_cursor.get("quarantine"), Some(&3));
    drop(first);

    // A restart over the same saved state continues at entry 3 and finds entry 4.
    let restarted = service(&backend, &env.site, &state, &["ai", "quarantine"], 2);
    let slice = restarted.run_full_slice().await.unwrap();
    assert!(slice.mismatch);
    assert_eq!(restarted.degraded().map(|m| m.seq), Some(4));
    assert_eq!(restarted.degraded_chain(), Some(ExtraChain::Ai));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unknown_name_is_refused_and_an_empty_list_reads_nothing_extra() {
    let bad = Settings {
        extra_chains: vec!["ai".to_owned(), "journal".to_owned()],
        ..Settings::default()
    };
    let error = bad.validate().unwrap_err();
    assert!(error.0.contains("`journal`"), "{}", error.0);

    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let backend = backend_of(&env).await;
    populate(&env, &backend).await;
    let scope = ExtraChain::Ai.scope(&env.site);
    let (from, to) = alteration(ExtraChain::Ai);
    tamper(
        &backend,
        &format!(
            "UPDATE spot_audit_chain SET record = replace(record, '{from}', '{to}') \
             WHERE site = '{scope}' AND seq = 2"
        ),
    )
    .await;
    // Switched off, the altered AI chain is not read: only the document lane is checked.
    let off = service(
        &backend,
        &env.site,
        &Arc::new(MemState::default()),
        &[],
        100,
    );
    assert!(off.extra_chains().is_empty());
    let report = off.run_incremental().await.unwrap();
    assert_eq!(report.checked, ENTRIES);
    assert!(!report.mismatch);
    assert_eq!(off.readiness_text(), "ok");
    // Only the chain that is named is read.
    let quarantine_only = service(
        &backend,
        &env.site,
        &Arc::new(MemState::default()),
        &["quarantine"],
        100,
    );
    quarantine_only.run_incremental().await.unwrap();
    assert_eq!(quarantine_only.degraded(), None);
    let ai_only = service(
        &backend,
        &env.site,
        &Arc::new(MemState::default()),
        &["ai"],
        100,
    );
    ai_only.run_incremental().await.unwrap();
    assert_eq!(ai_only.degraded_chain(), Some(ExtraChain::Ai));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_document_lanes_behave_as_before() {
    let Some(env) = Env::new().await else { return };
    env.bare_site();
    let backend = backend_of(&env).await;
    populate(&env, &backend).await;
    let lane = lane_site(&env.site, 0);
    tamper(
        &backend,
        &format!(
            "UPDATE spot_audit_chain SET record = replace(record, 'doc.update', 'doc.updatx') \
             WHERE site = '{lane}' AND seq = 2"
        ),
    )
    .await;
    let watchman = service(
        &backend,
        &env.site,
        &Arc::new(MemState::default()),
        &["ai", "quarantine"],
        100,
    );
    let report = watchman.run_incremental().await.unwrap();
    assert!(report.mismatch);
    assert_eq!(
        watchman.degraded(),
        Some(Mismatch {
            lane: Some(0),
            seq: 2,
            kind: "audit.hash_mismatch"
        })
    );
    // A lane's mismatch is not an extra chain's: the old readiness text, no chain named.
    assert_eq!(watchman.degraded_chain(), None);
    assert_eq!(
        watchman.readiness_text(),
        "degraded: audit.hash_mismatch at lane 0 entry 2"
    );
    // A degraded service advances no verified head, the extra chains' included.
    assert_eq!(watchman.extra_verified_head(ExtraChain::Ai), 0);
}
