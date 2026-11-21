//! WP-AI-11.T9: the engine-backed `ReadAccess` of the AI audit timeline, over a running host and
//! the test PostgreSQL (the host tests skip, with no output, when `SPOT_TEST_PG_URL` is unset).
//!
//! The host is the sample site with the platform's roles: a Viewer reads Tasks but not the
//! confidential `details` group, the Administrator reads everything, and a stranger holds no
//! role. The decision is asked straight of [`EngineReadAccess`] over the live hub and then through
//! the hosted AI audit port, whose timeline shows what the access allows: the entries as recorded,
//! their argument values hidden, or nothing.
//!
//! The refusal rule needs no database: a judge that cannot decide makes the timeline refuse.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use std::sync::Arc;
use std::time::Duration;

use common::*;
use serde_json::json;
use spot_app_api::prelude::RecordId;
use spot_audit_chain::InMemoryStore;
use spot_cli::services::ai_audit::read_access::{EngineReadAccess, Judge, LiveJudge, Viewer, why};
use spot_cli::services::ai_audit::{
    Access, ActionKind, AiAction, AiAudit, Fut, Outcome, ReadAccess, SecretNameMask, Settings,
    TIMELINE_REFUSED, hosted,
};
use spot_core::SiteId;

const VIEWER: &str = "viewer@example.com";
const VIEWER_PASSWORD: &str = "saffron-meadow-pilot-7-Cedar";
const STRANGER: &str = "stranger@example.com";
const STRANGER_PASSWORD: &str = "amber-thicket-vessel-6-Quill";
const PLANTED: &str = "PLANTED-CONFIDENTIAL-9917";

struct World {
    env: Env,
    started: Started,
    admin_id: String,
    viewer_id: String,
    stranger_id: String,
    task: String,
    access: EngineReadAccess<LiveJudge>,
}

/// A host with the AI audit service on, a Viewer, a Tenant Administrator, a stranger with no role
/// and one Task whose protected `details` the Viewer may not read.
async fn world() -> Option<World> {
    let env = Env::new("[services.ai_audit]\nenabled = true\n").await?;
    let first = env.site();
    env.user(VIEWER, VIEWER_PASSWORD);
    env.user(STRANGER, STRANGER_PASSWORD);
    let started = start(&env).await;
    let (addr, host) = (started.addr, env.site.clone());
    let admin = admin(addr, &host, &first).await;
    let admin_id = me(addr, &host, &admin).await;
    let viewer_id = user_id(addr, &host, &admin, VIEWER).await;
    let stranger_id = user_id(addr, &host, &admin, STRANGER).await;
    let viewer_role = role(addr, &host, &admin, "Viewer").await;
    assign(addr, &host, &admin, &viewer_id, &viewer_role).await;

    let project = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/project",
        &[],
        Some(json!({
            "code": "PRJ-AUD-1", "title": "Audit timeline", "status": "Active",
            "owner": viewer_id, "currency": "USD", "budget": "10.00"
        })),
    )
    .await;
    expect(&project, 201);
    let project = project.body["data"]["id"].as_str().unwrap().to_owned();
    let made = http(
        addr,
        &host,
        Some(&admin),
        "POST",
        "/api/v1/resource/task",
        &[],
        Some(json!({"subject": "Timeline task", "project": project, "details": PLANTED})),
    )
    .await;
    expect(&made, 201);
    let task = made.body["data"]["id"].as_str().unwrap().to_owned();

    let live = started
        .running
        .live(&env.site)
        .expect("the running host serves the site");
    let access = EngineReadAccess::new(LiveJudge::new(
        live,
        SiteId::new(&env.site).unwrap(),
        spot_cli::login::system_clock(),
    ));
    Some(World {
        env,
        started,
        admin_id,
        viewer_id,
        stranger_id,
        task,
        access,
    })
}

impl World {
    async fn decide(&self, viewer: &str, doctype: &str, id: &str) -> Result<Access, String> {
        self.access.access(viewer, doctype, id).await
    }
}

fn action(event: &str, task: &str) -> AiAction {
    AiAction {
        kind: ActionKind::ToolCall,
        event_id: event.to_owned(),
        agent: "bot1".to_owned(),
        acting_user: "someone".to_owned(),
        tool: "update_task".to_owned(),
        arguments: vec![
            ("title".to_owned(), "plan".to_owned()),
            ("api_token".to_owned(), "tok-aud-secret".to_owned()),
        ],
        outcome: Outcome::Ok,
        subject: Some(("task".to_owned(), task.to_owned())),
        refers_to: None,
        at_micros: 1_700_000_000_000_000,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_viewer_gets_the_access_the_engine_and_the_guard_give_them() {
    let Some(w) = world().await else { return };

    // The Administrator may read the document and every protected field: full.
    assert_eq!(
        w.decide(&w.admin_id, "task", &w.task).await,
        Ok(Access::Full)
    );
    // The `user:` form names the same person.
    assert_eq!(
        w.decide(&format!("user:{}", w.admin_id), "task", &w.task)
            .await,
        Ok(Access::Full)
    );
    // The Viewer reads the Task but the guard does not show `details`: masked.
    assert_eq!(
        w.decide(&w.viewer_id, "task", &w.task).await,
        Ok(Access::Masked)
    );
    // The stranger holds no role: none, the same as a document that does not exist.
    assert_eq!(
        w.decide(&w.stranger_id, "task", &w.task).await,
        Ok(Access::None)
    );
    let missing = RecordId::new_v7().to_string();
    assert_eq!(
        w.decide(&w.admin_id, "task", &missing).await,
        Ok(Access::None),
        "an id that is not in this site (another tenant's included) is none"
    );
    assert_eq!(
        w.decide(&w.admin_id, "task", "not-a-record-id").await,
        Ok(Access::None)
    );
    assert_eq!(
        w.decide(&w.admin_id, "no_such_doctype", &w.task).await,
        Ok(Access::None)
    );
    // A viewer that is no user holds no role.
    assert_eq!(w.decide("l3-user", "task", &w.task).await, Ok(Access::None));
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_is_judged_by_its_own_roles() {
    let Some(w) = world().await else { return };
    // The agent whose own user holds the Viewer role is masked, whoever it acts for.
    assert_eq!(
        w.decide(&format!("agent:{}", w.viewer_id), "task", &w.task)
            .await,
        Ok(Access::Masked)
    );
    // The agent with no role reads nothing, even though the Administrator it might act for reads
    // everything.
    assert_eq!(
        w.decide(&format!("agent:{}", w.stranger_id), "task", &w.task)
            .await,
        Ok(Access::None)
    );
    assert_eq!(
        w.decide(&format!("agent:{}", w.admin_id), "task", &w.task)
            .await,
        Ok(Access::Full)
    );
    w.started.finish().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_timeline_shows_what_the_access_allows() {
    let Some(w) = world().await else { return };
    let site = w.env.site.clone();
    let mut port = None;
    for _ in 0..100 {
        port = hosted(&site);
        if port.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let port = port.expect("the AI audit service is hosted");
    port.append(action("aud-event-1", &w.task)).await.unwrap();

    // Full: the entry as recorded (a secret-named argument is hidden by the mask at write time).
    let full = port
        .timeline(&w.admin_id, "task", &w.task, 10)
        .await
        .unwrap();
    assert_eq!(full.len(), 1);
    assert!(
        full[0]
            .arguments
            .contains(&("title".to_owned(), "plan".to_owned()))
    );
    assert!(
        full[0]
            .arguments
            .contains(&("api_token".to_owned(), "***".to_owned()))
    );

    // Masked: the same entry with every argument value hidden.
    let masked = port
        .timeline(&w.viewer_id, "task", &w.task, 10)
        .await
        .unwrap();
    assert_eq!(masked.len(), 1);
    assert_eq!(masked[0].tool, "update_task");
    assert!(masked[0].arguments.iter().all(|(_, value)| value == "***"));
    let shown = format!("{masked:?}");
    assert!(!shown.contains("plan") && !shown.contains("tok-aud-secret"));

    // None: a viewer with no right, an unknown document and a malformed id see no entry.
    for (viewer, id) in [
        (w.stranger_id.clone(), w.task.clone()),
        (w.admin_id.clone(), RecordId::new_v7().to_string()),
        (w.admin_id.clone(), "not-a-record-id".to_owned()),
    ] {
        let none = port.timeline(&viewer, "task", &id, 10).await.unwrap();
        assert!(none.is_empty(), "{viewer} {id}");
    }
    w.started.finish().await;
}

/// A judge that cannot decide: the engine is down.
struct Down;

impl Judge for Down {
    fn judge<'a>(
        &'a self,
        _viewer: Viewer,
        _doctype: &'a str,
        _id: RecordId,
    ) -> Fut<'a, Result<Access, &'static str>> {
        Box::pin(async { Err(why::ENGINE_FAILED) })
    }
}

#[tokio::test]
async fn an_access_that_cannot_decide_makes_the_timeline_refuse_never_show() {
    let access: Arc<dyn ReadAccess> = Arc::new(EngineReadAccess::new(Down));
    let audit = AiAudit::new(
        InMemoryStore::new(),
        "acme",
        Arc::new(SecretNameMask),
        access,
        Settings::default(),
    );
    let task = RecordId::new_v7().to_string();
    audit.append(action("down-1", &task)).await.unwrap();
    let viewer = RecordId::new_v7().to_string();
    let refused = audit
        .timeline(&viewer, "task", &task, 10)
        .await
        .unwrap_err();
    assert_eq!(refused.code, TIMELINE_REFUSED);
    assert_eq!(refused.code, "ai_audit_host.timeline_refused");
}
