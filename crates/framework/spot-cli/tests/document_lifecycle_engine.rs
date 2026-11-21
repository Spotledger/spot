//! WP-AI-07.T19: the document lifecycle port (`LiveLifecycle`, WP-AI-07.T11) over the **real**
//! document engine and `GuardedEngine`, on a running `spot serve` site with PostgreSQL. The
//! fake-service tests in `document_lifecycle.rs` prove the module's own logic; this file proves the
//! rules the module claims where the engine enforces them:
//!
//! - the actor reaches the engine as the user it names, never as the platform: a user with the
//!   right cancels, a user without it is refused (a platform actor would have cancelled for both),
//!   a draft is refused, an unknown actor is refused before the engine is asked;
//! - amend, the hard floor on a submitted financial record, `relink` and `journal entry` are
//!   refused with the fixed texts, a document of another site is invisible;
//! - recompute never returns or writes a protected field for an actor the field guard hides it
//!   from, writes nothing when no step changes the field, and a repeat writes nothing again;
//! - the `documents` world-access part's rehearsal leaves the engine untouched and its real run
//!   changes it.
//!
//! The fixture is the sample app with two patches (the `e2e.rs` approach): `time_entry`, the
//! submittable DocType, gets a field classified `financial` (`hours`) and a protection-group field
//! (`note`, group `confidential`, granted to Project Managers only); `task` keeps the group
//! `Env::new` gives its `details`. Documents are made and posted through the native API as the
//! Administrator; the lifecycle then acts as other users. Every test needs PostgreSQL
//! (`SPOT_TEST_PG_URL`) and skips, printing `SKIPPED:`, when it is unset.
//!
//! What this file cannot prove, and why, is in the task report: a recompute that *writes* needs a
//! DocType with a `field_changed:<field>` step that changes its own field, and no app in the
//! repository declares one.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::net::SocketAddr;
use std::sync::Arc;

use common::*;
use serde_json::json;
use spot_cli::services::document_lifecycle::{
    BAD_STATE, DENIED, HARD_FLOOR, LiveEngine, LiveLifecycle, NO_DOCUMENT, NOT_SUPPORTED,
    PROTECTED_FIELD, UNKNOWN_ACTOR, UNKNOWN_FIELD,
};
use spot_cli::services::world_access::documents::{DocumentLifecycle, from_parts};
use spot_core::{Clock, SiteId, SystemClock};
use spot_remedies::{DocOp, DocRef, DocState as RemedyState};
use tokio::runtime::Handle;

const PM_PASSWORD: &str = "amber-orchard-ladder-3-Falcon";
const MEMBER_PASSWORD: &str = "cobalt-harbor-mango-4-Pebble";
const VIEWER_PASSWORD: &str = "saffron-meadow-pilot-7-Cedar";
const PLANTED: &str = "PLANTED-CONFIDENTIAL-4417";

/// A site: the Administrator, a Project Manager (holds the `confidential` group), a Team Member
/// (does not) and a Viewer, with the lifecycle over the site's live engine.
struct World {
    env: Env,
    started: Started,
    admin: String,
    admin_id: String,
    pm_id: String,
    member_id: String,
    viewer_id: String,
    project: String,
    lifecycle: Arc<LiveLifecycle>,
}

/// Patches the sample app: `time_entry.hours` is financial, `time_entry.note` is protected and
/// the Project Manager role holds the group (with `create`: an amendment writes the copied
/// `note` as a create, which needs the field right `create`, not only `update`).
fn patch_sample(env: &Env) {
    let entry = env.root.0.join("sample/doctypes/time_entry.toml");
    let mut text = std::fs::read_to_string(&entry).unwrap();
    for (from, to) in [
        (
            "title_field = \"entry_no\"",
            "title_field = \"entry_no\"\n\n[[protection_groups]]\nname = \"confidential\"\nlabel = \"Confidential\"\ntrace = true",
        ),
        (
            "name = \"hours\"\nlabel = \"Hours\"\ntype = \"decimal\"",
            "name = \"hours\"\nlabel = \"Hours\"\ntype = \"decimal\"\nclassification = [\"financial\"]",
        ),
        (
            "name = \"note\"\nlabel = \"Note\"\ntype = \"long_text\"",
            "name = \"note\"\nlabel = \"Note\"\ntype = \"long_text\"\nprotection_group = \"confidential\"",
        ),
    ] {
        assert!(
            text.contains(from),
            "the sample's time_entry no longer has `{from}`"
        );
        text = text.replacen(from, to, 1);
    }
    std::fs::write(&entry, text).unwrap();
    let grants = env.root.0.join("sample/permissions/field_grants.toml");
    let mut text = std::fs::read_to_string(&grants).unwrap();
    text.push_str(
        "\n[[entries]]\nrole = \"Project Manager\"\ndoctype = \"time_entry\"\ntarget = { group = \"confidential\" }\naccess = [\"read\", \"create\", \"update\", \"report\"]\n",
    );
    std::fs::write(&grants, text).unwrap();
}

/// The id of a role the platform seeds (as opposed to `common::role`, which creates one).
async fn seeded_role(addr: SocketAddr, host: &str, admin: &str, name: &str) -> String {
    let reply = http(
        addr,
        host,
        Some(admin),
        "GET",
        "/api/v1/resource/role?limit=100",
        &[],
        None,
    )
    .await;
    expect(&reply, 200);
    reply.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["role_name"] == name)
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .unwrap_or_else(|| panic!("the platform seeds no `{name}` role"))
}

async fn world() -> Option<World> {
    build(false).await.map(|(world, _)| world)
}

fn r(doctype: &str, id: &str) -> String {
    format!("{doctype}/{id}")
}

fn cancel() -> DocOp {
    DocOp::Cancel {
        reason: "wrong customer".to_owned(),
    }
}

fn recompute(field: &str) -> DocOp {
    DocOp::Recompute {
        field: field.to_owned(),
    }
}

impl World {
    async fn call(
        &self,
        token: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<serde_json::Value>,
    ) -> Reply {
        http(
            self.started.addr,
            &self.env.site,
            Some(token),
            method,
            path,
            headers,
            body,
        )
        .await
    }

    /// A task made by the Administrator, with a planted value in its protected `details`.
    async fn task(&self) -> String {
        let made = self
            .call(
                &self.admin,
                "POST",
                "/api/v1/resource/task",
                &[],
                Some(json!({
                    "subject": "Lifecycle task", "project": self.project, "details": PLANTED
                })),
            )
            .await;
        expect(&made, 201);
        made.body["data"]["id"].as_str().unwrap().to_owned()
    }

    /// A time entry made by the Administrator on `task`, posted when `post`.
    async fn entry(&self, task: &str, post: bool) -> String {
        let made = self
            .call(
                &self.admin,
                "POST",
                "/api/v1/resource/time_entry",
                &[],
                Some(json!({
                    "task": task, "person": self.admin_id, "created_by": self.admin_id,
                    "work_date": "2026-11-02", "hours": "2.50", "note": "private note"
                })),
            )
            .await;
        expect(&made, 201);
        let id = made.body["data"]["id"].as_str().unwrap().to_owned();
        if post {
            let posted = self
                .call(
                    &self.admin,
                    "POST",
                    &format!("/api/v1/resource/time_entry/{id}/post"),
                    &[("If-Match", &made.etag)],
                    None,
                )
                .await;
            expect(&posted, 200);
        }
        id
    }

    async fn rows(&self, doctype: &str) -> usize {
        let reply = self
            .call(
                &self.admin,
                "GET",
                &format!("/api/v1/resource/{doctype}?limit=200"),
                &[],
                None,
            )
            .await;
        expect(&reply, 200);
        reply.body["data"].as_array().unwrap().len()
    }

    async fn read(&self, token: &str, doctype: &str, id: &str) -> Reply {
        self.call(
            token,
            "GET",
            &format!("/api/v1/resource/{doctype}/{id}"),
            &[],
            None,
        )
        .await
    }

    async fn finish(self) {
        self.started.finish().await;
    }
}

fn wire(reply: &Reply) -> String {
    String::from_utf8_lossy(&reply.bytes).into_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_runs_as_the_named_user_and_the_engine_decides() {
    let Some(w) = world().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let lc = &w.lifecycle;
    let task = w.task().await;
    let posted = w.entry(&task, true).await;
    let draft = w.entry(&task, false).await;
    let posted_ref = r("time_entry", &posted);
    let draft_ref = r("time_entry", &draft);

    // What the platform sees: two facts, no value. `hours` is classified `financial`.
    let view = lc.view(&posted_ref).await.unwrap().unwrap();
    assert_eq!(view.state, RemedyState::Submitted);
    assert!(
        view.financial,
        "a field classified `financial` makes the DocType financial"
    );
    let view = lc.view(&draft_ref).await.unwrap().unwrap();
    assert_eq!(view.state, RemedyState::Draft);

    // A user without the cancel right is refused by the engine (a platform actor would have
    // cancelled): the actor reaches the engine as that user. The document is untouched.
    assert_eq!(
        lc.run(&w.viewer_id, &posted_ref, &cancel())
            .await
            .unwrap_err(),
        DENIED
    );
    assert_eq!(
        lc.run(&format!("user:{}", w.member_id), &posted_ref, &cancel())
            .await
            .unwrap_err(),
        DENIED,
        "the member holds no cancel right on a Time Entry they did not make"
    );
    assert_eq!(
        lc.view(&posted_ref).await.unwrap().unwrap().state,
        RemedyState::Submitted
    );

    // A draft is not cancellable, even by the user who holds the right.
    assert_eq!(
        lc.run(&format!("agent:{}", w.pm_id), &draft_ref, &cancel())
            .await
            .unwrap_err(),
        BAD_STATE
    );

    // An actor that is not a user record is refused before the engine is asked.
    assert_eq!(
        lc.run("agent:not-a-record-id", &posted_ref, &cancel())
            .await
            .unwrap_err(),
        UNKNOWN_ACTOR
    );

    // The Project Manager holds the right: the document is cancelled, as that user.
    let after = lc
        .run(&format!("agent:{}", w.pm_id), &posted_ref, &cancel())
        .await
        .unwrap();
    assert_eq!(after, RemedyState::Cancelled);
    assert_eq!(
        lc.view(&posted_ref).await.unwrap().unwrap().state,
        RemedyState::Cancelled
    );
    // A cancelled document cannot be cancelled again.
    assert_eq!(
        lc.run(&w.pm_id, &posted_ref, &cancel()).await.unwrap_err(),
        BAD_STATE
    );
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn amend_the_hard_floor_and_the_unsupported_operations_over_the_real_engine() {
    let Some(w) = world().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let lc = &w.lifecycle;
    let task = w.task().await;
    let to_amend = w.entry(&task, true).await;
    let financial = w.entry(&task, true).await;
    let draft = w.entry(&task, false).await;
    let pm = format!("agent:{}", w.pm_id);

    // Amend: the engine's correct cancels the document and makes its amended draft. The answer is
    // the original's state, `cancelled` (open question 3 of T11: the world fixture's own amend
    // leaves the document `submitted`; the two disagree, and this asserts what the engine does).
    let before = w.rows("time_entry").await;
    let state = lc
        .run(&pm, &r("time_entry", &to_amend), &DocOp::Amend)
        .await
        .unwrap();
    assert_eq!(state, RemedyState::Cancelled);
    assert_eq!(
        w.rows("time_entry").await,
        before + 1,
        "the amended draft is a new document"
    );
    // A user without the right cannot amend.
    assert_eq!(
        lc.run(&w.viewer_id, &r("time_entry", &financial), &DocOp::Amend)
            .await
            .unwrap_err(),
        DENIED
    );

    // The hard floor on a submitted financial record: only cancel and amend pass (and the
    // journal entry, which is not supported). Repost and recompute are refused by the module
    // before the engine is asked, whoever the actor is.
    let doc = r("time_entry", &financial);
    for actor in [pm.as_str(), w.admin_id.as_str()] {
        for op in [DocOp::Repost, recompute("hours"), recompute("note")] {
            assert_eq!(
                lc.run(actor, &doc, &op).await.unwrap_err(),
                HARD_FLOOR,
                "{actor}: {op:?}"
            );
        }
    }
    assert_eq!(
        lc.run(
            &pm,
            &doc,
            &DocOp::Relink {
                link: "task".to_owned()
            }
        )
        .await
        .unwrap_err(),
        HARD_FLOOR,
        "a relink is not an allowed route for a submitted financial record"
    );
    assert_eq!(
        lc.run(
            &pm,
            &doc,
            &DocOp::JournalEntry {
                entry: "JE-1".to_owned()
            }
        )
        .await
        .unwrap_err(),
        NOT_SUPPORTED,
        "a journal entry passes the floor and is not a document lifecycle call"
    );
    assert_eq!(
        lc.view(&doc).await.unwrap().unwrap().state,
        RemedyState::Submitted,
        "nothing changed"
    );

    // Relink on a draft is not supported either: the engine has no operation to re-resolve a link.
    assert_eq!(
        lc.run(
            &pm,
            &r("time_entry", &draft),
            &DocOp::Relink {
                link: "task".to_owned()
            }
        )
        .await
        .unwrap_err(),
        NOT_SUPPORTED
    );

    // A document that is not there, and a reference that is not one.
    let missing = r(
        "time_entry",
        &spot_app_api::prelude::RecordId::new_v7().to_string(),
    );
    assert!(lc.view(&missing).await.unwrap().is_none());
    assert_eq!(
        lc.run(&pm, &missing, &cancel()).await.unwrap_err(),
        NO_DOCUMENT
    );
    assert!(lc.view("nonsense").await.unwrap().is_none());
    assert_eq!(
        lc.run(&pm, "nonsense", &cancel()).await.unwrap_err(),
        NO_DOCUMENT
    );
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn recompute_and_repost_never_expose_or_write_what_the_guard_hides() {
    let Some(w) = world().await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let lc = &w.lifecycle;
    let task = w.task().await;
    let doc = r("task", &task);
    let member = format!("agent:{}", w.member_id);

    // A task has no lifecycle (it reads `draft`) and is not financial.
    let view = lc.view(&doc).await.unwrap().unwrap();
    assert_eq!(view.state, RemedyState::Draft);
    assert!(!view.financial);

    // The positive control: the Administrator reads the planted value; the Team Member does not
    // hold the `confidential` group, so the guard hides it from them.
    let admin_read = w.read(&w.admin, "task", &task).await;
    expect(&admin_read, 200);
    assert!(
        wire(&admin_read).contains(PLANTED),
        "the value is stored and readable"
    );
    let member_token = token_of(
        w.started.addr,
        &w.env.site,
        "member@example.com",
        MEMBER_PASSWORD,
    )
    .await;
    let member_read = w.read(&member_token, "task", &task).await;
    expect(&member_read, 200);
    assert!(
        !wire(&member_read).contains(PLANTED),
        "the protected value is hidden from the member"
    );
    let stored = admin_read.etag.clone();

    // A protected field is never recomputed, for any actor: not for the member the guard hides
    // it from, and not for the Project Manager who may read it.
    for actor in [
        member.clone(),
        format!("agent:{}", w.pm_id),
        w.admin_id.clone(),
    ] {
        assert_eq!(
            lc.run(&actor, &doc, &recompute("details"))
                .await
                .unwrap_err(),
            PROTECTED_FIELD,
            "{actor}"
        );
    }
    // A field the DocType does not have.
    assert_eq!(
        lc.run(&member, &doc, &recompute("no_such_field"))
            .await
            .unwrap_err(),
        UNKNOWN_FIELD
    );

    // A recompute of a visible field as the member: no `field_changed` step of the sample app
    // changes `subject`, so nothing is written, twice over. Nothing the member cannot see is
    // returned (the call answers only a state) or written: the stored version and the planted
    // value are exactly as they were.
    for _ in 0..2 {
        assert_eq!(
            lc.run(&member, &doc, &recompute("subject")).await.unwrap(),
            RemedyState::Draft
        );
        let again = w.read(&w.admin, "task", &task).await;
        assert_eq!(
            again.etag, stored,
            "a recompute that changed nothing wrote nothing"
        );
        assert!(
            wire(&again).contains(PLANTED),
            "the hidden value is untouched"
        );
    }

    // Repost of a non-financial document: an update with no change, run as the actor.
    assert_eq!(
        lc.run(&member, &doc, &DocOp::Repost).await.unwrap(),
        RemedyState::Draft
    );
    let after = w.read(&w.admin, "task", &task).await;
    assert!(
        wire(&after).contains(PLANTED),
        "a repost never loses the hidden value"
    );
    // A Viewer cannot write, so a repost as the Viewer is refused by the engine.
    assert_eq!(
        lc.run(&w.viewer_id, &doc, &DocOp::Repost)
            .await
            .unwrap_err(),
        DENIED
    );
    w.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn another_sites_document_is_invisible_and_the_documents_part_rehearses_then_changes_it() {
    let Some((w, Some(other))) = build(true).await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let lc = &w.lifecycle;
    let pm = format!("agent:{}", w.pm_id);

    // Site B's posted entry: site A's engine holds none of site B's documents.
    let foreign = r("time_entry", &other.entry);
    assert!(lc.view(&foreign).await.unwrap().is_none());
    assert_eq!(
        lc.run(&pm, &foreign, &cancel()).await.unwrap_err(),
        NO_DOCUMENT
    );
    assert!(
        other.lifecycle.view(&foreign).await.unwrap().is_some(),
        "the document is real: its own site sees it"
    );

    // The documents part over the real lifecycle: a rehearsal never changes the engine, the real
    // run (as the Project Manager) does, and a refusal is the engine's.
    let task = w.task().await;
    let posted = w.entry(&task, true).await;
    let posted_ref = r("time_entry", &posted);
    let part = from_parts(
        Some(Arc::clone(lc) as Arc<dyn DocumentLifecycle>),
        Some(Handle::current()),
    )
    .unwrap();
    let doc = DocRef::new(&posted_ref);

    let dry = {
        let part = Arc::clone(&part);
        let doc = doc.clone();
        tokio::task::spawn_blocking(move || part.rehearsal().unwrap().run(&doc, &cancel()).unwrap())
            .await
            .unwrap()
    };
    assert_eq!(
        (dry.before, dry.after),
        (RemedyState::Submitted, RemedyState::Cancelled)
    );
    assert_eq!(
        lc.view(&posted_ref).await.unwrap().unwrap().state,
        RemedyState::Submitted,
        "a rehearsal leaves the real engine untouched"
    );

    let refused = {
        let part = Arc::clone(&part);
        let doc = doc.clone();
        let viewer = format!("agent:{}", w.viewer_id);
        tokio::task::spawn_blocking(move || part.live_as(&viewer).run(&doc, &cancel()))
            .await
            .unwrap()
    };
    assert_eq!(refused.unwrap_err().detail, DENIED);

    let applied = {
        let part = Arc::clone(&part);
        let doc = doc.clone();
        let actor = pm.clone();
        tokio::task::spawn_blocking(move || part.live_as(&actor).run(&doc, &cancel()).unwrap())
            .await
            .unwrap()
    };
    assert_eq!(
        (applied.before, applied.after),
        (RemedyState::Submitted, RemedyState::Cancelled)
    );
    assert_eq!(
        lc.view(&posted_ref).await.unwrap().unwrap().state,
        RemedyState::Cancelled,
        "the real run changed the engine"
    );
    w.finish().await;
}

/// What the second site contributes: a posted entry of its own and its lifecycle.
struct OtherSite {
    entry: String,
    lifecycle: Arc<LiveLifecycle>,
}

/// The world of site A and, when `second`, a site B in the same root served by the same host
/// with one posted time entry of its own. `None` when no PostgreSQL is configured.
async fn build(second: bool) -> Option<(World, Option<OtherSite>)> {
    let env = Env::new("").await?;
    patch_sample(&env);
    let first = env.site();
    let other_name = format!("{}-b", env.site);
    // A second site: its own key file is made by `run` for `new-site`.
    let first_b = second.then(|| secret(&ok(&env.root.0, &["new-site", &other_name])));
    env.user("pm@example.com", PM_PASSWORD);
    env.user("member@example.com", MEMBER_PASSWORD);
    env.user("viewer@example.com", VIEWER_PASSWORD);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin_token = admin(addr, &host, &first).await;
    let admin_id = me(addr, &host, &admin_token).await;
    let pm_id = user_id(addr, &host, &admin_token, "pm@example.com").await;
    let member_id = user_id(addr, &host, &admin_token, "member@example.com").await;
    let viewer_id = user_id(addr, &host, &admin_token, "viewer@example.com").await;
    let user_manager = seeded_role(addr, &host, &admin_token, "User Manager").await;
    let manager = role(addr, &host, &admin_token, "Project Manager").await;
    let member = role(addr, &host, &admin_token, "Team Member").await;
    let viewer = role(addr, &host, &admin_token, "Viewer").await;
    assign(addr, &host, &admin_token, &pm_id, &manager).await;
    assign(addr, &host, &admin_token, &pm_id, &user_manager).await;
    assign(addr, &host, &admin_token, &member_id, &member).await;
    assign(addr, &host, &admin_token, &member_id, &user_manager).await;
    assign(addr, &host, &admin_token, &viewer_id, &viewer).await;
    let project = http(
        addr,
        &host,
        Some(&admin_token),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-LC-1", "title": "Lifecycle", "status": "Active",
            "owner": admin_id, "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&project, 201);
    let project = project.body["data"]["id"].as_str().unwrap().to_owned();

    let clock: Arc<dyn Clock> = Arc::new(SystemClock::new());
    let live_a = started.running.live(&env.site).expect("site A is serving");
    let lifecycle = Arc::new(LiveLifecycle::new(
        Arc::new(LiveEngine::new(live_a)),
        SiteId::new(&env.site).unwrap(),
        Arc::clone(&clock),
    ));
    let other = match first_b {
        Some(first_b) => Some(second_site(&started, &other_name, &first_b, clock).await),
        None => None,
    };
    Some((
        World {
            env,
            started,
            admin: admin_token,
            admin_id,
            pm_id,
            member_id,
            viewer_id,
            project,
            lifecycle,
        },
        other,
    ))
}

/// Site B: its Administrator, a project, a task and a posted entry, and its lifecycle.
async fn second_site(
    started: &Started,
    name: &str,
    first_secret: &str,
    clock: Arc<dyn Clock>,
) -> OtherSite {
    let addr = started.addr;
    let admin_b = admin(addr, name, first_secret).await;
    let admin_b_id = me(addr, name, &admin_b).await;
    let project_b = http(
        addr,
        name,
        Some(&admin_b),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-LC-B", "title": "Other site", "status": "Active",
            "owner": admin_b_id, "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&project_b, 201);
    let project_b = project_b.body["data"]["id"].as_str().unwrap().to_owned();
    let task_b = http(
        addr,
        name,
        Some(&admin_b),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({"subject": "Site B task", "project": project_b})),
    )
    .await;
    expect(&task_b, 201);
    let task_b = task_b.body["data"]["id"].as_str().unwrap().to_owned();
    let entry_b = http(
        addr,
        name,
        Some(&admin_b),
        "POST",
        "/api/v1/resource/time_entry",
        &[],
        Some(json!({
            "task": task_b, "person": admin_b_id, "created_by": admin_b_id,
            "work_date": "2026-11-02", "hours": "1.00"
        })),
    )
    .await;
    expect(&entry_b, 201);
    let posted_b = http(
        addr,
        name,
        Some(&admin_b),
        "POST",
        &format!(
            "/api/v1/resource/time_entry/{}/post",
            entry_b.body["data"]["id"].as_str().unwrap()
        ),
        &[("If-Match", &entry_b.etag)],
        None,
    )
    .await;
    expect(&posted_b, 200);

    let live_b = started.running.live(name).expect("site B is serving");
    OtherSite {
        entry: entry_b.body["data"]["id"].as_str().unwrap().to_owned(),
        lifecycle: Arc::new(LiveLifecycle::new(
            Arc::new(LiveEngine::new(live_b)),
            SiteId::new(name).unwrap(),
            clock,
        )),
    }
}
