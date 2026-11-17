//! Helpers of the process end-to-end cases (WP-BPM-02.T7), shared with the security regression of
//! WP-BPM-02.T8 (`process_security.rs` reuses this module; it does not copy it).
//!
//! Everything here is HTTP: the only thing that touches the database is `count_rows`, a read-only
//! connection for the "exactly once" counts.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use spot_login::LoginConfig;

use crate::client::{Client, Reply};
use crate::fixture::{ADMIN_PASSWORD, Fixture, MANAGER, MEMBER, VIEWER};
use crate::report::Failure;
use crate::{ensure, failure};

/// The role the Decision node assigns to.
pub const APPROVER_ROLE: &str = "approver";

/// The three actors of the fixture: a requester, an approver (holds `approver`) and someone unrelated.
pub struct Actors {
    pub requester: Client,
    pub approver: Client,
    pub unrelated: Client,
    pub requester_id: String,
    pub approver_id: String,
    pub unrelated_id: String,
}

/// Creates the `approver` role, gives it to the Manager and signs the three actors in:
/// the Team Member requests, the Project Manager approves, the Viewer is unrelated.
pub async fn actors(fixture: &Fixture) -> Result<Actors, Failure> {
    let role = fixture
        .admin()
        .create(
            "role",
            &json!({"role_name": APPROVER_ROLE, "origin": "reference"}),
        )
        .await?;
    role.expect(201)?;
    fixture
        .admin()
        .create(
            "role_assignment",
            &json!({
                "user": fixture.user(MANAGER).id.clone(),
                "role": role.id()?,
                "source": "manual",
                "state": "active",
                "reason": "The approver of the process cases.",
            }),
        )
        .await?
        .expect(201)?;
    Ok(Actors {
        requester: fixture.sign_in(MEMBER).await?,
        approver: fixture.sign_in(MANAGER).await?,
        unrelated: fixture.sign_in(VIEWER).await?,
        requester_id: fixture.user(MEMBER).id.clone(),
        approver_id: fixture.user(MANAGER).id.clone(),
        unrelated_id: fixture.user(VIEWER).id.clone(),
    })
}

/// The process text of the cases: a manual trigger, a Decision node for role `approver`, a Document
/// node that creates a `label` (the sample app's simplest DocType) and an end. `process` is the
/// process name; `label` the title the Decision shows.
#[must_use]
pub fn definition(process: &str, label: &str) -> String {
    format!(
        r#"name = "{process}"
app = "sample"
title = "{label}"
version = 1
status = "published"

[[nodes]]
id = "start"
kind = "trigger"
trigger = "manual"

[[nodes]]
id = "review"
kind = "decision"
title = "{label}"
assignee = "role:{APPROVER_ROLE}"
outcomes = ["approve", "reject"]

[[nodes]]
id = "record"
kind = "document"
action = "create"
doctype = "label"
mapping = "label_values"

[[nodes]]
id = "done"
kind = "logic"
logic = "branch"
terminal = true

[[edges]]
from = "start"
to = "review"

[[edges]]
from = "review"
to = "record"

[[edges]]
from = "record"
to = "done"
"#
    )
}

/// `POST /api/v1/process/{process}/versions`.
pub async fn publish(
    client: &Client,
    process: &str,
    text: &str,
    key: &str,
) -> Result<Reply, Failure> {
    client
        .send_json(
            "POST",
            &format!("/api/v1/process/{process}/versions"),
            &[("Idempotency-Key", key)],
            &json!({"definition": text, "note": "published by the l3-http process cases"}),
        )
        .await
}

/// `POST /api/v1/process/{process}/runs` with an `Idempotency-Key`.
pub async fn start(
    client: &Client,
    process: &str,
    key: &str,
    input: &Json,
) -> Result<Reply, Failure> {
    // The Document node names the case variable `label_values` as its field values (the model's
    // `mapping` is the path of an object of literals): the start input carries a unique label name.
    let mut input = input.clone();
    if let Some(map) = input.as_object_mut() {
        let name: String = format!("L {key}").chars().take(50).collect();
        map.entry("label_values").or_insert(json!({"name": name}));
    }
    client
        .send_json(
            "POST",
            &format!("/api/v1/process/{process}/runs"),
            &[("Idempotency-Key", key)],
            &json!({"input": input}),
        )
        .await
}

/// `GET /api/v1/process/runs/{id}`.
pub async fn run(client: &Client, id: &str) -> Result<Reply, Failure> {
    client.get(&format!("/api/v1/process/runs/{id}")).await
}

/// The open tasks of the caller's inbox (`view=mine`) for `process`.
pub async fn inbox(client: &Client, process: &str) -> Result<Vec<Json>, Failure> {
    let reply = client
        .get(&format!(
            "/api/v1/task?view=mine&process={process}&limit=100"
        ))
        .await?;
    reply.expect(200)?;
    reply.rows()
}

/// Waits until `client`'s inbox holds a task of `process` (the Decision node runs asynchronously on
/// the job worker) and returns it.
pub async fn wait_for_task(client: &Client, process: &str) -> Result<Json, Failure> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let tasks = inbox(client, process).await?;
        if let Some(task) = tasks.into_iter().next() {
            return Ok(task);
        }
        ensure!(
            Instant::now() < deadline,
            "no task of `{process}` reached the inbox within 30 s"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Claims `task_id` (`If-Match` from `task_get`) and completes it with `outcome`.
pub async fn decide(
    client: &Client,
    task_id: &str,
    outcome: &str,
    key: &str,
) -> Result<Reply, Failure> {
    let got = client.get(&format!("/api/v1/task/{task_id}")).await?;
    got.expect(200)?;
    let claim = client
        .send_json(
            "POST",
            &format!("/api/v1/task/{task_id}/claim"),
            &[("If-Match", &got.etag()?)],
            &json!({}),
        )
        .await?;
    claim.expect(200)?;
    client
        .send_json(
            "POST",
            &format!("/api/v1/task/{task_id}/complete"),
            &[("If-Match", &claim.etag()?), ("Idempotency-Key", key)],
            &json!({"outcome": outcome, "comment": "decided by the l3-http process cases"}),
        )
        .await
}

/// Polls `run_get` until the run is `state` (60 s budget: a run advances one step per resume scan,
/// which the fixture's site runs every 5 s).
pub async fn wait_for_state(client: &Client, id: &str, state: &str) -> Result<Json, Failure> {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let reply = run(client, id).await?;
        reply.expect(200)?;
        let data = reply.data();
        if data["state"] == json!(state) {
            return Ok(data);
        }
        if Instant::now() >= deadline {
            let events = client
                .get(&format!("/api/v1/process/runs/{id}/events"))
                .await
                .map(|r| r.text())
                .unwrap_or_default();
            ensure!(
                false,
                "the run never became `{state}`; last answer: {}; events: {events}",
                reply.text()
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Polls `/api/health/ready` until the site answers.
pub async fn wait_ready(client: &Client) -> Result<(), Failure> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut last;
    loop {
        match client.health("ready").await {
            Ok(reply) if reply.status == 200 => return Ok(()),
            Ok(reply) => last = format!("HTTP {}: {}", reply.status, reply.text()),
            Err(e) => last = format!("{e}"),
        }
        ensure!(
            Instant::now() < deadline,
            "the host never became ready (last answer: {})",
            &last[..last.len().min(700)]
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A host started on the fixture's root: the one the fixture owns is stopped first. Used by the
/// cases that restart the server or serve two sites (`tests/e2e.rs` does the same).
pub struct Host {
    running: Option<spot_cli::Running>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    pub addr: SocketAddr,
}

impl Host {
    /// Stops the fixture's own host and starts a new one on the same root.
    pub async fn restart(fixture: &Fixture) -> Result<Host, Failure> {
        fixture.begin_shutdown();
        fixture.join_host().await;
        Host::start(fixture).await
    }

    /// Starts a host on the fixture's root (the fixture's own must not be running).
    pub async fn start(fixture: &Fixture) -> Result<Host, Failure> {
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let running = spot_cli::start_with(
            fixture.root(),
            false,
            spot_cli::Options {
                clock: None,
                login: LoginConfig::for_tests(),
            },
            async {
                let _ = stopped.await;
            },
        )
        .await
        .map_err(|e| failure!("the host did not start: {e}"))?;
        let addr = *running
            .local_addrs()
            .first()
            .ok_or_else(|| failure!("the host bound no address"))?;
        Ok(Host {
            running: Some(running),
            stop: Some(stop),
            addr,
        })
    }

    /// A client for `site` on this host.
    #[must_use]
    pub fn client(&self, site: &str) -> Client {
        Client::new(self.addr, site.to_owned())
    }

    /// Stops the host and waits for it.
    pub async fn stop(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(running) = self.running.take() {
            let _ = tokio::time::timeout(Duration::from_secs(15), running.join()).await;
        }
    }
}

/// Signs `user` in on a client of a restarted host (the fixture's own sessions died with its host
/// only if the session store is memory-bound; signing in again is always correct).
pub async fn sign_in_again(
    host: &Host,
    site: &str,
    fixture: &Fixture,
    role: &str,
) -> Result<Client, Failure> {
    let user = fixture.user(role);
    host.client(site)
        .sign_in(&user.email, user.password.as_deref().unwrap_or_default())
        .await
}

/// The Administrator on a restarted host.
pub async fn admin_again(host: &Host, site: &str) -> Result<Client, Failure> {
    host.client(site)
        .sign_in("administrator", ADMIN_PASSWORD)
        .await
}

/// A read-only count over the site's database (the only direct database read of these cases).
///
/// `sql` must be a `SELECT count(*)` with no parameters.
pub async fn count_rows(fixture: &Fixture, sql: &str) -> Result<i64, Failure> {
    let loaded = spot_cli::config::load(fixture.root(), false)
        .map_err(|e| failure!("cannot load the configuration: {e}"))?;
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == fixture.site())
        .ok_or_else(|| failure!("the site is not in the configuration"))?;
    let backend =
        spot_postgres::PgBackend::connect(loaded.admin.clone().with_dbname(&site.database))
            .await
            .map_err(|e| failure!("cannot connect to the site database: {e}"))?;
    let client = backend
        .pool()
        .get()
        .await
        .map_err(|e| failure!("no connection: {e}"))?;
    client
        .batch_execute("SET default_transaction_read_only = on")
        .await
        .map_err(|e| failure!("cannot make the connection read-only: {e}"))?;
    let row = client
        .query_one(sql, &[])
        .await
        .map_err(|e| failure!("the count failed: {e}"))?;
    Ok(row.get(0))
}
