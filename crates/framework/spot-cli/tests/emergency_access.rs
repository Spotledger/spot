//! Emergency access on a running site (WP-IAM-08.T14): request with a reason, approval, the
//! time-boxed assignment with source `emergency`, the recorded session, the exact end, the
//! controller review with its second check and its escalation, restart in every phase, the
//! segregation-of-duties gate with its override, and a stored record that was tampered with.
//!
//! The flow runs over fakes for the process launcher, the gate, the assignment records, the
//! archive and the journal, with the shared in-memory `GovernanceStore` standing in for the
//! durable table: a "restart" is a second `Emergency` over the same store, journal and records,
//! with fresh timers. The PostgreSQL parts (`PgKv`, `PgAssignments`) and the process host are
//! covered by the bundle's live-site run (see the report); nothing here needs a database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_audit_chain::InMemoryStore;
use spot_bpm_engine::InMemoryTimerPort;
use spot_bpm_model::ProcessVersion;
use spot_cli::services::access_questions_model::history::{
    ChainJournal, ChangeJournal, JournalEntry, JournalError, ReadLimits, read_window,
};
use spot_cli::services::emergency_access::flow::{Emergency, Parts, RefusalLog};
use spot_cli::services::emergency_access::model::{
    CODE_APPROVER_NOT_ALLOWED, CODE_AUDIT_UNAVAILABLE, CODE_CONFLICT, CODE_NOT_PERMITTED,
    CODE_OVERRIDE_REQUIRED, CODE_REVIEW_UNASSIGNED, CODE_STATE_INVALID, CODE_STATE_VERSION,
    NewRequest, Settings,
};
use spot_cli::services::emergency_access::seams::{Activation, Assignments, Code, Ids, Recording};
use spot_cli::services::emergency_access::store::{GrantStore, key_of};
use spot_cli::services::governance::{
    ApprovalResult, GovernanceStore, GrantGuard, GrantOutcome, MemoryGovernanceStore,
};
use spot_core::ManualClock;
use spot_http::api::service::BoxFuture;
use spot_iam_governance::approval::LaunchReceipt;
use spot_iam_governance::{
    ChangeEvent, ChangeOp, GovError, GrantRequest, GrantRequestId, MitigatingControl,
    OverrideSubmission, ProcessLauncher,
};
use time::OffsetDateTime;
use time::macros::datetime;

const NOW: OffsetDateTime = datetime!(2026-11-13 12:00 UTC);
const SITE: &str = "site-a";

fn now_s() -> i64 {
    NOW.unix_timestamp()
}

// ---------------------------------------------------------------------------------------------
// The fakes
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
struct Row {
    user: String,
    role: String,
    from: i64,
    to: i64,
    ended: Option<i64>,
}

#[derive(Default)]
struct FakeAssignments {
    rows: Mutex<BTreeMap<String, Row>>,
    roles: Mutex<BTreeMap<String, Vec<String>>>,
    grants: AtomicUsize,
    down: AtomicBool,
}

impl FakeAssignments {
    fn row(&self, grant: &str) -> Option<Row> {
        self.rows.lock().unwrap().get(grant).cloned()
    }
}

impl Assignments for FakeAssignments {
    fn find<'a>(&'a self, grant: &'a str) -> BoxFuture<'a, Result<Option<String>, Code>> {
        Box::pin(async move {
            Ok(self
                .rows
                .lock()
                .unwrap()
                .contains_key(grant)
                .then(|| format!("assignment-{grant}")))
        })
    }

    fn grant<'a>(&'a self, a: &'a Activation) -> BoxFuture<'a, Result<String, Code>> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                return Err("emergency_access_host.write_failed");
            }
            let mut rows = self.rows.lock().unwrap();
            if !rows.contains_key(&a.grant) {
                self.grants.fetch_add(1, Ordering::SeqCst);
                rows.insert(
                    a.grant.clone(),
                    Row {
                        user: a.user.clone(),
                        role: a.role.clone(),
                        from: a.valid_from,
                        to: a.valid_to,
                        ended: None,
                    },
                );
            }
            Ok(format!("assignment-{}", a.grant))
        })
    }

    fn end<'a>(
        &'a self,
        grant: &'a str,
        at: i64,
        _event: &'a ChangeEvent,
    ) -> BoxFuture<'a, Result<(), Code>> {
        Box::pin(async move {
            if let Some(row) = self.rows.lock().unwrap().get_mut(grant)
                && row.ended.is_none()
            {
                row.ended = Some(at.min(row.to));
            }
            Ok(())
        })
    }

    fn roles_of<'a>(&'a self, user: &'a str, _at: i64) -> BoxFuture<'a, Result<Vec<String>, Code>> {
        Box::pin(async move {
            Ok(self
                .roles
                .lock()
                .unwrap()
                .get(user)
                .cloned()
                .unwrap_or_default())
        })
    }
}

/// 0 allowed, 1 blocked, 2 needs an override unless one is attached.
#[derive(Default)]
struct FakeGuard {
    mode: AtomicU8,
    seen: Mutex<Vec<GrantRequest>>,
}

impl GrantGuard for FakeGuard {
    fn decide<'a>(&'a self, _request: GrantRequest) -> BoxFuture<'a, GrantOutcome> {
        Box::pin(async { GrantOutcome::Allowed })
    }
    fn decide_approval<'a>(
        &'a self,
        _request: &'a str,
        _by: &'a str,
        _approve: bool,
        _reason: Option<String>,
    ) -> BoxFuture<'a, ApprovalResult> {
        Box::pin(async { ApprovalResult::Refused("not used".to_owned()) })
    }
    fn recheck_approved<'a>(&'a self, request: GrantRequest) -> BoxFuture<'a, GrantOutcome> {
        Box::pin(async move {
            let over = request.override_submission.is_some() && request.grantor.may_override_sod();
            self.seen.lock().unwrap().push(request);
            match self.mode.load(Ordering::SeqCst) {
                1 => GrantOutcome::Blocked(vec!["sod.rule".to_owned()]),
                2 if !over => GrantOutcome::NeedsOverride(vec!["sod.rule".to_owned()]),
                _ => GrantOutcome::Allowed,
            }
        })
    }
    fn mark_granted<'a>(&'a self, _request: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async { true })
    }
}

#[derive(Default)]
struct FakeLauncher {
    launches: AtomicUsize,
}

impl ProcessLauncher for FakeLauncher {
    fn launch(
        &self,
        _process: &ProcessVersion,
        request: &GrantRequestId,
    ) -> Result<LaunchReceipt, GovError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(LaunchReceipt {
            case_id: format!("case-{}", request.as_str()),
            process_hash: "hash".to_owned(),
        })
    }
}

#[derive(Default)]
struct FakeRecording {
    archived: Mutex<Vec<(String, usize)>>,
    fail: AtomicBool,
}

impl Recording for FakeRecording {
    fn archive<'a>(
        &'a self,
        grant: &'a str,
        _user: &'a str,
        _starts_at: i64,
        _ends_at: i64,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<String, Code>> {
        Box::pin(async move {
            if self.fail.load(Ordering::SeqCst) {
                return Err("emergency_access_host.recording_unavailable");
            }
            self.archived
                .lock()
                .unwrap()
                .push((grant.to_owned(), bytes.len()));
            Ok("digest".to_owned())
        })
    }
}

struct Toggle {
    inner: ChainJournal<InMemoryStore>,
    down: AtomicBool,
}

impl ChangeJournal for Toggle {
    fn head<'a>(&'a self) -> BoxFuture<'a, Result<u64, JournalError>> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                return Err(JournalError::Unavailable);
            }
            self.inner.head().await
        })
    }
    fn range<'a>(
        &'a self,
        from: u64,
        to: u64,
    ) -> BoxFuture<'a, Result<Vec<JournalEntry>, JournalError>> {
        Box::pin(async move { self.inner.range(from, to).await })
    }
    fn append<'a>(&'a self, event: &'a ChangeEvent) -> BoxFuture<'a, Result<u64, JournalError>> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                return Err(JournalError::Unavailable);
            }
            self.inner.append(event).await
        })
    }
}

#[derive(Default)]
struct SeqIds(AtomicUsize);

impl Ids for SeqIds {
    fn next(&self) -> String {
        format!("em-{}", self.0.fetch_add(1, Ordering::SeqCst) + 1)
    }
}

struct World {
    kv: Arc<MemoryGovernanceStore>,
    journal: Arc<Toggle>,
    assignments: Arc<FakeAssignments>,
    guard: Arc<FakeGuard>,
    launcher: Arc<FakeLauncher>,
    recording: Arc<FakeRecording>,
    clock: Arc<ManualClock>,
    ids: Arc<SeqIds>,
    refusals: Arc<RefusalLog>,
}

fn settings() -> Settings {
    Settings {
        enabled: true,
        max_duration_minutes: 60,
        emergency_roles: vec!["Firefighter".to_owned()],
        approvers: vec!["u-owner".to_owned(), "role:Role Owner".to_owned()],
        controllers: vec!["u-ctl".to_owned(), "u-owner".to_owned()],
        approval_sla_minutes: 60,
        review_within_hours: 24,
        escalate_after_hours: 24,
        max_escalations: 3,
        escalate_to: "u-boss".to_owned(),
        second_check: true,
        allow_sod_override: false,
        sweep_seconds: 5,
    }
}

impl World {
    fn new() -> World {
        World {
            kv: Arc::new(MemoryGovernanceStore::default()),
            journal: Arc::new(Toggle {
                inner: ChainJournal::in_memory(SITE),
                down: AtomicBool::new(false),
            }),
            assignments: Arc::new(FakeAssignments::default()),
            guard: Arc::new(FakeGuard::default()),
            launcher: Arc::new(FakeLauncher::default()),
            recording: Arc::new(FakeRecording::default()),
            clock: Arc::new(ManualClock::new(NOW)),
            ids: Arc::new(SeqIds::default()),
            refusals: Arc::new(RefusalLog::default()),
        }
    }

    /// A server of the site: the same store, journal and records, fresh timers.
    fn instance(&self, settings: Settings, launcher: bool, guard: bool) -> Emergency {
        let kv: Arc<dyn GovernanceStore> = Arc::clone(&self.kv) as Arc<dyn GovernanceStore>;
        Emergency::new(Parts {
            site: SITE.to_owned(),
            clock: Arc::clone(&self.clock) as Arc<dyn spot_core::Clock>,
            policy: settings.validate().unwrap(),
            store: GrantStore::new(kv, SITE),
            assignments: Arc::clone(&self.assignments) as Arc<dyn Assignments>,
            guard: guard.then(|| Arc::clone(&self.guard) as Arc<dyn GrantGuard>),
            launcher: launcher
                .then(|| Arc::clone(&self.launcher) as Arc<dyn ProcessLauncher + Send + Sync>),
            journal: Some(Arc::clone(&self.journal) as Arc<dyn ChangeJournal>),
            recording: Arc::clone(&self.recording) as Arc<dyn Recording>,
            ids: Arc::clone(&self.ids) as Arc<dyn Ids>,
            timers: Arc::new(InMemoryTimerPort::new()),
            rights: None,
            refusals: Arc::clone(&self.refusals),
        })
    }

    fn full(&self) -> Emergency {
        self.instance(settings(), true, true)
    }

    fn advance(&self, seconds: u64) {
        self.clock.advance(Duration::from_secs(seconds));
    }
}

fn request(minutes: u64) -> NewRequest {
    NewRequest {
        role: "Firefighter".to_owned(),
        reason: "the ledger is locked and payroll runs in an hour".to_owned(),
        approver: "u-owner".to_owned(),
        minutes,
    }
}

async fn active(world: &World, em: &Emergency, minutes: u64) -> String {
    let id = em.request("u-fire", request(minutes)).await.unwrap().id;
    let view = em.approve(&id, "u-owner", None).await.unwrap();
    assert_eq!(view.phase, "active");
    world
        .assignments
        .row(&id)
        .expect("the assignment was written");
    id
}

// ---------------------------------------------------------------------------------------------
// The happy path and the policy
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_whole_life_from_request_to_a_confirmed_review() {
    let w = World::new();
    let em = w.full();
    let view = em.request("u-fire", request(30)).await.unwrap();
    assert_eq!(view.phase, "awaiting_approval");
    assert_eq!(w.launcher.launches.load(Ordering::SeqCst), 1);
    let id = view.id;

    let view = em.approve(&id, "u-owner", None).await.unwrap();
    assert_eq!(view.phase, "active");
    assert_eq!(view.ends_at, Some(now_s() + 1_800));
    let row = w.assignments.row(&id).unwrap();
    assert_eq!(
        (row.user.as_str(), row.role.as_str()),
        ("u-fire", "Firefighter")
    );
    assert_eq!((row.from, row.to), (now_s(), now_s() + 1_800));

    em.record_action(&id, "u-fire", "read", Some("user u-1"))
        .await
        .unwrap();
    em.record_action(&id, "u-fire", "post", None).await.unwrap();
    assert_eq!(em.status(&id).await.unwrap().actions, 2);

    // The session ends on its own at valid_to; the sweep archives the recording and opens the
    // controller's review.
    w.advance(31 * 60);
    em.sweep().await;
    let view = em.status(&id).await.unwrap();
    assert_eq!(view.phase, "ended");
    assert_eq!(view.recording, "archived");
    assert_eq!(w.recording.archived.lock().unwrap().len(), 1);
    let review = view.review.unwrap();
    assert_eq!(review.controller, "u-ctl");
    assert_eq!(review.checks_required, 1);
    assert_eq!(
        w.assignments.row(&id).unwrap().ended,
        Some(now_s() + 1_800),
        "the grant ended at valid_to, not when the sweep noticed"
    );

    let view = em.review(&id, "u-ctl", true, "").await.unwrap();
    assert_eq!(view.review.unwrap().state, "confirmed");
}

#[tokio::test]
async fn a_duration_over_the_policy_is_refused_and_nothing_is_stored() {
    let w = World::new();
    let em = w.full();
    let refused = em.request("u-fire", request(61)).await.unwrap_err();
    assert_eq!(refused.code, "iam_questions.emergency_duration");
    assert!(em.list().await.unwrap().0.is_empty());
}

#[tokio::test]
async fn a_role_off_the_allow_list_and_an_approver_off_the_policy_are_refused() {
    let w = World::new();
    let em = w.full();
    let mut other = request(10);
    other.role = "Tenant Administrator".to_owned();
    assert_eq!(
        em.request("u-fire", other).await.unwrap_err().code,
        "iam_questions.request_invalid"
    );
    let mut stranger = request(10);
    stranger.approver = "u-nobody".to_owned();
    assert_eq!(
        em.request("u-fire", stranger).await.unwrap_err().code,
        CODE_APPROVER_NOT_ALLOWED
    );
    assert!(em.list().await.unwrap().0.is_empty());
}

#[tokio::test]
async fn the_requester_cannot_approve_and_the_refusal_is_audited() {
    let w = World::new();
    let em = w.full();
    let id = em.request("u-fire", request(10)).await.unwrap().id;
    let refused = em.approve(&id, "u-fire", None).await.unwrap_err();
    assert_eq!(refused.code, "iam_governance.approver_is_grantee");
    assert_eq!(em.status(&id).await.unwrap().phase, "awaiting_approval");
    assert!(
        em.refusals()
            .iter()
            .any(|r| r.by == "u-fire" && r.attempted == "approve"),
        "the refusal wrote its row"
    );
    // Someone who is not the named approver is refused too.
    assert_eq!(
        em.approve(&id, "u-other", None).await.unwrap_err().code,
        "iam_governance.not_the_approver"
    );
}

#[tokio::test]
async fn approving_twice_grants_once() {
    let w = World::new();
    let em = w.full();
    let id = active(&w, &em, 20).await;
    let again = em.approve(&id, "u-owner", None).await.unwrap();
    assert_eq!(again.phase, "active");
    assert_eq!(w.assignments.grants.load(Ordering::SeqCst), 1);
    assert_eq!(w.launcher.launches.load(Ordering::SeqCst), 1);
    // Another person may not "approve again" an approved grant.
    assert_eq!(
        em.approve(&id, "u-ctl", None).await.unwrap_err().code,
        CODE_NOT_PERMITTED
    );
}

#[tokio::test]
async fn a_role_approver_decides_through_the_roles_they_hold() {
    let w = World::new();
    w.assignments
        .roles
        .lock()
        .unwrap()
        .insert("u-ro".to_owned(), vec!["Role Owner".to_owned()]);
    let em = w.full();
    let mut by_role = request(10);
    by_role.approver = "role:Role Owner".to_owned();
    let id = em.request("u-fire", by_role).await.unwrap().id;
    assert!(em.approve(&id, "u-nobody", None).await.is_err());
    assert_eq!(em.approve(&id, "u-ro", None).await.unwrap().phase, "active");
}

// ---------------------------------------------------------------------------------------------
// The end, the review and the escalation
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_end_job_run_twice_is_a_no_op_and_the_user_loses_the_rights_at_once() {
    let w = World::new();
    let em = w.full();
    let id = active(&w, &em, 10).await;
    let row = w.assignments.row(&id).unwrap();
    // The assignment's own window is the control: after `to` it grants nothing whatever the
    // sweep does.
    assert_eq!(row.to, now_s() + 600);
    w.advance(601);
    let first = em.sweep().await;
    assert!(
        first.advanced >= 3,
        "end, archive and open the review: {first:?}"
    );
    let second = em.sweep().await;
    assert_eq!(second.advanced, 0, "a second run changes nothing");
    assert_eq!(second.failed, 0);
    assert_eq!(w.assignments.row(&id).unwrap().ended, Some(now_s() + 600));
}

#[tokio::test]
async fn a_user_may_end_their_session_early_and_a_stranger_may_not() {
    let w = World::new();
    let em = w.full();
    let id = active(&w, &em, 30).await;
    assert_eq!(
        em.end(&id, "u-random").await.unwrap_err().code,
        CODE_NOT_PERMITTED
    );
    w.advance(120);
    let view = em.end(&id, "u-fire").await.unwrap();
    assert_eq!(view.phase, "ended");
    assert_eq!(
        w.assignments.row(&id).unwrap().ended,
        Some(now_s() + 120),
        "the window closed when the session was ended"
    );
    assert!(
        view.review.is_some(),
        "an ended session is reviewed at once"
    );
}

#[tokio::test]
async fn the_reviewer_is_never_the_user_and_the_second_check_is_distinct() {
    let w = World::new();
    // The first eligible controller is the approver: two distinct checks are needed.
    let mut s = settings();
    s.controllers = vec![
        "u-fire".to_owned(),
        "u-owner".to_owned(),
        "u-ctl".to_owned(),
    ];
    let em = w.instance(s, true, true);
    let id = active(&w, &em, 5).await;
    w.advance(301);
    em.sweep().await;
    let review = em.status(&id).await.unwrap().review.unwrap();
    assert_eq!(
        review.controller, "u-owner",
        "the user is skipped as a controller"
    );
    assert_eq!(review.checks_required, 2);
    assert_eq!(
        em.review(&id, "u-fire", true, "").await.unwrap_err().code,
        "iam_questions.review_self"
    );
    assert_eq!(
        em.review(&id, "u-owner", true, "")
            .await
            .unwrap()
            .review
            .unwrap()
            .state,
        "pending",
        "one check by the approver is not enough"
    );
    assert_eq!(
        em.review(&id, "u-owner", true, "").await.unwrap_err().code,
        "iam_questions.review_wrong_person",
        "the same person cannot be the second check"
    );
    assert_eq!(
        em.review(&id, "u-ctl", true, "")
            .await
            .unwrap()
            .review
            .unwrap()
            .state,
        "confirmed"
    );
}

#[tokio::test]
async fn a_flag_needs_a_note_and_with_no_eligible_controller_the_review_is_unassigned() {
    let w = World::new();
    let mut s = settings();
    s.controllers = vec!["u-fire".to_owned()];
    let em = w.instance(s, true, true);
    let id = active(&w, &em, 5).await;
    w.advance(301);
    em.sweep().await;
    let view = em.status(&id).await.unwrap();
    assert!(view.review.is_none());
    assert_eq!(view.blocked.as_deref(), Some(CODE_REVIEW_UNASSIGNED));
    // The sweep says so once and then waits quietly.
    assert_eq!(em.sweep().await.advanced, 0);

    let w2 = World::new();
    let em2 = w2.full();
    let id2 = active(&w2, &em2, 5).await;
    w2.advance(301);
    em2.sweep().await;
    assert!(em2.review(&id2, "u-ctl", false, "  ").await.is_err());
    let flagged = em2
        .review(&id2, "u-ctl", false, "unexpected export")
        .await
        .unwrap();
    assert_eq!(flagged.review.unwrap().state, "flagged");
}

#[tokio::test]
async fn an_unreviewed_task_escalates_once_per_deadline_and_the_escalation_is_audited() {
    let w = World::new();
    let em = w.full();
    let id = active(&w, &em, 5).await;
    w.advance(301);
    em.sweep().await;
    assert_eq!(em.status(&id).await.unwrap().review.unwrap().escalations, 0);
    w.advance(25 * 3600);
    em.sweep().await;
    let review = em.status(&id).await.unwrap().review.unwrap();
    assert_eq!(review.escalations, 1);
    assert_eq!(review.state, "pending");
    em.sweep().await;
    assert_eq!(
        em.status(&id).await.unwrap().review.unwrap().escalations,
        1,
        "the same overdue review does not escalate twice"
    );
    let read = read_window(
        &*w.journal,
        now_s() - 10,
        now_s() + 200_000,
        &ReadLimits::default(),
    )
    .await
    .unwrap();
    assert!(
        read.events
            .iter()
            .any(|e| e.reason.as_deref().is_some_and(|r| r.contains("escalated"))),
        "the escalation is in the journal"
    );
}

// ---------------------------------------------------------------------------------------------
// Restart in each phase
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_restart_resumes_a_waiting_request_a_pending_approval_an_active_grant_and_an_open_review()
{
    let w = World::new();
    // A request made while the process host was down waits; the next server launches it.
    let early = w.instance(settings(), false, true);
    let id = early.request("u-fire", request(10)).await.unwrap().id;
    assert_eq!(early.status(&id).await.unwrap().phase, "requested");
    let second = w.full();
    second.sweep().await;
    assert_eq!(second.status(&id).await.unwrap().phase, "awaiting_approval");
    assert_eq!(w.launcher.launches.load(Ordering::SeqCst), 1);

    // A pending approval is decided by a third server.
    let third = w.full();
    assert_eq!(
        third.approve(&id, "u-owner", None).await.unwrap().phase,
        "active"
    );

    // An active grant ends under a fourth server, which opens the review.
    w.advance(601);
    let fourth = w.full();
    fourth.sweep().await;
    assert_eq!(fourth.status(&id).await.unwrap().phase, "ended");

    // An open review escalates under a fifth server whose timers start empty.
    w.advance(25 * 3600);
    let fifth = w.full();
    fifth.sweep().await;
    assert_eq!(
        fifth.status(&id).await.unwrap().review.unwrap().escalations,
        1
    );
}

// ---------------------------------------------------------------------------------------------
// The segregation-of-duties gate
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_conflict_is_refused_and_nothing_is_granted() {
    let w = World::new();
    w.guard.mode.store(1, Ordering::SeqCst);
    let em = w.full();
    let id = em.request("u-fire", request(10)).await.unwrap().id;
    let view = em.approve(&id, "u-owner", None).await.unwrap();
    assert_eq!(view.phase, "awaiting_approval");
    assert_eq!(view.blocked.as_deref(), Some(CODE_CONFLICT));
    assert!(w.assignments.row(&id).is_none());
    // The gate is asked as an emergency grant, by the approver, with no override right.
    let seen = w.guard.seen.lock().unwrap();
    let asked = seen.last().unwrap();
    assert_eq!(asked.source, spot_iam_roles::Source::Emergency);
    assert!(!asked.grantor.may_override_sod());
}

#[tokio::test]
async fn an_override_is_refused_unless_the_policy_allows_it_and_then_it_needs_a_reason_and_a_control()
 {
    let over = OverrideSubmission::new(
        "payroll cannot wait",
        MitigatingControl::new("CTL-9", "the controller reviews every posting"),
    );
    // Not allowed: refused outright.
    let w = World::new();
    w.guard.mode.store(2, Ordering::SeqCst);
    let em = w.full();
    let id = em.request("u-fire", request(10)).await.unwrap().id;
    assert_eq!(
        em.approve(&id, "u-owner", Some(over.clone()))
            .await
            .unwrap_err()
            .code,
        CODE_CONFLICT
    );

    // Allowed: without an override the grant waits; with an incomplete one it is refused; with a
    // complete one it is granted and the override reaches the gate.
    let w = World::new();
    w.guard.mode.store(2, Ordering::SeqCst);
    let mut s = settings();
    s.allow_sod_override = true;
    let em = w.instance(s, true, true);
    let id = em.request("u-fire", request(10)).await.unwrap().id;
    let waiting = em.approve(&id, "u-owner", None).await.unwrap();
    assert_eq!(waiting.blocked.as_deref(), Some(CODE_OVERRIDE_REQUIRED));
    let incomplete = OverrideSubmission::new("", MitigatingControl::new("", ""));
    assert_eq!(
        em.approve(&id, "u-owner", Some(incomplete))
            .await
            .unwrap_err()
            .code,
        CODE_OVERRIDE_REQUIRED
    );
    let granted = em.approve(&id, "u-owner", Some(over)).await.unwrap();
    assert_eq!(granted.phase, "active");
    let seen = w.guard.seen.lock().unwrap();
    let last = seen.last().unwrap();
    assert!(last.override_submission.is_some());
    assert!(last.grantor.may_override_sod());
}

#[tokio::test]
async fn without_the_gate_nothing_is_granted_and_a_server_with_the_gate_finishes_the_job() {
    let w = World::new();
    let offline = w.instance(settings(), true, false);
    assert_eq!(offline.wired(), (false, true, true));
    let id = offline.request("u-fire", request(10)).await.unwrap().id;
    let view = offline.approve(&id, "u-owner", None).await.unwrap();
    assert_eq!(view.phase, "awaiting_approval", "approved, not activated");
    assert!(w.assignments.row(&id).is_none());
    assert_eq!(offline.sweep().await.failed, 1, "the sweep reports it");
    // The server with the gate activates the same approval.
    let server = w.full();
    server.sweep().await;
    assert_eq!(server.status(&id).await.unwrap().phase, "active");
    assert_eq!(w.assignments.grants.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------------------------
// Audit, recording and failure
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_journal_has_every_change_in_order() {
    let w = World::new();
    let em = w.full();
    let id = active(&w, &em, 10).await;
    w.advance(601);
    em.sweep().await;
    em.review(&id, "u-ctl", true, "").await.unwrap();
    let read = read_window(
        &*w.journal,
        now_s() - 10,
        now_s() + 100_000,
        &ReadLimits::default(),
    )
    .await
    .unwrap();
    let ops: Vec<ChangeOp> = read.events.iter().map(|e| e.op).collect();
    assert_eq!(
        ops,
        vec![
            ChangeOp::Updated, // requested
            ChangeOp::Updated, // approval started
            ChangeOp::Updated, // approved
            ChangeOp::Created, // the assignment
            ChangeOp::Ended,   // at valid_to
            ChangeOp::Updated, // recording archived
            ChangeOp::Updated, // review opened
            ChangeOp::Updated, // review confirmed
        ]
    );
    let created = &read.events[3];
    assert_eq!(created.user.as_deref(), Some("u-fire"));
    assert_eq!(created.role.as_deref(), Some("Firefighter"));
    assert_eq!(created.source, Some(spot_iam_roles::Source::Emergency));
    assert_eq!(read.events[4].cause.as_deref(), Some("expired"));
    assert_eq!(read.skipped, 0);
}

#[tokio::test]
async fn an_unwritable_journal_grants_nothing() {
    let w = World::new();
    let em = w.full();
    w.journal.down.store(true, Ordering::SeqCst);
    assert_eq!(
        em.request("u-fire", request(10)).await.unwrap_err().code,
        CODE_AUDIT_UNAVAILABLE
    );
    assert!(em.list().await.unwrap().0.is_empty());

    // A journal that fails after the request: the approval is refused, the grant is not made.
    w.journal.down.store(false, Ordering::SeqCst);
    let id = em.request("u-fire", request(10)).await.unwrap().id;
    w.journal.down.store(true, Ordering::SeqCst);
    assert_eq!(
        em.approve(&id, "u-owner", None).await.unwrap_err().code,
        CODE_AUDIT_UNAVAILABLE
    );
    assert!(w.assignments.row(&id).is_none());
    assert_eq!(em.status(&id).await.unwrap().phase, "awaiting_approval");
}

#[tokio::test]
async fn a_recording_that_cannot_be_archived_says_so_and_the_review_still_opens() {
    let w = World::new();
    w.recording.fail.store(true, Ordering::SeqCst);
    let em = w.full();
    let id = active(&w, &em, 5).await;
    em.record_action(&id, "u-fire", "read", None).await.unwrap();
    w.advance(301);
    em.sweep().await;
    let view = em.status(&id).await.unwrap();
    assert_eq!(view.recording, "unavailable");
    assert!(view.review.is_some());
    assert_eq!(view.actions, 1, "the actions stay in the record");
}

#[tokio::test]
async fn only_the_user_records_actions_and_only_while_the_session_runs() {
    let w = World::new();
    let em = w.full();
    let id = active(&w, &em, 5).await;
    assert_eq!(
        em.record_action(&id, "u-owner", "read", None)
            .await
            .unwrap_err()
            .code,
        CODE_NOT_PERMITTED
    );
    w.advance(301);
    em.sweep().await;
    assert!(em.record_action(&id, "u-fire", "read", None).await.is_err());
}

// ---------------------------------------------------------------------------------------------
// A stored record that was changed
// ---------------------------------------------------------------------------------------------

async fn stored_text(w: &World, id: &str) -> String {
    w.kv.get(&key_of(id)).await.unwrap().unwrap()
}

async fn replace(w: &World, key: &str, old: &str, new: &str) {
    assert!(w.kv.compare_put(key, Some(old), new).await.unwrap());
}

#[tokio::test]
async fn a_tampered_record_refuses_to_load() {
    let w = World::new();
    let em = w.full();
    let id = em.request("u-fire", request(10)).await.unwrap().id;
    let text = stored_text(&w, &id).await;

    // An edit that forgot the digest.
    let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v["body"]["grant"]["request"]["duration_secs"] = serde_json::json!(86_400);
    let edited = v.to_string();
    replace(&w, &key_of(&id), &text, &edited).await;
    assert_eq!(em.status(&id).await.unwrap_err().code, CODE_STATE_INVALID);
    assert_eq!(
        em.approve(&id, "u-owner", None).await.unwrap_err().code,
        CODE_STATE_INVALID
    );
    let report = em.sweep().await;
    assert_eq!(
        report.failed + report.unreadable,
        1,
        "the sweep names it, never repairs it"
    );
    replace(&w, &key_of(&id), &edited, &text).await;
    assert!(em.status(&id).await.is_ok());

    // A version this release does not read.
    let mut v: serde_json::Value = serde_json::from_str(&text).unwrap();
    v["body"]["v"] = serde_json::json!(2);
    let future = v.to_string();
    replace(&w, &key_of(&id), &text, &future).await;
    assert_eq!(em.status(&id).await.unwrap_err().code, CODE_STATE_VERSION);
    replace(&w, &key_of(&id), &future, &text).await;

    // A record copied under another key (or another site) answers as a damaged one.
    assert!(
        w.kv.compare_put(&key_of("other"), None, &text)
            .await
            .unwrap()
    );
    assert_eq!(
        em.status("other").await.unwrap_err().code,
        CODE_STATE_INVALID
    );
    let elsewhere = GrantStore::new(Arc::clone(&w.kv) as Arc<dyn GovernanceStore>, "site-b");
    assert_eq!(
        elsewhere.load(&id).await.unwrap_err().code,
        CODE_STATE_INVALID
    );
}

// ---------------------------------------------------------------------------------------------
// The settings
// ---------------------------------------------------------------------------------------------

#[test]
fn a_blank_or_malformed_escalation_target_and_open_lists_refuse_to_start() {
    assert!(settings().validate().is_ok());
    let mut s = settings();
    s.escalate_to = "  ".to_owned();
    assert_eq!(s.validate().unwrap_err(), "escalate_to");
    s.escalate_to = "u-boss\u{7}".to_owned();
    assert_eq!(s.validate().unwrap_err(), "escalate_to");
    s.escalate_to = String::new();
    assert_eq!(s.validate().unwrap_err(), "escalate_to");
    for (edit, name) in [
        (
            Box::new(|s: &mut Settings| s.emergency_roles.clear()) as Box<dyn Fn(&mut Settings)>,
            "emergency_roles",
        ),
        (
            Box::new(|s: &mut Settings| s.approvers.clear()),
            "approvers",
        ),
        (
            Box::new(|s: &mut Settings| s.controllers.clear()),
            "controllers",
        ),
        (
            Box::new(|s: &mut Settings| s.max_duration_minutes = 0),
            "max_duration_minutes",
        ),
        (
            Box::new(|s: &mut Settings| s.max_duration_minutes = 1441),
            "max_duration_minutes",
        ),
        (
            Box::new(|s: &mut Settings| s.max_escalations = 0),
            "max_escalations",
        ),
        (
            Box::new(|s: &mut Settings| s.sweep_seconds = 0),
            "sweep_seconds",
        ),
    ] {
        let mut s = settings();
        edit(&mut s);
        assert_eq!(s.validate().unwrap_err(), name);
    }
    assert!(!Settings::default().enabled, "off by default");
}
