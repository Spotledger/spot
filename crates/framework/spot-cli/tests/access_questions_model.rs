//! The access-question model on the live permission model (WP-IAM-08.T13).
//!
//! The model is built from stored role, assignment and user records and the installed apps' grants,
//! and must agree with the permission engine. Most tests run over a fixed directory and fixed site
//! inputs (the model is a pure function of them); the PostgreSQL reader is exercised against a real
//! site and skipped, with a printed reason, when `SPOT_TEST_PG_URL` is not set.
//!
//! - **Agreement**: a randomised comparison over seeded matrices of (user, DocType, action, owner):
//!   the model's decision equals the decision of an engine built independently from a hand-made
//!   registry of the same roles, and (for roles without a parent) equals the decision of the site's
//!   own engine over role-named grants.
//! - **The four questions** return the expected rows with reasons; a portal user never shows a desk
//!   right; a change between two reads shows after the next snapshot version, not before.
//! - **History** lists the seeded grant, revoke and edit in order; an entry the reader cannot
//!   interpret is skipped and counted; an unreadable journal refuses that one question only.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::too_many_lines
)]

mod support;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spot_cli::services::access_questions_model::assemble::{
    AssembleInput, GrantInputs, RawAssignment, RawDirectory, RawRole, RawUser, assemble,
};
use spot_cli::services::access_questions_model::directory::{DirectoryReader, FixedDirectory};
use spot_cli::services::access_questions_model::history::{
    ChainJournal, ChangeJournal, JournalEntry, JournalError, ReadLimits, record,
};
use spot_cli::services::access_questions_model::live::{
    LiveAccessModel, LiveModelSource, ModelSettings, SiteInputs, SiteView,
};
use spot_cli::services::access_questions_model::{
    CODE_DUTIES_UNDEFINED, CODE_HISTORY_UNAVAILABLE, ModelError,
};
use spot_cli::services::roles::{Published, RoleData, RoleSnapshots};
use spot_core::ManualClock;
use spot_events_contract::lifecycle::Operation;
use spot_http::api::service::BoxFuture;
use spot_iam_governance::{ChangeActor, ChangeEvent, ChangeOp};
use spot_iam_questions::{MemoryAuditor, Page, PermissionSpec, Questions, ScopeQuery, build};
use spot_iam_roles::{
    Assignment, Origin, OutsideGuard, Registry, Role, RoleName, Source, Version,
    build_snapshot_input,
};
use spot_permissions::{
    Action, DocFacts, Model, ModelTexts, PermissionEngine, Principal, Role as GrantRole, RoleGrant,
    Snapshot,
};
use spot_sod::RuleSet;
use time::OffsetDateTime;
use time::macros::datetime;

const NOW: OffsetDateTime = datetime!(2026-11-13 12:00 UTC);

const GRANTS: &str = r#"
[[entries]]
role = "System Manager"
doctype = "user"
rights = ["read", "write", "create", "report", "select"]
[[entries]]
role = "System Manager"
doctype = "role"
rights = ["read", "write", "create", "delete"]
[[entries]]
role = "Support Agent"
doctype = "user"
rights = ["read", "select"]
[[entries]]
role = "Support Agent"
doctype = "role"
rights = ["read"]
[[entries]]
role = "Own Records"
doctype = "role_assignment"
rights = ["read", "write"]
if_owner = true
[[entries]]
role = "Auditor"
doctype = "role_assignment"
rights = ["read", "report"]
[[entries]]
role = "Auditor"
doctype = "user"
rights = ["read"]
[[entries]]
role = "Reviewer"
doctype = "user"
rights = ["write"]
"#;

const FINANCE_GRANTS: &str = r#"
[[entries]]
role = "Finance Clerk"
doctype = "role_assignment"
rights = ["create"]
"#;

const DOCTYPES: [&str; 3] = ["user", "role", "role_assignment"];

/// (name, parent, reference pack).
const ROLES: [(&str, Option<&str>, Option<&str>); 6] = [
    ("System Manager", None, Some("platform")),
    ("Support Agent", None, Some("platform")),
    ("Auditor", None, None),
    ("Reviewer", Some("Support Agent"), None),
    ("Own Records", None, None),
    ("Finance Clerk", None, Some("finance")),
];

fn meta() -> Arc<spot_meta::EffectiveMeta> {
    let definitions = spot_cli::appdir::platform_definitions().unwrap();
    spot_meta::Registry::new(definitions).unwrap().effective()
}

fn loaded(text: &str) -> Vec<RoleGrant> {
    spot_permissions::load(&ModelTexts {
        grants: text.to_owned(),
        ..ModelTexts::default()
    })
    .unwrap()
    .grants
}

fn grant_inputs() -> GrantInputs {
    let mut grants: Vec<(String, RoleGrant)> = loaded(GRANTS)
        .into_iter()
        .map(|g| ("platform".to_owned(), g))
        .collect();
    grants.extend(
        loaded(FINANCE_GRANTS)
            .into_iter()
            .map(|g| ("finance".to_owned(), g)),
    );
    GrantInputs {
        grants,
        ..GrantInputs::default()
    }
}

fn rid(name: &str) -> String {
    format!("role-{}", name.replace(' ', "-").to_lowercase())
}

fn raw_role(name: &str, parent: Option<&str>, reference: bool) -> RawRole {
    RawRole {
        id: rid(name),
        name: name.to_owned(),
        origin: Some(if reference { "reference" } else { "custom" }.to_owned()),
        is_system: reference,
        parents: parent.map(str::to_owned).into_iter().collect(),
        ..RawRole::default()
    }
}

fn raw_assignment(
    id: &str,
    user: &str,
    role: &str,
    state: &str,
    window: (Option<OffsetDateTime>, Option<OffsetDateTime>),
) -> RawAssignment {
    RawAssignment {
        id: id.to_owned(),
        user: user.to_owned(),
        role_id: rid(role),
        source: "manual".to_owned(),
        state: state.to_owned(),
        valid_from: window.0,
        valid_to: window.1,
        ..RawAssignment::default()
    }
}

fn user(id: &str, user_type: &str) -> RawUser {
    RawUser {
        id: id.to_owned(),
        user_type: user_type.to_owned(),
    }
}

fn core_of(directory: &RawDirectory) -> LiveAccessModel {
    let grants = grant_inputs();
    let core = assemble(&AssembleInput {
        directory,
        grants: &grants,
        duties: &[],
        rules: &RuleSet::default(),
        meta: meta(),
    })
    .unwrap();
    LiveAccessModel::new(Arc::new(core))
}

// ---------------------------------------------------------------------------------------------
// The seeded directory of the deterministic tests
// ---------------------------------------------------------------------------------------------

fn seeded() -> RawDirectory {
    let mut roles: Vec<RawRole> = ROLES
        .iter()
        .map(|(name, parent, pack)| raw_role(name, *parent, pack.is_some()))
        .collect();
    let mut governance = raw_role("Governance", None, false);
    governance.duties = vec![("Access Governance".to_owned(), None)];
    roles.push(governance);
    let mut portal_role = raw_role("Customer Portal", None, false);
    portal_role.kind = Some("portal".to_owned());
    roles.push(portal_role);
    let past = Some(NOW - time::Duration::days(3));
    RawDirectory {
        roles,
        assignments: vec![
            raw_assignment("a-admin", "u-admin", "Governance", "active", (None, None)),
            raw_assignment("a-ana", "u-ana", "System Manager", "active", (None, None)),
            raw_assignment("a-ben", "u-ben", "Reviewer", "active", (None, None)),
            raw_assignment("a-cho", "u-cho", "Own Records", "active", (None, None)),
            // Ended, and expired by its window: neither counts.
            raw_assignment("a-cho-2", "u-cho", "Support Agent", "ended", (None, None)),
            raw_assignment("a-ana-2", "u-ana", "Auditor", "active", (None, past)),
            // A portal user with a desk role (refused by the portal rule) and with a portal role.
            raw_assignment("a-pat", "u-pat", "Support Agent", "active", (None, None)),
            raw_assignment(
                "a-pat-2",
                "u-pat",
                "Customer Portal",
                "active",
                (None, None),
            ),
        ],
        users: vec![
            user("u-admin", "internal"),
            user("u-ana", "internal"),
            user("u-ben", "internal"),
            user("u-cho", "internal"),
            user("u-pat", "portal"),
        ],
    }
}

fn rows_of(model: &LiveAccessModel, user: &str) -> Vec<spot_iam_questions::PermissionRow> {
    let auditor = MemoryAuditor::new();
    Questions::new(model, &auditor)
        .what_can_user_do("u-admin", user, NOW, Page::new(0, 500).unwrap())
        .unwrap()
        .items
}

#[test]
fn what_can_this_user_do_names_the_assignment_role_source_and_the_app() {
    let model = core_of(&seeded());
    let rows = rows_of(&model, "u-ana");
    let read_user = rows
        .iter()
        .find(|r| r.doctype == "user" && r.right == "read")
        .expect("System Manager may read user");
    assert_eq!(read_user.assignment, "a-ana");
    assert_eq!(read_user.role, "System Manager");
    assert_eq!(read_user.role_path, vec!["System Manager".to_owned()]);
    assert_eq!(read_user.source, "manual");
    assert_eq!(read_user.duty, None, "a direct grant, not a duty's");
    // The reference role says which app shipped it: the reason "granted directly by role System
    // Manager from app platform".
    let role = model
        .core()
        .registry
        .role(&RoleName::from("System Manager"))
        .unwrap();
    assert!(matches!(&role.origin, Origin::Reference { pack, .. } if pack == "platform"));
    assert_eq!(role.direct_grants.len(), 2);
    let clerk = model
        .core()
        .registry
        .role(&RoleName::from("Finance Clerk"))
        .unwrap();
    assert!(
        matches!(&clerk.origin, Origin::Reference { pack, .. } if pack == "finance"),
        "a role only an app names is a reference role of that app"
    );
    // The expired Auditor assignment of the same user adds nothing.
    assert!(rows.iter().all(|r| r.assignment != "a-ana-2"));
}

#[test]
fn a_role_with_a_parent_gives_its_parents_rights_with_the_path() {
    let model = core_of(&seeded());
    let rows = rows_of(&model, "u-ben");
    let inherited = rows
        .iter()
        .find(|r| r.doctype == "role" && r.right == "read")
        .expect("Reviewer inherits Support Agent");
    assert_eq!(
        inherited.role_path,
        vec!["Reviewer".to_owned(), "Support Agent".to_owned()]
    );
    assert!(
        rows.iter()
            .any(|r| r.doctype == "user" && r.right == "write")
    );
}

#[test]
fn an_inactive_or_expired_assignment_gives_nothing_and_a_if_owner_grant_says_so() {
    let model = core_of(&seeded());
    let rows = rows_of(&model, "u-cho");
    assert!(
        rows.iter().all(|r| r.assignment != "a-cho-2"),
        "an ended assignment is not a right"
    );
    let own = rows
        .iter()
        .find(|r| r.doctype == "role_assignment" && r.right == "read")
        .unwrap();
    assert!(own.if_owner);
}

#[test]
fn a_portal_user_never_shows_a_desk_right() {
    let model = core_of(&seeded());
    assert_eq!(model.core().portal, vec!["u-pat".to_owned()]);
    assert!(
        rows_of(&model, "u-pat").is_empty(),
        "Barrier 3: the desk role of a portal user grants nothing in an answer"
    );
    let auditor = MemoryAuditor::new();
    let holders = Questions::new(&model, &auditor)
        .who_can_do(
            "u-admin",
            &PermissionSpec::parse("user.read").unwrap(),
            &ScopeQuery::everywhere(),
            NOW,
            Page::new(0, 500).unwrap(),
        )
        .unwrap();
    assert!(holders.items.iter().all(|h| h.user != "u-pat"));
}

#[test]
fn who_can_do_lists_every_holder_with_the_reason() {
    let model = core_of(&seeded());
    let auditor = MemoryAuditor::new();
    let holders = Questions::new(&model, &auditor)
        .who_can_do(
            "u-admin",
            &PermissionSpec::parse("user.read").unwrap(),
            &ScopeQuery::everywhere(),
            NOW,
            Page::new(0, 500).unwrap(),
        )
        .unwrap();
    // One row per place that grants the right (a role path), not one per user: Reviewer holds
    // `user.write` itself and `user.read` through Support Agent, and the grant the model resolves
    // for the DocType merges the two, so Ben is listed once for each of his paths.
    let who: std::collections::BTreeSet<&str> =
        holders.items.iter().map(|h| h.user.as_str()).collect();
    assert_eq!(who.into_iter().collect::<Vec<_>>(), vec!["u-ana", "u-ben"]);
    assert!(
        holders
            .items
            .iter()
            .any(|h| h.user == "u-ben" && h.role_path == ["Reviewer", "Support Agent"]),
        "the inherited path is among Ben's reasons"
    );
    assert!(holders.items.iter().all(|h| !h.rule.is_empty()));
}

#[test]
fn only_a_holder_of_the_governance_duty_may_ask_about_others() {
    let model = core_of(&seeded());
    let auditor = MemoryAuditor::new();
    let questions = Questions::new(&model, &auditor);
    assert!(
        questions
            .what_can_user_do("u-cho", "u-ana", NOW, Page::first())
            .is_err()
    );
    assert!(
        questions
            .what_can_user_do("u-cho", "u-cho", NOW, Page::first())
            .is_ok(),
        "anyone may ask about themselves"
    );
}

#[test]
fn a_duty_no_app_defines_is_noted_and_empty() {
    let model = core_of(&seeded());
    assert!(
        model
            .notes()
            .iter()
            .any(|n| n.code == CODE_DUTIES_UNDEFINED && n.count == 1)
    );
}

// ---------------------------------------------------------------------------------------------
// The agreement with the permission engine
// ---------------------------------------------------------------------------------------------

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

struct Case {
    user: String,
    role: &'static str,
    state: &'static str,
    window: (Option<OffsetDateTime>, Option<OffsetDateTime>),
}

/// The engine built the way the roles service builds it, from a registry written by hand here.
fn independent_engine(cases: &[Case]) -> (PermissionEngine, spot_iam_roles::SnapshotInput) {
    let grants = grant_inputs();
    let roles: Vec<Role> = ROLES
        .iter()
        .map(|(name, parent, pack)| {
            let mut role = Role::custom(name);
            if let Some(pack) = pack {
                role.origin = Origin::Reference {
                    pack: (*pack).to_owned(),
                    version: Version(1),
                };
            }
            role.parents = parent.map(RoleName::from).into_iter().collect();
            role.direct_grants = grants
                .grants
                .iter()
                .filter(|(_, g)| g.role.as_str() == *name)
                .map(|(_, g)| g.clone())
                .collect();
            role
        })
        .collect();
    let registry = Registry::new(Vec::new(), roles).unwrap();
    let assignments: Vec<Assignment> = cases
        .iter()
        .filter(|c| c.state == "active")
        .enumerate()
        .map(|(i, c)| {
            let mut a = Assignment::new(&format!("x{i}"), &c.user, c.role);
            a.valid_from = c.window.0;
            a.valid_to = c.window.1;
            a
        })
        .collect();
    let input = build_snapshot_input(&registry, &assignments, NOW).unwrap();
    let snapshot = Snapshot::build(&meta(), &input.model(Vec::new(), Vec::new())).unwrap();
    (PermissionEngine::new(Arc::new(snapshot)), input)
}

/// The site's own engine: grants keyed by role name, no inheritance (`permissions::engine`).
fn site_engine() -> PermissionEngine {
    let model = Model {
        grants: grant_inputs().grants.into_iter().map(|(_, g)| g).collect(),
        ..Model::default()
    };
    PermissionEngine::new(Arc::new(Snapshot::build(&meta(), &model).unwrap()))
}

fn doc(doctype: &str, creator: &str) -> DocFacts {
    DocFacts {
        doctype: doctype.to_owned(),
        created_by: Some(creator.to_owned()),
        ..DocFacts::default()
    }
}

#[test]
fn the_model_agrees_with_the_permission_engine_over_random_matrices() {
    let actions = [
        Action::Read,
        Action::Op(Operation::Create),
        Action::Op(Operation::Update),
        Action::Op(Operation::Delete),
    ];
    let users = ["u0", "u1", "u2", "u3", "u4", "u5"];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut compared = 0_u64;
    let mut allowed_somewhere = 0_u64;
    for _ in 0..40 {
        // A random matrix: each user holds up to three distinct roles in random states and windows.
        let mut cases: Vec<Case> = Vec::new();
        for u in users {
            let mut taken: Vec<usize> = Vec::new();
            for _ in 0..rng.below(4) {
                let r = usize::try_from(rng.below(ROLES.len() as u64)).unwrap();
                if taken.contains(&r) {
                    continue;
                }
                taken.push(r);
                let state = match rng.below(10) {
                    0..=6 => "active",
                    7..=8 => "ended",
                    _ => "expired",
                };
                let window = match rng.below(4) {
                    0 => (None, Some(NOW - time::Duration::hours(1))),
                    1 => (
                        Some(NOW + time::Duration::hours(1)),
                        Some(NOW + time::Duration::hours(2)),
                    ),
                    2 => (None, Some(NOW + time::Duration::hours(1))),
                    _ => (None, None),
                };
                cases.push(Case {
                    user: u.to_owned(),
                    role: ROLES[r].0,
                    state,
                    window,
                });
            }
        }
        let directory = RawDirectory {
            roles: ROLES
                .iter()
                .map(|(name, parent, pack)| raw_role(name, *parent, pack.is_some()))
                .collect(),
            assignments: cases
                .iter()
                .enumerate()
                .map(|(i, c)| raw_assignment(&format!("x{i}"), &c.user, c.role, c.state, c.window))
                .collect(),
            users: users.iter().map(|u| user(u, "internal")).collect(),
        };
        let model = core_of(&directory);
        let world = build(&model, NOW).unwrap();
        let (engine_a, input_a) = independent_engine(&cases);
        let engine_c = site_engine();
        for u in users {
            let in_model = world.principal(u);
            let in_engine = input_a.principal(u);
            // The site's engine reads a user's assignments by role name, with no inheritance.
            let flat = !cases
                .iter()
                .any(|c| c.user == u && c.state == "active" && c.role == "Reviewer");
            let by_name = Principal::user(
                u,
                in_engine
                    .assignments
                    .iter()
                    .map(|a| {
                        let mut a = a.clone();
                        a.roles = a
                            .roles
                            .iter()
                            .map(|r| GrantRole(r.as_str().to_owned()))
                            .collect();
                        a
                    })
                    .collect(),
            );
            for doctype in DOCTYPES {
                for action in actions {
                    for creator in [u, "someone-else"] {
                        let facts = doc(doctype, creator);
                        let b = world.engine.can(&in_model, action, &facts).allowed;
                        let a = engine_a.can(&in_engine, action, &facts).allowed;
                        assert_eq!(
                            b, a,
                            "{u} {doctype} {action:?} by {creator}: model vs engine"
                        );
                        if flat {
                            let c = engine_c.can(&by_name, action, &facts).allowed;
                            assert_eq!(b, c, "{u} {doctype} {action:?}: model vs the site engine");
                        }
                        compared += 1;
                        allowed_somewhere += u64::from(b);
                    }
                }
            }
        }
    }
    assert!(compared > 1_000);
    assert!(
        allowed_somewhere > 100,
        "the matrices must allow something, or the comparison proves nothing"
    );
}

// ---------------------------------------------------------------------------------------------
// The source: caching per snapshot version, fail closed
// ---------------------------------------------------------------------------------------------

struct Inputs(Mutex<SiteView>);

impl SiteInputs for Inputs {
    fn view(&self) -> Result<SiteView, ModelError> {
        Ok(self.0.lock().unwrap().clone())
    }
}

struct Snaps {
    version: AtomicU64,
    present: AtomicBool,
}

impl RoleSnapshots for Snaps {
    fn current(&self) -> Option<Arc<Published>> {
        if !self.present.load(Ordering::SeqCst) {
            return None;
        }
        let registry = Registry::new(Vec::new(), Vec::new()).unwrap();
        Some(Arc::new(Published {
            version: self.version.load(Ordering::SeqCst),
            built_at: NOW,
            input: build_snapshot_input(&registry, &[], NOW).unwrap(),
            data: RoleData {
                registry,
                assignments: Vec::new(),
                guard: OutsideGuard::none(),
                ids: BTreeMap::new(),
            },
        }))
    }
}

struct Rig {
    source: LiveModelSource,
    directory: Arc<FixedDirectory>,
    snaps: Arc<Snaps>,
    clock: Arc<ManualClock>,
}

fn rig(journal: Option<Arc<dyn ChangeJournal>>) -> Rig {
    let directory = Arc::new(FixedDirectory::new(seeded()));
    let snaps = Arc::new(Snaps {
        version: AtomicU64::new(1),
        present: AtomicBool::new(true),
    });
    let clock = Arc::new(ManualClock::new(NOW));
    let inputs = Arc::new(Inputs(Mutex::new(SiteView {
        meta: meta(),
        token: 1,
        grants: grant_inputs(),
        duties: Vec::new(),
        rules: RuleSet::default(),
    })));
    let source = LiveModelSource::new(
        Arc::clone(&directory) as Arc<dyn DirectoryReader>,
        inputs,
        Some(Arc::clone(&snaps) as Arc<dyn RoleSnapshots>),
        journal,
        Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
        ModelSettings {
            max_age_seconds: 5,
            limits: ReadLimits::default(),
        },
    );
    Rig {
        source,
        directory,
        snaps,
        clock,
    }
}

#[tokio::test]
async fn a_role_change_shows_after_the_next_snapshot_version_and_not_before() {
    let rig = rig(None);
    let before = rows_of(&rig.source.model(None).await.unwrap(), "u-ana");
    assert!(
        before
            .iter()
            .any(|r| r.doctype == "role" && r.right == "delete")
    );

    // The role loses a grant in storage: the roles service has not published yet, so the answer is
    // the cached one (the same version, inside the age bound).
    let mut changed = seeded();
    changed.assignments.retain(|a| a.id != "a-ana");
    rig.directory.set(Ok(changed));
    let cached = rows_of(&rig.source.model(None).await.unwrap(), "u-ana");
    assert_eq!(cached, before, "the same snapshot version reuses the core");

    // A new version is published: the core is rebuilt and the answer changes.
    rig.snaps.version.store(2, Ordering::SeqCst);
    let after = rows_of(&rig.source.model(None).await.unwrap(), "u-ana");
    assert!(
        after.is_empty(),
        "the assignment is gone in the new version"
    );
}

#[tokio::test]
async fn the_age_bound_rereads_the_records_and_a_failed_read_is_never_served_stale() {
    let rig = rig(None);
    rig.source.core().await.unwrap();
    rig.directory.set(Err(ModelError::DirectoryUnavailable));
    // Inside the bound the built core is still the answer.
    assert!(rig.source.core().await.is_ok());
    rig.clock.advance(Duration::from_secs(6));
    assert_eq!(
        rig.source.core().await.err(),
        Some(ModelError::DirectoryUnavailable),
        "past the bound the records are read again, and a failure refuses instead of answering"
    );
}

#[tokio::test]
async fn a_missing_role_snapshot_refuses_every_question() {
    let rig = rig(None);
    rig.snaps.present.store(false, Ordering::SeqCst);
    assert_eq!(
        rig.source.model(None).await.err(),
        Some(ModelError::SnapshotMissing)
    );
    let status = rig.source.status().await;
    assert!(status.readiness().unwrap_err().contains("snapshot_missing"));
}

#[tokio::test]
async fn a_registry_that_cannot_be_built_refuses_instead_of_answering_in_part() {
    let rig = rig(None);
    let mut broken = seeded();
    // A cycle: Support Agent inherits Reviewer, which inherits Support Agent.
    broken.roles[1].parents = vec!["Reviewer".to_owned()];
    rig.directory.set(Ok(broken));
    assert_eq!(
        rig.source.core().await.err(),
        Some(ModelError::RegistryInvalid)
    );
}

// ---------------------------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------------------------

fn grant_event(id: &str, op: ChangeOp, at: i64) -> ChangeEvent {
    ChangeEvent::grant(
        id,
        "u-ben",
        "Reviewer",
        Source::Manual,
        op,
        ChangeActor::user("u-admin"),
        at,
    )
}

async fn seeded_journal() -> Arc<ChainJournal<spot_audit_chain::InMemoryStore>> {
    let journal = Arc::new(ChainJournal::in_memory("site-a"));
    let base = NOW.unix_timestamp() - 86_400;
    record(&*journal, &grant_event("a-ben", ChangeOp::Created, base))
        .await
        .unwrap();
    record(
        &*journal,
        &ChangeEvent::role(
            "Reviewer",
            ChangeOp::Updated,
            "custom",
            ChangeActor::user("u-admin"),
            base + 60,
        )
        .with_reason("added a user right"),
    )
    .await
    .unwrap();
    record(
        &*journal,
        &grant_event("a-ben", ChangeOp::Ended, base + 120).with_cause("removed"),
    )
    .await
    .unwrap();
    journal
}

#[tokio::test]
async fn the_history_question_lists_the_grant_the_edit_and_the_revoke_in_order() {
    let journal = seeded_journal().await;
    let rig = rig(Some(journal as Arc<dyn ChangeJournal>));
    let base = NOW.unix_timestamp() - 86_400;
    let model = rig
        .source
        .model(Some((base - 10, base + 600)))
        .await
        .unwrap();
    let auditor = MemoryAuditor::new();
    let answer = Questions::new(&model, &auditor)
        .access_changes(
            "u-admin",
            base - 10,
            base + 600,
            NOW,
            Page::new(0, 500).unwrap(),
        )
        .unwrap();
    let ops: Vec<(&str, &str)> = answer
        .items
        .iter()
        .map(|c| (c.kind.as_str(), c.op.as_str()))
        .collect();
    assert_eq!(
        ops,
        vec![
            ("grant", "created"),
            ("role", "updated"),
            ("grant", "ended")
        ]
    );
    assert!(answer.items.windows(2).all(|w| w[0].at <= w[1].at));
    assert_eq!(answer.items[0].actor, "u-admin");
    assert_eq!(answer.items[2].cause.as_deref(), Some("removed"));
    // A window with nothing in it answers nothing, not an error.
    let empty = rig
        .source
        .model(Some((base - 9_000, base - 8_000)))
        .await
        .unwrap();
    assert!(empty.history_is_empty());
}

trait HistoryEmpty {
    fn history_is_empty(&self) -> bool;
}

impl HistoryEmpty for LiveAccessModel {
    fn history_is_empty(&self) -> bool {
        spot_iam_questions::AccessModel::history(self).is_empty()
    }
}

struct Broken;

impl ChangeJournal for Broken {
    fn head<'a>(&'a self) -> BoxFuture<'a, Result<u64, JournalError>> {
        Box::pin(async { Err(JournalError::Unavailable) })
    }
    fn range<'a>(
        &'a self,
        _: u64,
        _: u64,
    ) -> BoxFuture<'a, Result<Vec<JournalEntry>, JournalError>> {
        Box::pin(async { Err(JournalError::Unavailable) })
    }
    fn append<'a>(&'a self, _: &'a ChangeEvent) -> BoxFuture<'a, Result<u64, JournalError>> {
        Box::pin(async { Err(JournalError::Unavailable) })
    }
}

#[tokio::test]
async fn an_unreadable_journal_degrades_the_history_question_only() {
    let rig = rig(Some(Arc::new(Broken) as Arc<dyn ChangeJournal>));
    // The other questions answer from the same source.
    let model = rig.source.model(None).await.unwrap();
    assert!(!rows_of(&model, "u-ana").is_empty());
    // The history question refuses with its own code.
    let refused = rig.source.model(Some((0, 10))).await.err().unwrap();
    assert_eq!(refused, ModelError::HistoryUnavailable);
    assert!(refused.history_only());
    // Readiness is degraded, with the reason code, and says it is the history.
    let status = rig.source.status().await;
    assert!(status.model.is_ok());
    assert!(!status.ready());
    let reason = status.readiness().unwrap_err();
    assert!(reason.starts_with(CODE_HISTORY_UNAVAILABLE), "{reason}");
    // A source with no journal at all says the same.
    let none = rig_without_journal_status().await;
    assert!(
        none.readiness()
            .unwrap_err()
            .starts_with(CODE_HISTORY_UNAVAILABLE)
    );
}

async fn rig_without_journal_status()
-> spot_cli::services::access_questions_model::live::ModelStatus {
    rig(None).source.status().await
}

#[tokio::test]
async fn a_healthy_source_is_ready() {
    let journal = seeded_journal().await;
    let rig = rig(Some(journal as Arc<dyn ChangeJournal>));
    let status = rig.source.status().await;
    assert!(status.ready());
    assert!(status.readiness().is_ok());
}

// ---------------------------------------------------------------------------------------------
// The PostgreSQL reader
// ---------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_postgres_reader_reads_roles_assignments_and_users_in_one_read() {
    let Some(env) = support::Env::new().await else {
        eprintln!(
            "skipped: SPOT_TEST_PG_URL is not set (no PostgreSQL to read the directory from)"
        );
        return;
    };
    env.bare_site();
    spot_cli::users::add_user(
        &env.root,
        &env.site,
        "ama@example.com",
        None,
        &["Tenant Administrator".to_owned()],
    )
    .await
    .unwrap();
    let loaded = spot_cli::config::load(&env.root, false).unwrap();
    let site = loaded.sites.iter().find(|s| s.site == env.site).unwrap();
    let meta = spot_cli::apps::serving(&env.root, &loaded, site).unwrap();
    let backend = spot_cli::schema::backend(&loaded, site).await.unwrap();
    let catalog = Arc::new(spot_cli::schema::catalog(&meta).unwrap());
    let reader =
        spot_cli::services::access_questions_model::directory::PgDirectory::new(backend, catalog);
    let directory = reader.read().await.unwrap();
    let admin = directory
        .roles
        .iter()
        .find(|r| r.name == "Tenant Administrator")
        .expect("the platform's administrator role is stored");
    assert!(
        directory.assignments.iter().any(|a| a.role_id == admin.id),
        "the invited user's assignment is read, active or not"
    );
    assert!(directory.users.iter().any(|u| u.user_type == "internal"));
}
