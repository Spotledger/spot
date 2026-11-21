//! The task inbox hosted for one site (WP-BPM-05.T8; ADR-49, ADR-45, native API 0.11.0).
//!
//! `spot-bpm-tasks` is the Decision node and the inbox as pure logic over injected ports
//! (`TaskStore`, `Directory`, `InboxAccess`, `Segregation`, `Notifier`, `TaskEngine`); WP-BPM-05.T6
//! is its PostgreSQL store, WP-BPM-05.T7 serves the seven inbox operations in `spot-http`. This
//! module is the host that joins them for one site:
//!
//! - **Durable tasks**: `PgTaskStore` over a bridge this module owns (`spot-pg-bridge`). The
//!   store refuses to run on a thread that belongs to a tokio runtime, so everything that touches
//!   the task service runs on a plain operating-system thread (see "Threads" below). No SQL is
//!   written here (ADR-45): the task tables are the store crate's; the identity reads below go
//!   through the storage contract's typed query, with bound values.
//! - **The Decision node**: [`HostDecisionHandler`] is registered with the process engine's
//!   `HandlerBuilder`, so a Decision node opens a durable task and parks the Run on it.
//! - **Who may act**: a role's members are the active `role_assignment` records of that role,
//!   each held by an active user; a user's manager is the `manager` link of their `user` record.
//!   An inactive user is never a role member, a manager, an approver or the target of a delegation
//!   or a reassignment. `InboxAccess` is `PermissionAccess` over the permission engine the site
//!   serves *now* (a customisation swap is seen on the next call), so a reader of the guarded
//!   document may see the task but never act on it.
//! - **SLA**: reminders, escalations and the declared final action fire from the timers of
//!   `EngineSlaTimers` over this module's own timer port. A timer is only an index: the truth is
//!   the open task row and its event trail, so the lease holder re-arms every open task from the
//!   store when it takes the lease (after a restart or a change of leader) and every few minutes
//!   after that. Auto-approval is the crate's option B (a node declares `sla_auto_approve`, no
//!   `distinct_from_creator`, no `duty`; the actor is `system:sla`, never counted as an approver,
//!   a metric and a supervisor notice on each).
//! - **Notices**: assignment, reminder, escalation and completion notices go to the
//!   [`TaskNoticeSink`] port when a module provides one. Best effort: a sink that fails, panics or
//!   is absent is counted and never fails an action.
//! - **Flushing after a node** (WP-BPM-05.T13): `TaskService::open_task` is synchronous (the
//!   engine's `NodeHandler::run` is), so the Decision node only queues its `Assigned` notice.
//!   [`FlushingHandler`] wraps the handler registered with the engine and nudges the
//!   [`NoticePump`] when the node returns; the pump, a service of its own, calls
//!   `TaskService::flush_notifications` after a short settle time, on a runtime task, never inside
//!   the handler and never under a lock. Each send is bounded by [`SEND_TIMEOUT`]
//!   ([`BoundedSink`]) and a whole flush by [`FLUSH_TIMEOUT`]; a timeout drops that notice like any
//!   other delivery failure and never fails an engine step or changes a stored state. The
//!   actions of the inbox (`complete`, `delegate`, `reassign`) and the SLA tick flush for
//!   themselves inside the task service. A restart loses queued, unsent notices (WP-BPM-05.T14).
//! - **The production `TaskPort`**: [`Inbox`], offered with `ctx.provide::<dyn TaskPort>(..)` and
//!   mounted on the site router. The caller is whoever the bearer credential names (the port
//!   authenticates again, as every hosted route group does); the request never names an actor.
//!
//! # Threads
//!
//! `PgTaskStore` and the task service are synchronous, and the bridge refuses a caller that is on
//! a runtime thread (`pg_bridge.wrong_thread`). Every port call therefore hops to a short-lived
//! plain thread, drives the task service there, and reaches back to the runtime only through
//! [`Hop`]: the document service (facts), the identity reads, the process engine's `signal` and
//! the notice sink all run as tasks on the host's runtime, awaited from the plain thread. The
//! engine's synchronous `NodeHandler` hook runs on a runtime worker, so [`HostDecisionHandler`]
//! leaves the runtime with `block_in_place` and runs the node on a scoped plain thread; a
//! current-thread runtime fails the node closed with a stable code.
//!
//! # Fail closed
//!
//! A directory read that fails answers "nobody" (a role is empty, a user has no manager), a
//! permission engine that is unavailable denies, a document that cannot be read gives no facts,
//! and a hop that cannot reach the runtime answers `server.unavailable`. None of them allows.
//!
//! # Configuration
//!
//! `[services.process_tasks]`: `enabled` (default `true`), `tick_secs` (default 15, bounded 5 to
//! 300) and an optional `[services.process_tasks.calendar]` table (`days`, `start`, `end`,
//! `utc_offset_minutes`). With no calendar SLAs run on wall time and readiness says so
//! (`process_tasks_calendar`). See `docs/running-spot/process_tasks.md`.
//!
//! # Audit and logs
//!
//! Target `spot::audit::task`: a line for every inbox call (who, which task, which action, the
//! outcome class tried and the result code), refusals included. A line never carries a document
//! value, a comment, a form value or a token; ids appear in logs, never in metric labels. Counters
//! carry no label at all.

#[path = "process_tasks/adapters.rs"]
mod adapters;
#[path = "process_tasks/calendar.rs"]
mod calendar;
#[path = "process_tasks/host.rs"]
mod host;
#[path = "process_tasks/identity.rs"]
mod identity;
#[path = "process_tasks/inbox.rs"]
mod inbox;
#[path = "process_tasks/node.rs"]
mod node;
#[path = "process_tasks/ops.rs"]
mod ops;
#[path = "process_tasks/seams.rs"]
mod seams;
#[path = "process_tasks/sla.rs"]
mod sla;
#[path = "process_tasks/threads.rs"]
mod threads;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use spot_app_api::prelude::SiteId;
use spot_bpm_engine::{NodeCtx, NodeHandler, NodeKind, NodeOutcome};
use spot_bpm_tasks::{
    ApproverResolver, BusinessCalendar, NoApprovers, TaskService, WallClockCalendar,
};
use spot_bpm_tasks_pg::PgTaskStore;
use spot_http::api::Ports;
use spot_http::api::ports::task::TaskPort;
use spot_http::api::router::routes::{self, Registration};
use spot_http::api::service::BoxFuture;
use spot_pg_bridge::{Bridge, BridgeConfig};
use tokio::runtime::Handle;
use tokio::sync::{Notify, watch};

pub use adapters::HostEngine;
pub use calendar::{CalendarSettings, Settings, WorkCalendar};
pub use host::{TaskHost, Wiring};
pub use identity::StorageDirectory;
pub use inbox::Inbox;
pub use node::HostDecisionHandler;
pub use seams::{
    Callers, DecisionSource, StatsSnapshot, TaskDirectory, TaskNotice, TaskNoticeSink,
};
pub use threads::Hop;

use crate::services::partition::on_plain_thread;
use crate::services::ports::OutboxLease;
use crate::services::process::{HandlerBuilder, ProcessHost};
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};
use identity::{LiveCallers, LiveDecisions, LiveFacts, LivePrincipals};
use sla::{CalendarNotice, SlaService};
use threads::lock;

/// Start order: after the process engine (1100), which provides [`HandlerBuilder`] and
/// [`ProcessHost`], and after the Document node module (1110).
pub const ORDER: u32 = 1120;

/// The audit target of this module.
const AUDIT_TARGET: &str = "spot::audit::task";

/// The consumer name the SLA loop's lease is held under (`outbox:<site>:process_tasks`).
const LEASE_CONSUMER: &str = "process_tasks";

/// The stable code of a Decision node that cannot run because no multi-thread runtime exists.
const CODE_NO_RUNTIME: &str = "bpm_tasks.no_runtime";

/// The stable code of a Decision node whose plain thread could not run or panicked.
const CODE_THREAD: &str = "bpm_tasks.thread_failed";

/// The segregation code the crate maps to `DistinctFromCreator`.
const DISTINCT_CODE: &str = "bpm_tasks.distinct_from_creator";

/// The segregation code the crate maps to `SegregationBlocked`.
const SOD_CODE: &str = "bpm_tasks.sod_conflict";

/// How many ticks pass between two full re-arms from the store (the self-heal for a timer that
/// fired and whose step then failed to commit).
const REARM_EVERY_TICKS: u64 = 40;

// ---------------------------------------------------------------------------------------------
// Notices after a node (WP-BPM-05.T13).
// ---------------------------------------------------------------------------------------------

/// How long one notice may take to leave through the sink. A sink that does not answer in time
/// drops that notice, like any other delivery failure (counted as failed).
pub const SEND_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one flush may take in all. A flush that runs out of time stops where it is: what is
/// still queued goes out on the next flush, or is lost with the queue's bound.
pub const FLUSH_TIMEOUT: Duration = Duration::from_secs(60);

/// How long the pump waits after a node returned before it flushes, so the engine has stored the
/// node's outcome by then (the handler returns, the engine records the result in the same step).
pub const SETTLE: Duration = Duration::from_millis(50);

/// How often the pump flushes with nothing to wake it: a backstop for a wake that was missed.
/// An empty queue costs no send.
pub const IDLE_FLUSH: Duration = Duration::from_secs(5);

/// A [`TaskNoticeSink`] whose every send is bounded in time with the runtime's timer, so a sink
/// that hangs (an SMTP server that never answers) cannot stall the flush that called it.
pub struct BoundedSink {
    inner: Arc<dyn TaskNoticeSink>,
    timeout: Duration,
}

impl BoundedSink {
    /// `inner`, with each send limited to `timeout`.
    #[must_use]
    pub fn new(inner: Arc<dyn TaskNoticeSink>, timeout: Duration) -> Self {
        Self { inner, timeout }
    }
}

impl TaskNoticeSink for BoundedSink {
    fn deliver<'a>(
        &'a self,
        notice: &'a TaskNotice,
    ) -> BoxFuture<'a, std::result::Result<(), String>> {
        Box::pin(async move {
            match tokio::time::timeout(self.timeout, self.inner.deliver(notice)).await {
                Ok(answer) => answer,
                Err(_) => Err("the notice was not delivered in time".to_owned()),
            }
        })
    }
}

/// Sends the `Assigned` notices a Decision node queued.
///
/// `TaskService::open_task` is synchronous (the engine's `NodeHandler::run` is), so it only
/// queues the notice. The pump is the host's flush: [`FlushingHandler`] wakes it after the node
/// returned, and it calls `TaskService::flush_notifications` on its own runtime task, never
/// inside the node handler and never while holding a lock. A flush that fails or times out never
/// changes a stored state and never fails the engine step, which has finished by then.
pub struct NoticePump {
    service: Arc<TaskService>,
    wake: Notify,
    settle: Duration,
    idle: Duration,
    flush_timeout: Duration,
}

impl NoticePump {
    /// A pump over `service` with the production timings.
    #[must_use]
    pub fn new(service: Arc<TaskService>) -> Self {
        Self::with_timings(service, SETTLE, IDLE_FLUSH, FLUSH_TIMEOUT)
    }

    /// A pump with explicit timings (tests use short ones).
    #[must_use]
    pub fn with_timings(
        service: Arc<TaskService>,
        settle: Duration,
        idle: Duration,
        flush_timeout: Duration,
    ) -> Self {
        Self {
            service,
            wake: Notify::new(),
            settle,
            idle: idle.max(Duration::from_millis(1)),
            flush_timeout,
        }
    }

    /// Asks for a flush soon. Cheap, synchronous and safe from any thread: wakes that arrive
    /// before the pump runs are remembered as one.
    pub fn nudge(&self) {
        self.wake.notify_one();
    }

    /// Flushes the queue now, bounded by the flush timeout. Whatever happens, nothing is
    /// returned: a notice is never a precondition of a state change.
    pub async fn flush(&self) {
        if tokio::time::timeout(self.flush_timeout, self.service.flush_notifications())
            .await
            .is_err()
        {
            tracing::warn!("flushing the task notices ran out of time");
        }
    }

    /// Serves until `stop` is set: flushes after every nudge (once the settle time has passed)
    /// and every idle period, and once more on the way out.
    pub async fn serve(&self, mut stop: watch::Receiver<bool>) {
        let mut idle = tokio::time::interval(self.idle);
        idle.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            if *stop.borrow() {
                break;
            }
            tokio::select! {
                changed = stop.changed() => {
                    if changed.is_err() || *stop.borrow() {
                        break;
                    }
                }
                () = self.wake.notified() => {
                    tokio::time::sleep(self.settle).await;
                    self.flush().await;
                }
                _ = idle.tick() => self.flush().await,
            }
        }
        self.flush().await;
    }
}

impl Service for NoticePump {
    fn name(&self) -> &'static str {
        "process_tasks_notices"
    }

    fn level(&self) -> Level {
        // Notices are best effort: losing the pump must not take document serving down.
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async { Ok(()) })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            self.serve(stop).await;
            Ok(())
        })
    }
}

/// The Decision node handler of the process engine, followed by a nudge of the [`NoticePump`]:
/// the node queues its `Assigned` notice, the engine stores the node's outcome, and the pump
/// sends the notice after that. The wrapper changes nothing about the node's outcome.
pub struct FlushingHandler {
    inner: Arc<dyn NodeHandler>,
    pump: Arc<NoticePump>,
}

impl FlushingHandler {
    /// `inner`, followed by a nudge of `pump` after each run.
    #[must_use]
    pub fn new(inner: Arc<dyn NodeHandler>, pump: Arc<NoticePump>) -> Self {
        Self { inner, pump }
    }
}

impl NodeHandler for FlushingHandler {
    fn kind(&self) -> NodeKind {
        self.inner.kind()
    }

    fn run(&self, ctx: &mut NodeCtx<'_>) -> NodeOutcome {
        let outcome = self.inner.run(ctx);
        self.pump.nudge();
        outcome
    }
}

/// The hosts of the sites built so far, by site name: a reload builds the site again without
/// building the host again, and its routes must reach the same port.
fn built() -> &'static Mutex<HashMap<String, Arc<TaskHost>>> {
    static BUILT: OnceLock<Mutex<HashMap<String, Arc<TaskHost>>>> = OnceLock::new();
    BUILT.get_or_init(Mutex::default)
}

/// The route group of the inbox, registered by hand: `spot-http` mounts only the groups it is
/// given ports for, and the host gives the router its ports one module at a time.
fn registration() -> Registration {
    Registration {
        name: "task",
        serves: routes::task::SERVES,
        required_ports: routes::task::required_ports,
        routes: routes::task::routes,
        public: None,
    }
}

/// Registers the task inbox: the store and its bridge, the Decision node handler, the SLA service
/// and the `TaskPort` with its routes. Inert when `services.process_tasks.enabled = false`.
///
/// # Errors
/// A setting that does not decode or is out of range; a bridge, a store or a table that cannot be
/// built; a port another module already provided.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("process_tasks")?;
    settings
        .validate()
        .map_err(|why| Error(format!("services.process_tasks.{why}")))?;
    if !settings.enabled {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site_name = env.site.site.clone();
    let host = if ctx.assembling() {
        let host = assemble(ctx, &settings)?;
        lock(built()).insert(site_name, Arc::clone(&host));
        host
    } else {
        match lock(built()).get(&site_name).cloned() {
            Some(host) => host,
            None => return Ok(()),
        }
    };
    ctx.provide::<dyn TaskPort>(host.port())?;
    // The process API reads it to show a run to the people its tasks are tied to.
    ctx.provide::<dyn spot_bpm_tasks::TaskStore>(host.store())?;
    ctx.provide::<dyn crate::services::process_api::CaseTaskCanceller>(host.canceller())?;
    let mut ports = Ports::new();
    ports.provide::<dyn TaskPort>(host.port());
    // The group is mounted here, not by the route registry. The host authenticates every request
    // before the router sees it, and the port authenticates again.
    let mounted = routes::mount(&[registration()], &ports, &[]);
    if let Some((module, reason)) = mounted.report.skipped.first() {
        tracing::error!(
            module = *module,
            ?reason,
            "the task routes were not mounted"
        );
    }
    let router = mounted.authenticated;
    ctx.router_layer(move |base| base.merge(router));
    Ok(())
}

/// Builds the host for a first build of the site.
fn assemble(ctx: &mut SiteCtx<'_>, settings: &Settings) -> Result<Arc<TaskHost>> {
    let env = *ctx.env()?;
    let site_name = env.site.site.clone();
    let site = SiteId::new(&site_name).map_err(crate::error)?;
    let runtime = Handle::try_current()
        .map_err(|_| Error("the task inbox needs to be built inside the async runtime".into()))?;
    let clock = Arc::clone(ctx.clock());
    let builder = ctx.require_port::<HandlerBuilder>()?;
    let process = ctx.require_port::<ProcessHost>()?;
    let outbox = ctx.require_port::<dyn OutboxLease>()?;

    let pool = env.backend.pool();
    let pool_size = pool.status().max_size;
    let largest = BridgeConfig::largest_in_flight(pool_size);
    if largest < 1 {
        return Err(Error(format!(
            "cli.process_tasks_pool: the task inbox needs a PostgreSQL pool of at least 3 \
             connections (postgres.pool_max_size is {pool_size})"
        )));
    }
    let config = BridgeConfig {
        name: "process_tasks".to_owned(),
        max_in_flight: largest.min(4),
        workers: 2,
        queue_capacity: 16,
        default_deadline: Duration::from_secs(10),
        max_deadline: Duration::from_secs(30),
    };
    let backend = env.backend.clone();
    let bridge = Arc::new(on_plain_thread("task bridge", move || {
        Bridge::for_backend(config, &backend).map_err(|e| Error(e.to_string()))
    })?);
    let store = Arc::new(
        PgTaskStore::new(Arc::clone(&bridge), &pool, &site_name)
            .map_err(|e| Error(e.to_string()))?,
    );
    {
        let store = Arc::clone(&store);
        on_plain_thread("task tables", move || {
            store.prepare().map_err(|e| Error(e.to_string()))
        })?;
    }

    let calendar: Arc<dyn BusinessCalendar> = match &settings.calendar {
        Some(calendar) => Arc::new(WorkCalendar::parse(calendar).map_err(Error)?),
        None => Arc::new(WallClockCalendar),
    };
    let hop = Hop::new(runtime.clone());
    let live = Arc::clone(env.live);
    let host = Wiring {
        site: site_name.clone(),
        clock: Arc::clone(&clock),
        runtime: runtime.clone(),
        store,
        callers: Arc::new(LiveCallers {
            live: Arc::clone(&live),
            site: site_name,
        }),
        directory: Arc::new(StorageDirectory::new(
            env.backend.clone(),
            Arc::clone(&env.permission.catalog),
            Arc::clone(&clock),
            runtime.clone(),
        )),
        principals: Arc::new(LivePrincipals {
            live: Arc::clone(&live),
            hop: hop.clone(),
        }),
        facts: Arc::new(LiveFacts {
            live: Arc::clone(&live),
            site,
            clock: Arc::clone(&clock),
            hop,
        }),
        decisions: Arc::new(LiveDecisions { live }),
        resolver: ctx
            .port::<dyn ApproverResolver>()
            .unwrap_or_else(|| Arc::new(NoApprovers)),
        engine: Arc::new(HostEngine::new(process, runtime)),
        // Every send is bounded in time: a sink that hangs drops that notice and nothing else.
        sink: ctx
            .port::<dyn TaskNoticeSink>()
            .map(|sink| Arc::new(BoundedSink::new(sink, SEND_TIMEOUT)) as Arc<dyn TaskNoticeSink>),
        calendar,
    }
    .build();
    let host = Arc::new(host);

    // The Decision node only queues its `Assigned` notice (the engine's handler is synchronous);
    // the pump sends it after the node returned.
    let pump = Arc::new(NoticePump::new(host.service()));
    builder.register(Arc::new(FlushingHandler::new(
        host.handler(),
        Arc::clone(&pump),
    )));
    ctx.add(pump);
    ctx.add(Arc::new(SlaService {
        core: Arc::clone(&host.core),
        bridge,
        outbox,
        interval: Duration::from_secs(settings.tick_secs),
    }));
    if settings.calendar.is_none() {
        ctx.add(Arc::new(CalendarNotice));
    }
    Ok(host)
}
