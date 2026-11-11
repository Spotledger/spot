//! Simulated users over the native API.
//!
//! Every [`Session`] is one user with a connection of its own, a bearer token from its own
//! sign-in and the few ids it needs (tasks to open, a draft to update). Passwords are held in
//! memory only, never printed, and never reach a report.

use std::time::Duration;

use serde_json::{Value as Json, json};
use tokio::sync::Mutex;

use crate::http::{Conn, HttpError, Response, Target};
use crate::metrics::{Class, Outcome, classify_status};
use crate::profile::Op;
use crate::rng::Rng;
use crate::runner::{BoxFuture, Executor};

/// A secret string that cannot be printed by accident.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    /// Wraps a secret.
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    /// The secret itself, for the one request that sends it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// One simulated user's credentials.
#[derive(Debug, Clone)]
pub struct SimUser {
    /// The sign-in name.
    pub email: String,
    /// The password.
    pub password: Secret,
}

/// The DocType the posting operations use.
pub const POSTING: &str = "time_entry";
/// The DocType the list and form operations read.
pub const READING: &str = "task";

struct Draft {
    id: String,
    version: i64,
}

/// One simulated user.
pub struct Session {
    user: SimUser,
    run: String,
    index: u32,
    token: Option<Secret>,
    person: String,
    conn: Conn,
    drafts: Vec<Draft>,
    tasks: Vec<String>,
    rng: Rng,
    requests: u64,
    timeout: Duration,
    report: String,
}

impl Session {
    /// A session for `user`; nothing is sent until [`Session::prepare`].
    pub fn new(
        user: SimUser,
        run: &str,
        index: u32,
        seed: u64,
        timeout: Duration,
        report: &str,
    ) -> Self {
        Self {
            user,
            run: run.to_owned(),
            index,
            token: None,
            person: String::new(),
            conn: Conn::new(),
            drafts: Vec::new(),
            tasks: Vec::new(),
            rng: Rng::new(seed ^ (u64::from(index) + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15)),
            requests: 0,
            timeout,
            report: report.to_owned(),
        }
    }

    /// Signs in, learns the user's id and the tasks it can open, and makes one draft to update.
    /// Not measured: it is the setup a user has done before the load starts.
    pub async fn prepare(&mut self, target: &Target) -> Result<(), String> {
        let outcome = self.sign_in(target).await;
        if outcome.class != Class::Ok {
            return Err(format!(
                "session {}: the sign-in was refused or failed ({:?}, HTTP {})",
                self.index, outcome.class, outcome.status
            ));
        }
        let me = self
            .call(target, "GET", "/api/v1/auth/session", &[], None)
            .await;
        self.person = me
            .as_ref()
            .ok()
            .map(Response::json)
            .and_then(|j| {
                j.pointer("/data/user/id")
                    .and_then(Json::as_str)
                    .map(str::to_owned)
            })
            .ok_or_else(|| {
                format!(
                    "session {}: the session answer names no user id",
                    self.index
                )
            })?;
        self.refresh_tasks(target).await;
        if self.tasks.is_empty() {
            self.make_task(target).await;
        }
        if self.tasks.is_empty() {
            return Err(format!(
                "session {}: the user can read no task and cannot create one",
                self.index
            ));
        }
        let draft = self.create_draft(target).await;
        if draft.class != Class::Ok {
            return Err(format!(
                "session {}: cannot create a {POSTING} draft ({:?}, HTTP {})",
                self.index, draft.class, draft.status
            ));
        }
        Ok(())
    }

    /// Performs one operation and says how it ended.
    pub async fn perform(&mut self, target: &Target, op: Op) -> Outcome {
        if self.token.is_none() && op != Op::SignIn {
            let outcome = self.sign_in(target).await;
            if outcome.class != Class::Ok {
                return outcome;
            }
        }
        let outcome = match op {
            Op::SignIn => self.sign_in(target).await,
            Op::OpenForm => self.open_form(target).await,
            Op::List => self.list(target).await,
            Op::CreateSubmit => self.create_submit(target).await,
            Op::Update => self.update(target, false).await,
            Op::StaleUpdate => self.update(target, true).await,
            Op::ReportRun => self.report_run(target).await,
        };
        if outcome.status == 401 {
            // The token is gone (expired or evicted): the next operation signs in again.
            self.token = None;
        }
        outcome
    }

    async fn call(
        &mut self,
        target: &Target,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&Json>,
    ) -> Result<Response, Outcome> {
        self.requests += 1;
        let request_id = format!("l9-{}-{}-{}", self.run, self.index, self.requests);
        let bearer = self
            .token
            .as_ref()
            .map(|t| format!("Bearer {}", t.expose()));
        let mut all: Vec<(&str, &str)> = vec![("X-Request-Id", &request_id)];
        if let Some(bearer) = &bearer {
            all.push(("Authorization", bearer));
        }
        all.extend_from_slice(headers);
        let bytes = body.map(|b| b.to_string().into_bytes());
        match self
            .conn
            .request(target, method, path, &all, bytes.as_deref(), self.timeout)
            .await
        {
            Ok(response) => Ok(response),
            Err(HttpError::Timeout) => Err(Outcome::of(Class::Timeout)),
            Err(HttpError::Transport) => Err(Outcome::of(Class::Transport)),
        }
    }

    async fn sign_in(&mut self, target: &Target) -> Outcome {
        self.token = None;
        let body = json!({ "user": self.user.email, "password": self.user.password.expose() });
        let response = match self
            .call(target, "POST", "/api/v1/auth/login", &[], Some(&body))
            .await
        {
            Ok(r) => r,
            Err(outcome) => return outcome,
        };
        let outcome = classify_status(response.status, false);
        if outcome.class == Class::Ok {
            match response
                .json()
                .pointer("/data/token")
                .and_then(Json::as_str)
            {
                Some(token) => self.token = Some(Secret::new(token)),
                None => return Outcome::of(Class::ClientError),
            }
        }
        outcome
    }

    async fn refresh_tasks(&mut self, target: &Target) {
        let body = json!({ "limit": 50 });
        let path = format!("/api/v1/resource/{READING}/query");
        if let Ok(response) = self.call(target, "POST", &path, &[], Some(&body)).await
            && response.status == 200
        {
            self.tasks = response
                .json()
                .get("data")
                .and_then(Json::as_array)
                .map(|rows| {
                    rows.iter()
                        .filter_map(|r| r.get("id").and_then(Json::as_str).map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
        }
    }

    /// Makes a project and a task when the site has none the user can open.
    async fn make_task(&mut self, target: &Target) {
        let code = format!("L9{}{}", self.index, self.rng.below(1_000_000));
        let project = json!({
            "code": code, "title": "L9 load project", "status": "Active",
            "owner": self.person, "currency": "USD", "budget": "1000.00"
        });
        let Ok(response) = self
            .call(
                target,
                "POST",
                "/api/v1/resource/project",
                &[],
                Some(&project),
            )
            .await
        else {
            return;
        };
        let Some(project_id) = response
            .json()
            .pointer("/data/id")
            .and_then(Json::as_str)
            .map(str::to_owned)
        else {
            return;
        };
        let task = json!({ "subject": "L9 load task", "project": project_id, "status": "Open" });
        let path = format!("/api/v1/resource/{READING}");
        if let Ok(response) = self.call(target, "POST", &path, &[], Some(&task)).await
            && let Some(id) = response.json().pointer("/data/id").and_then(Json::as_str)
        {
            self.tasks.push(id.to_owned());
        }
    }

    fn pick_task(&mut self) -> String {
        let n = self.tasks.len() as u64;
        let index = usize::try_from(self.rng.below(n)).unwrap_or(0);
        self.tasks.get(index).cloned().unwrap_or_default()
    }

    async fn open_form(&mut self, target: &Target) -> Outcome {
        let meta = match self
            .call(target, "GET", &format!("/api/v1/meta/{READING}"), &[], None)
            .await
        {
            Ok(r) => classify_status(r.status, false),
            Err(outcome) => return outcome,
        };
        if meta.class != Class::Ok || self.tasks.is_empty() {
            return meta;
        }
        let id = self.pick_task();
        match self
            .call(
                target,
                "GET",
                &format!("/api/v1/resource/{READING}/{id}"),
                &[],
                None,
            )
            .await
        {
            Ok(r) => classify_status(r.status, false),
            Err(outcome) => outcome,
        }
    }

    async fn list(&mut self, target: &Target) -> Outcome {
        let body = json!({
            "filter": { "field": "status", "op": "eq", "value": "Open" },
            "limit": 25
        });
        match self
            .call(
                target,
                "POST",
                &format!("/api/v1/resource/{READING}/query"),
                &[],
                Some(&body),
            )
            .await
        {
            Ok(r) => classify_status(r.status, false),
            Err(outcome) => outcome,
        }
    }

    async fn create_draft(&mut self, target: &Target) -> Outcome {
        let task = self.pick_task();
        let hours = format!("{}.00", 1 + self.rng.below(8));
        let day = 1 + self.rng.below(28);
        let body = json!({
            "task": task, "person": self.person,
            "work_date": format!("2026-10-{day:02}"), "hours": hours
        });
        let response = match self
            .call(
                target,
                "POST",
                &format!("/api/v1/resource/{POSTING}"),
                &[],
                Some(&body),
            )
            .await
        {
            Ok(r) => r,
            Err(outcome) => return outcome,
        };
        let outcome = classify_status(response.status, false);
        if outcome.class == Class::Ok {
            let json = response.json();
            match (
                json.pointer("/data/id").and_then(Json::as_str),
                json.pointer("/data/version").and_then(Json::as_i64),
            ) {
                (Some(id), Some(version)) => self.drafts.push(Draft {
                    id: id.to_owned(),
                    version,
                }),
                _ => return Outcome::of(Class::ClientError),
            }
        }
        outcome
    }

    async fn create_submit(&mut self, target: &Target) -> Outcome {
        let created = self.create_draft(target).await;
        if created.class != Class::Ok {
            return created;
        }
        let Some(draft) = self.drafts.pop() else {
            return Outcome::of(Class::ClientError);
        };
        let etag = format!("\"{}\"", draft.version);
        let path = format!("/api/v1/resource/{POSTING}/{}/post", draft.id);
        match self
            .call(target, "POST", &path, &[("If-Match", &etag)], None)
            .await
        {
            Ok(r) => classify_status(r.status, false),
            Err(outcome) => outcome,
        }
    }

    async fn update(&mut self, target: &Target, stale: bool) -> Outcome {
        if self.drafts.is_empty() {
            let created = self.create_draft(target).await;
            if created.class != Class::Ok {
                return created;
            }
        }
        let Some(draft) = self.drafts.last() else {
            return Outcome::of(Class::ClientError);
        };
        let (id, held) = (draft.id.clone(), draft.version);
        let etag = format!("\"{}\"", if stale { 0 } else { held });
        let body = json!({ "hours": format!("{}.00", 1 + self.rng.below(8)) });
        let path = format!("/api/v1/resource/{POSTING}/{id}");
        let response = match self
            .call(target, "PATCH", &path, &[("If-Match", &etag)], Some(&body))
            .await
        {
            Ok(r) => r,
            Err(outcome) => return outcome,
        };
        let outcome = classify_status(response.status, stale);
        if outcome.class == Class::Ok
            && let Some(version) = response
                .json()
                .pointer("/data/version")
                .and_then(Json::as_i64)
            && let Some(draft) = self.drafts.last_mut()
        {
            draft.version = version;
        }
        outcome
    }

    async fn report_run(&mut self, target: &Target) -> Outcome {
        let path = format!("/api/v1/report/{}/run", self.report);
        let body = json!({ "limit": 50 });
        match self.call(target, "POST", &path, &[], Some(&body)).await {
            // The engine does not serve reports yet and answers 404 `report.not_found`;
            // that refusal is the expected answer until it does, and a 200 is better.
            Ok(r) => classify_status(r.status, r.status == 404),
            Err(outcome) => outcome,
        }
    }
}

/// The executor that runs operations through the sessions of one run.
pub struct ApiExecutor {
    target: Target,
    sessions: Vec<Mutex<Session>>,
}

impl ApiExecutor {
    /// Builds one session per user and prepares them all (a few at a time).
    pub async fn start(
        target: Target,
        users: Vec<SimUser>,
        run: &str,
        seed: u64,
        timeout: Duration,
        report: &str,
    ) -> Result<ApiExecutor, String> {
        let mut sessions: Vec<Session> = users
            .into_iter()
            .enumerate()
            .map(|(i, user)| {
                Session::new(
                    user,
                    run,
                    u32::try_from(i).unwrap_or(u32::MAX),
                    seed,
                    timeout,
                    report,
                )
            })
            .collect();
        let mut prepared = Vec::with_capacity(sessions.len());
        while !sessions.is_empty() {
            let batch: Vec<Session> = sessions.drain(..sessions.len().min(16)).collect();
            let mut tasks = Vec::new();
            for mut session in batch {
                let target = target.clone();
                tasks.push(tokio::spawn(async move {
                    let result = session.prepare(&target).await;
                    (session, result)
                }));
            }
            for task in tasks {
                let (session, result) =
                    task.await.map_err(|_| "a setup task panicked".to_owned())?;
                result?;
                prepared.push(session);
            }
        }
        Ok(ApiExecutor {
            target,
            sessions: prepared.into_iter().map(Mutex::new).collect(),
        })
    }
}

impl Executor for ApiExecutor {
    fn execute(&self, session: u32, op: Op) -> BoxFuture<'_, Outcome> {
        Box::pin(async move {
            let Some(slot) = self.sessions.get(session as usize) else {
                return Outcome::of(Class::Transport);
            };
            slot.lock().await.perform(&self.target, op).await
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn a_secret_never_prints() {
        let user = SimUser {
            email: "a@b.test".into(),
            password: Secret::new("hunter2-very-secret"),
        };
        let shown = format!("{user:?}");
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("redacted"));
        assert_eq!(user.password.expose(), "hunter2-very-secret");
    }
}
