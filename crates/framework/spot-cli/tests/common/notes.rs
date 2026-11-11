//! A tiny test app, `notes`: one DocType with a native controller that enqueues a job after a
//! note is created (and fails for a note titled `boom`, after enqueueing, so the rollback is
//! the proof the job followed the transaction), and two whitelisted methods that require named
//! permissions: `approve` (declared) and `bless` (declared by nobody).
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]
use super::Env;
use serde_json::json;
use spot_app_api::prelude::Name;
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, ErrorClass, HookContext, HookPoint, JobRequest,
    MethodContext, MethodDecl, MethodRight, Operation, Phase, StepDecl, StepName, Target,
};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The titles of the notes whose `notes:ping` job ran, in run order.
pub static PINGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

pub fn pings() -> Vec<String> {
    PINGS.lock().unwrap().clone()
}

/// Creates a note as `token`.
pub async fn make(addr: super::SocketAddr, host: &str, token: &str, title: &str) -> super::Reply {
    super::http(
        addr,
        host,
        Some(token),
        "POST",
        "/api/v1/resource/note",
        &[],
        Some(json!({ "title": title })),
    )
    .await
}

fn step(name: &str, priority: i32) -> StepDecl {
    StepDecl {
        name: StepName::new(name).unwrap(),
        point: HookPoint::of(Phase::After, Target::Op(Operation::Create)),
        priority,
        budget: Duration::from_millis(500),
    }
}

pub struct Notes;

impl Controller for Notes {
    fn doctype(&self) -> &str {
        "note"
    }

    fn steps(&self) -> Vec<StepDecl> {
        vec![
            step("notes:enqueue_ping", 10),
            step("notes:notify", 15),
            step("notes:fail_on_boom", 20),
        ]
    }

    fn methods(&self) -> Vec<MethodDecl> {
        let permission = |name: &str, method: &str| MethodDecl {
            name: Name::new(method).unwrap(),
            mutates: false,
            right: MethodRight::Permission(Name::new(name).unwrap()),
        };
        vec![
            permission("approve_note", "approve"),
            permission("no_such_permission", "bless"),
        ]
    }

    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let title = ctx.doc().text("title")?.unwrap_or_default().to_owned();
            match step.as_str() {
                "notes:enqueue_ping" => ctx.enqueue_job(JobRequest {
                    job: "notes:ping".to_owned(),
                    args: json!({ "title": title }),
                    idempotency_key: None,
                    not_before: None,
                }),
                // `notify:<user id>` (and `notify:<user id>:boom`, which fails below, after the
                // notice, so a rollback is the proof the notice followed its transaction).
                "notes:notify" => match title.strip_prefix("notify:") {
                    Some(rest) => {
                        let id = rest.split(':').next().unwrap_or_default();
                        match spot_http::api::codec::parse_record_id(id) {
                            Some(user) => ctx.notify_user(user, json!({ "kind": "note_created" })),
                            None => Ok(()),
                        }
                    }
                    None => Ok(()),
                },
                _ if title == "boom" || title.ends_with(":boom") => Err(AppError::new(
                    ErrorClass::Validation,
                    concat!("notes", ".boom"),
                    "This note is refused after its job was enqueued.",
                )),
                _ => Ok(()),
            }
        })
    }

    fn call_method<'a>(
        &'a self,
        method: &'a str,
        _args: serde_json::Value,
        _ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<serde_json::Value>> {
        Box::pin(async move { Ok(json!({ "ran": method })) })
    }
}

/// Registers the controller and the job handler with the host (once per process).
pub fn register() {
    spot_cli::natives::register("notes", Arc::new(Notes));
    spot_cli::natives::register_job("notes:ping", |ctx| {
        Box::pin(async move {
            let title = ctx.job.args["title"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            PINGS.lock().unwrap().push(title);
            Ok(spot_jobs::worker::HandlerResult::Ok)
        })
    });
}

/// Writes the app into the env's root, lists it in `common.toml` after the sample, and installs
/// it on `sites` in this process (the `spot` binary does not contain the test controller, so
/// it could not install the app itself). Create every site and user with the binary first.
pub async fn install(env: &Env, sites: &[&str]) {
    register();
    let dir = env.root.0.join("notes");
    std::fs::create_dir_all(dir.join("doctypes")).unwrap();
    std::fs::create_dir_all(dir.join("permissions")).unwrap();
    std::fs::write(
        dir.join("app.toml"),
        r#"[app]
name = "notes"
version = "1.0.0"
title = "Notes"
controller = "native:notes"

[[events]]
doctype = "note"
event = "after_create"
step = "notes:enqueue_ping"

[[events]]
doctype = "note"
event = "after_create"
step = "notes:notify"

[[events]]
doctype = "note"
event = "after_create"
step = "notes:fail_on_boom"

[[permissions]]
name = "approve_note"
title = "Approve notes"
description = "Approve a note."
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("doctypes/note.toml"),
        r#"name = "note"
label = "Note"
app = "notes"
module = "notes"
title_field = "title"

[[fields]]
name = "title"
label = "Title"
type = "text"
max_chars = 100
required = true
in_list_view = true
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("permissions/grants.toml"),
        r#"[[entries]]
role = "Note Reader"
doctype = "note"
rights = ["read", "report", "select"]

[[entries]]
role = "Note Approver"
doctype = "note"
rights = ["read", "report", "select"]
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("permissions/app_permissions.toml"),
        r#"[[entries]]
role = "Note Approver"
permission = "approve_note"
"#,
    )
    .unwrap();
    env.write_common(&["'sample'", "'notes'"], &env.services);
    for site in sites {
        spot_cli::ops::install_app(
            &env.root.0,
            "notes",
            Some(site),
            spot_cli::ops::Install {
                apply: true,
                seed: false,
            },
        )
        .await
        .unwrap();
    }
}
