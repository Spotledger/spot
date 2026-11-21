//! Undo under governance (WP-BPM-15.T5): the engine's own reversal of a governed record runs
//! under a request grant from `GrantHost`, and exactly-once survives a crash between the
//! lifecycle commit and the receipt ledger.
//!
//! Every test needs a real PostgreSQL server (`SPOT_TEST_PG_URL`) and skips, printing
//! `SKIPPED:`, when it is unset (as `tests/process_effects.rs` does). The governance ledger, the
//! grant authority and the effects ledger are real; the document engine is a small fake
//! (`FakeEngine`) that behaves like the governed guard: it accepts a `cancel` only when its
//! command id verifies, through the real authority, for the acting person and an open step, and
//! it keeps a record version so a "crash" can be staged between its commit and the receipt
//! ledger. The undo orchestration itself is `spot-bpm-effects`' and is covered by
//! `tests/process_effects.rs`; this file is about the port stack
//! `DurableReversal(GrantedReversal(engine))`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use deadpool_postgres::Pool;
use spot_app_api::prelude::{OffsetDateTime, RecordId, Version};
use spot_app_api::{BoxFuture, Operation, OperationReceipt};
use spot_bpm_effects::{PortError, RecordRef, Refusal, ReversalCall, ReversalPort, Via};
use spot_bpm_engine::CaseId;
use spot_cli::services::process_effects::{
    DurableReversal, EffLedger, GrantedReversal, UNCONFIRMED, VersionProbe,
};
use spot_cli::services::process_governance::{
    DerivedSecrets, GovernanceHost, GovernedEntry, PrincipalLookup, Settings, SitePolicy, Wiring,
};
use spot_core::{Clock, ManualClock};
use spot_permissions::Principal;
use spot_postgres::PgBackend;
use spot_postgres::testing::TestDb;
use tokio::runtime::Handle;

const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";
const DENIED: &str = "bpm_governance.outside_process_denied";

fn start() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap()
}

// --- fakes ---------------------------------------------------------------------------------------

struct NoRoles;

impl PrincipalLookup for NoRoles {
    fn principal<'a>(
        &'a self,
        actor: &'a spot_app_api::context::Actor,
    ) -> BoxFuture<'a, Principal> {
        Box::pin(async move { Principal::with_roles(&actor.principal_text(), &[]) })
    }
}

/// What the fake engine knows about each record: `(version, cancelled)`.
#[derive(Default)]
struct Records(Mutex<BTreeMap<String, (u64, bool)>>);

impl Records {
    fn put(&self, name: &str, version: u64) {
        self.0
            .lock()
            .unwrap()
            .insert(name.to_owned(), (version, false));
    }
    fn get(&self, name: &str) -> (u64, bool) {
        self.0.lock().unwrap()[name]
    }
}

/// The engine as the guard sees it: a cancel of a governed type needs a command id that
/// verifies for the actor; an ungoverned type takes any command id. `crash_after_commit` makes
/// the call commit and then report the engine down (the process died before the ledger write).
struct FakeEngine {
    host: Arc<GovernanceHost>,
    records: Arc<Records>,
    governed: BTreeSet<String>,
    seen_commands: Mutex<Vec<String>>,
    cancels: Mutex<u32>,
    crash_after_commit: Mutex<bool>,
}

impl ReversalPort for FakeEngine {
    fn reverse(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        self.seen_commands.lock().unwrap().push(call.key.clone());
        if self.governed.contains(&call.record.doctype) {
            let verified = self
                .host
                .grants()
                .authority()
                .authenticate(&call.key, call.principal);
            let ok =
                verified.is_some_and(|v| v.request == call.request.to_string() && v.node == "undo");
            if !ok {
                return Err(PortError::Refused(Refusal {
                    code: DENIED.to_owned(),
                    dependants: Vec::new(),
                    alternatives: Vec::new(),
                }));
            }
        }
        let mut records = self.records.0.lock().unwrap();
        let entry = records.get_mut(&call.record.name).unwrap();
        if entry.0 != call.expected.0 {
            return Err(PortError::Refused(Refusal {
                code: "document.version_conflict".to_owned(),
                dependants: Vec::new(),
                alternatives: Vec::new(),
            }));
        }
        *entry = (entry.0 + 1, true);
        *self.cancels.lock().unwrap() += 1;
        if std::mem::take(&mut *self.crash_after_commit.lock().unwrap()) {
            return Err(PortError::Unavailable("process_died".to_owned()));
        }
        Ok(OperationReceipt {
            operation: Operation::Cancel,
            doctype: call.record.doctype.clone(),
            name: call.record.name.clone(),
            version: Version(entry.0),
            effects: Vec::new(),
            committed_at: call.now,
        })
    }

    fn compensate(&self, call: &ReversalCall<'_>) -> Result<OperationReceipt, PortError> {
        self.reverse(call)
    }
}

struct Probe(Arc<Records>);

impl VersionProbe for Probe {
    fn version(&self, call: &ReversalCall<'_>) -> Result<Option<Version>, PortError> {
        Ok(self
            .0
            .0
            .lock()
            .unwrap()
            .get(&call.record.name)
            .map(|r| Version(r.0)))
    }
}

// --- the site ------------------------------------------------------------------------------------

struct Env {
    _db: TestDb,
    pool: Pool,
    clock: Arc<ManualClock>,
}

async fn env() -> Option<Env> {
    let db = TestDb::create().await?;
    let mut cfg = db.config();
    cfg.application_name = "effects-gov".to_owned();
    cfg.pool_max_size = 6;
    let pool = PgBackend::connect_unchecked(cfg).unwrap().pool();
    Some(Env {
        _db: db,
        pool,
        clock: Arc::new(ManualClock::new(start())),
    })
}

fn settings() -> Settings {
    Settings {
        governed: vec![GovernedEntry {
            doctype: "invoice".to_owned(),
            scope: None,
            mode: "governed".to_owned(),
        }],
        ..Settings::default()
    }
}

struct Stack {
    /// Kept so the ledger tables outlive the stack; nothing reads it back.
    #[allow(dead_code)]
    ledger: EffLedger,
    engine: Arc<FakeEngine>,
    records: Arc<Records>,
    host: Arc<GovernanceHost>,
    durable: Arc<DurableReversal>,
    plain: Arc<DurableReversal>,
}

/// `governed_types` are the types the decorator asks grants for; `engine_governed` the types the
/// fake engine's guard governs (differing sets stage a misconfiguration).
async fn stack(env: &Env, site: &str, governed_types: &[&str], engine_governed: &[&str]) -> Stack {
    let settings = settings();
    let policy = SitePolicy::from_settings(&settings, site).shared();
    let covered = Some(settings.covered());
    let host = Wiring {
        site: site.to_owned(),
        clock: Arc::clone(&env.clock) as Arc<dyn Clock>,
        pool: env.pool.clone(),
        settings,
        policy,
        covered,
        principals: Arc::new(NoRoles),
        outside: Arc::new(spot_bpm_governance::RoleOutsideProcess),
        secrets: Arc::new(DerivedSecrets::fixed(&[("grant-1", KEY)])),
        participants: None,
    }
    .build()
    .unwrap();
    host.ledger().prepare().await.unwrap();
    let host = Arc::new(host);
    let ledger = EffLedger::new(env.pool.clone(), site);
    ledger.prepare().await.unwrap();
    let records = Arc::new(Records::default());
    let engine = Arc::new(FakeEngine {
        host: Arc::clone(&host),
        records: Arc::clone(&records),
        governed: engine_governed.iter().map(|t| (*t).to_owned()).collect(),
        seen_commands: Mutex::default(),
        cancels: Mutex::new(0),
        crash_after_commit: Mutex::new(false),
    });
    let granted = GrantedReversal::new(
        Arc::clone(&engine) as Arc<dyn ReversalPort>,
        Arc::clone(host.grants()),
        governed_types.iter().map(|t| (*t).to_owned()).collect(),
        ledger.clone(),
        Arc::new(Probe(Arc::clone(&records))),
        Handle::current(),
    );
    let durable = Arc::new(DurableReversal::new(
        Arc::new(granted),
        ledger.clone(),
        Handle::current(),
    ));
    // The T4 stack: no decorator.
    let plain = Arc::new(DurableReversal::new(
        Arc::clone(&engine) as Arc<dyn ReversalPort>,
        ledger.clone(),
        Handle::current(),
    ));
    Stack {
        ledger,
        engine,
        records,
        host,
        durable,
        plain,
    }
}

fn principal() -> String {
    format!("user:{}", RecordId::new_v7())
}

/// Runs one reversal on a blocking thread, as the undo does.
#[allow(clippy::too_many_arguments)]
async fn reverse(
    port: Arc<DurableReversal>,
    request: &CaseId,
    doctype: &str,
    name: &str,
    expected: u64,
    key: &str,
    who: &str,
    now: OffsetDateTime,
) -> Result<OperationReceipt, PortError> {
    let (request, record, key, who) = (
        request.clone(),
        RecordRef::new(doctype, name),
        key.to_owned(),
        who.to_owned(),
    );
    tokio::task::spawn_blocking(move || {
        let call = ReversalCall {
            request: &request,
            record: &record,
            expected: Version(expected),
            operation: Operation::Cancel,
            action: None,
            via: Via::Declared,
            principal: &who,
            key,
            now,
        };
        port.reverse(&call)
    })
    .await
    .unwrap()
}

fn refused_code(result: Result<OperationReceipt, PortError>) -> String {
    match result {
        Err(PortError::Refused(refusal)) => refusal.code,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

async fn grant_rows(env: &Env, site: &str) -> i64 {
    let client = env.pool.get().await.unwrap();
    let rows = client
        .query(
            "SELECT count(*) FROM spot_bpm_gov_request WHERE tenant = $1",
            &[&site],
        )
        .await;
    rows.map(|r| r[0].get(0)).unwrap_or(-1)
}

// --- tests ---------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_governed_undo_is_refused_without_the_decorator_and_succeeds_with_it() {
    let Some(env) = env().await else { return };
    let s = stack(&env, "main", &["invoice"], &["invoice"]).await;
    s.records.put("inv-1", 3);
    let (request, who, now) = (
        CaseId::new("request_0000000001").unwrap(),
        principal(),
        start(),
    );
    // The T4 behaviour: the request key is the command id and the guard refuses it.
    let before = reverse(
        s.plain.clone(),
        &request,
        "invoice",
        "inv-1",
        3,
        "undo:r:1:0",
        &who,
        now,
    )
    .await;
    assert_eq!(refused_code(before), DENIED);
    assert_eq!(s.records.get("inv-1"), (3, false));
    // With the decorator the same reversal runs under a grant.
    let receipt = reverse(
        s.durable.clone(),
        &request,
        "invoice",
        "inv-1",
        3,
        "undo:r:1:0",
        &who,
        now,
    )
    .await
    .unwrap();
    assert_eq!(receipt.version, Version(4));
    assert_eq!(s.records.get("inv-1"), (4, true));
    let seen = s.engine.seen_commands.lock().unwrap().clone();
    assert_ne!(
        seen.last().unwrap(),
        "undo:r:1:0",
        "the grant replaced the key"
    );
    assert!(grant_rows(&env, "main").await >= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_ungoverned_type_and_a_site_without_governance_take_the_old_path() {
    let Some(env) = env().await else { return };
    // The decorator governs `invoice` only; a `note` goes through with its own key and opens no
    // grant.
    let s = stack(&env, "main", &["invoice"], &["invoice"]).await;
    s.records.put("note-1", 1);
    let (request, who, now) = (
        CaseId::new("request_0000000002").unwrap(),
        principal(),
        start(),
    );
    reverse(
        s.durable.clone(),
        &request,
        "note",
        "note-1",
        1,
        "undo:r:1:0",
        &who,
        now,
    )
    .await
    .unwrap();
    assert_eq!(
        s.engine.seen_commands.lock().unwrap().as_slice(),
        ["undo:r:1:0"]
    );
    assert_eq!(grant_rows(&env, "main").await, 0, "no grant was opened");
    // Governance off: an empty governed set, the same behaviour for every type.
    let off = stack(&env, "other", &[], &[]).await;
    off.records.put("inv-9", 1);
    reverse(
        off.durable.clone(),
        &request,
        "invoice",
        "inv-9",
        1,
        "undo:r:1:1",
        &who,
        now,
    )
    .await
    .unwrap();
    assert_eq!(
        off.engine.seen_commands.lock().unwrap().as_slice(),
        ["undo:r:1:1"]
    );
    assert_eq!(grant_rows(&env, "other").await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_forged_or_replayed_grant_is_refused_by_the_guard() {
    let Some(env) = env().await else { return };
    let s = stack(&env, "main", &["invoice"], &["invoice"]).await;
    s.records.put("inv-2", 1);
    let (request, who, now) = (
        CaseId::new("request_0000000003").unwrap(),
        principal(),
        start(),
    );
    reverse(
        s.durable.clone(),
        &request,
        "invoice",
        "inv-2",
        1,
        "undo:r:1:0",
        &who,
        now,
    )
    .await
    .unwrap();
    let grant = s
        .engine
        .seen_commands
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    // Another person cannot use it; a changed tag is no grant; a key is no grant.
    let other = principal();
    assert!(
        s.host
            .grants()
            .authority()
            .authenticate(&grant, &other)
            .is_none()
    );
    let mut forged = grant.clone();
    forged.push('x');
    assert!(
        s.host
            .grants()
            .authority()
            .authenticate(&forged, &who)
            .is_none()
    );
    s.records.put("inv-3", 1);
    let direct = reverse(
        s.plain.clone(),
        &request,
        "invoice",
        "inv-3",
        1,
        &grant,
        &other,
        now,
    )
    .await;
    assert_eq!(refused_code(direct), DENIED);
    let keyed = reverse(
        s.plain.clone(),
        &request,
        "invoice",
        "inv-3",
        1,
        "undo:r:2:0",
        &who,
        now,
    )
    .await;
    assert_eq!(refused_code(keyed), DENIED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_request_stops_the_undo_with_the_guards_code() {
    let Some(env) = env().await else { return };
    let s = stack(&env, "main", &["invoice"], &["invoice"]).await;
    s.records.put("inv-4", 1);
    let (request, who, now) = (
        CaseId::new("request_0000000004").unwrap(),
        principal(),
        start(),
    );
    // Open and close the request: the step row is closed.
    reverse(
        s.durable.clone(),
        &request,
        "invoice",
        "inv-4",
        1,
        "undo:r:1:0",
        &who,
        now,
    )
    .await
    .unwrap();
    s.host
        .grants()
        .close_request(&request.to_string())
        .await
        .unwrap();
    s.records.put("inv-5", 1);
    let refused = reverse(
        s.durable.clone(),
        &request,
        "invoice",
        "inv-5",
        1,
        "undo:r:2:0",
        &who,
        now,
    )
    .await;
    let code = refused_code(refused);
    assert!(
        code.starts_with("bpm_request_authority") || code.starts_with("bpm_governance"),
        "the guard's own code text, got {code}"
    );
    assert_eq!(s.records.get("inv-5"), (1, false), "nothing was cancelled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_crash_between_the_commit_and_the_ledger_never_cancels_twice() {
    let Some(env) = env().await else { return };
    let s = stack(&env, "main", &["invoice"], &["invoice"]).await;
    s.records.put("inv-6", 5);
    let (request, who, now) = (
        CaseId::new("request_0000000006").unwrap(),
        principal(),
        start(),
    );
    *s.engine.crash_after_commit.lock().unwrap() = true;
    let first = reverse(
        s.durable.clone(),
        &request,
        "invoice",
        "inv-6",
        5,
        "undo:r:1:0",
        &who,
        now,
    )
    .await;
    assert!(matches!(first, Err(PortError::Unavailable(_))), "{first:?}");
    assert_eq!(s.records.get("inv-6"), (6, true), "the lifecycle committed");
    // The resumed undo finds the started mark and a record that moved on: it stops for a person,
    // never a second cancel.
    let resumed = reverse(
        s.durable.clone(),
        &request,
        "invoice",
        "inv-6",
        5,
        "undo:r:1:0",
        &who,
        now,
    )
    .await;
    assert_eq!(refused_code(resumed), UNCONFIRMED);
    assert_eq!(*s.engine.cancels.lock().unwrap(), 1, "one cancel in total");
    assert_eq!(s.records.get("inv-6"), (6, true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_attempt_that_changed_nothing_may_run_again_with_a_fresh_grant() {
    let Some(env) = env().await else { return };
    let s = stack(&env, "main", &["invoice"], &["invoice"]).await;
    s.records.put("inv-7", 2);
    let (request, who) = (CaseId::new("request_0000000007").unwrap(), principal());
    // A first attempt with a stale expected version is refused by the engine: the started mark
    // exists, the record is unchanged.
    let stale = reverse(
        s.durable.clone(),
        &request,
        "invoice",
        "inv-7",
        1,
        "undo:r:1:0",
        &who,
        start(),
    )
    .await;
    assert_eq!(refused_code(stale), "document.version_conflict");
    // The grant ttl passes; the retry (same key, right version) opens a new grant, not the old.
    let first_grant = s
        .engine
        .seen_commands
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    env.clock.set_wall(start() + time::Duration::minutes(30));
    let later = env.clock.now_utc();
    // The record is still at the version the entry left it at when the entry's version is 2.
    let ok = reverse(
        s.durable.clone(),
        &request,
        "invoice",
        "inv-7",
        2,
        "undo:r:1:0",
        &who,
        later,
    )
    .await;
    assert!(ok.is_ok(), "{ok:?}");
    let second_grant = s
        .engine
        .seen_commands
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    assert_ne!(
        first_grant, second_grant,
        "an expired grant is never reused"
    );
}
