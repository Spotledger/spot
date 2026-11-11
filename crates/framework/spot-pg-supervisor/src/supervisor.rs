//! The supervisor state machine: owns the `postgres` child, publishes its state on a
//! `tokio::sync::watch` channel, performs graceful stops and crash restarts within a
//! budget, and refuses to start over a live postmaster.
//!
//! Timing flows through the injected [`Clock`](crate::clock::Clock) (timestamps,
//! backoff, restart and stability windows) while the readiness poll and the start
//! timeout use tokio's pause-able time, so tests drive every delay without sleeping.

use crate::clock::Clock;
use crate::config::SupervisorConfig;
use crate::error::SupervisorError;
use crate::events::{EventStream, SupervisorEvent};
use crate::extensions::{ExtensionProvisioned, ExtensionProvisioner, REQUIRED_EXTENSIONS};
use crate::init::InitState;
use crate::layout::PgLayout;
use crate::pidfile::{LockOutcome, clean_stale_or_fail};
use crate::platform;
use crate::settings::PgSettings;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, broadcast, mpsc, watch};

/// How often the readiness poll re-checks the port/probe while `Starting`.
pub(crate) const READINESS_TICK: Duration = Duration::from_millis(50);

/// Lifecycle of the supervised child, published on a watch channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorState {
    /// Not running (never started, or stopped by a call).
    Stopped,
    /// The child is up but not accepting connections yet.
    Starting,
    /// The port accepts connections (and the optional probe passed).
    Ready,
    /// A graceful stop is in progress.
    Stopping,
    /// Crash restart in progress (`attempt` is 1-based).
    Restarting {
        /// 1-based restart attempt.
        attempt: u32,
    },
    /// Terminal failure: start timeout, live postmaster or restart budget exhausted.
    Failed {
        /// Human-readable cause; never contains secrets.
        reason: String,
    },
}

/// How a requested stop ended. `StoppedForced` tells the caller that recovery will run
/// at the next start (the brief's forced path of `Handle::stop`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoppedHow {
    /// The child exited inside the grace period.
    Graceful,
    /// The child was killed after the grace period elapsed.
    StoppedForced,
}

/// A stop request flowing from [`Handle::stop`](Handle::stop) to the monitor task.
#[derive(Debug, Clone, Copy)]
pub(crate) struct StopRequest {
    /// Grace period before the kill.
    pub(crate) grace: Duration,
}

/// Bookkeeping shared between entry points and the monitor task.
#[derive(Default)]
pub(crate) struct InternalState {
    /// Wall-clock ms (from the clock) of each restart inside the budget window.
    pub(crate) restart_times_ms: Vec<u64>,
    /// Set once the restart budget is exhausted; a fresh `start()` clears it.
    pub(crate) budget_exhausted: bool,
}

/// Supervises one bundled PostgreSQL cluster.
pub struct Supervisor {
    pub(crate) layout: PgLayout,
    pub(crate) settings: PgSettings,
    pub(crate) config: SupervisorConfig,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) state_tx: watch::Sender<SupervisorState>,
    /// Kept so the watch channel never closes even with zero external subscribers;
    /// state publications before the first `subscribe()` would otherwise be lost.
    pub(crate) _state_rx: watch::Receiver<SupervisorState>,
    pub(crate) event_tx: broadcast::Sender<SupervisorEvent>,
    pub(crate) internal: Arc<Mutex<InternalState>>,
    /// The host's `extension.install` adapter (see [`crate::extensions`]). `None` on a
    /// supervisor that does not host a desktop data directory — the snapshot and fault
    /// harness supervisors, and this crate's own tests of the supervision mechanics.
    /// The desktop service wrapper always installs one; there is no switch that makes a
    /// *failed* install a warning.
    pub(crate) extension_provisioner: Option<Arc<dyn ExtensionProvisioner>>,
}

impl Supervisor {
    /// Builds a supervisor for `layout`. Syncs the port from the layout into the
    /// settings so the rendered `spot.conf` and the readiness probe always agree.
    pub fn new(layout: PgLayout, mut settings: PgSettings, config: SupervisorConfig) -> Self {
        settings.port = layout.port;
        let (state_tx, state_rx) = watch::channel(SupervisorState::Stopped);
        let (event_tx, _) = broadcast::channel(256);
        Supervisor {
            layout,
            settings,
            config,
            clock: Arc::new(crate::clock::SystemClock),
            state_tx,
            _state_rx: state_rx,
            event_tx,
            internal: Arc::new(Mutex::new(InternalState::default())),
            extension_provisioner: None,
        }
    }

    /// Installs the host's `extension.install` adapter for the first-start gate
    /// (ADR-087 section 4.3): after `initdb` and the first `Ready` the supervisor calls
    /// it for `postgis` and publishes `Ready` only once it succeeded.
    ///
    /// The desktop service wrapper (WP-PRD-03) always installs one. A supervisor without
    /// one supervises PostgreSQL alone; a *failing* adapter is always `Failed`, never a
    /// warning, and no configuration value can soften it.
    pub fn with_extension_provisioner(
        mut self,
        provisioner: Arc<dyn ExtensionProvisioner>,
    ) -> Self {
        self.extension_provisioner = Some(provisioner);
        self
    }

    /// Replaces the clock (test seam).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Reads the current published state.
    pub fn state(&self) -> SupervisorState {
        self.state_tx.borrow().clone()
    }

    /// Watches the published state.
    pub fn subscribe(&self) -> watch::Receiver<SupervisorState> {
        self.state_tx.subscribe()
    }

    /// Streams supervision events (state changes, restarts, forced stops, stale-lock
    /// cleanups). Each stream carries its own event cursor.
    pub fn events(&self) -> EventStream {
        EventStream {
            rx: self.event_tx.subscribe(),
        }
    }

    /// Runs `initdb` exactly once: fresh cluster with `--auth-local=peer`,
    /// `--auth=scram-sha-256`, UTF-8 and `--no-instructions`; `PG_VERSION` already
    /// present means nothing is run; a half-initialised data directory is refused,
    /// never deleted. The password travels through a protected temporary `--pwfile`
    /// that is removed afterwards — it never touches the command line or any log.
    ///
    /// # Errors
    /// Typed [`SupervisorError`].
    pub async fn ensure_initialised(&self, password: &str) -> Result<InitState, SupervisorError> {
        if password.is_empty() {
            return Err(SupervisorError::MissingPassword);
        }
        crate::init::initdb_once(&self.layout, &self.settings, password).await
    }

    /// `true` while the last run ended in the restart-budget failure.
    pub fn is_budget_exhausted(&self) -> bool {
        self.internal
            .try_lock()
            .map(|internal| internal.budget_exhausted)
            .unwrap_or(false)
    }

    /// Clears the restart budget and the terminal failure so `start()` may run again.
    pub async fn reset(&self) {
        let mut internal = self.internal.lock().await;
        internal.budget_exhausted = false;
        internal.restart_times_ms.clear();
        if matches!(
            self.state_tx.borrow().clone(),
            SupervisorState::Failed { .. }
        ) {
            let _ = self.state_tx.send(SupervisorState::Stopped);
        }
    }

    /// Validates the layout, clears (or refuses) a stale `postmaster.pid`, spawns
    /// `postgres -D <data_dir>` as a child process, installs the death guarantees and
    /// the monitor task, then returns a [`Handle`].
    ///
    /// # Errors
    /// Typed [`SupervisorError`] — including the refusal over a live postmaster.
    pub async fn start(&self) -> Result<Handle, SupervisorError> {
        if !matches!(
            self.state(),
            SupervisorState::Stopped | SupervisorState::Failed { .. }
        ) {
            return Err(SupervisorError::InvalidState {
                action: "start".to_string(),
                state: self.state(),
            });
        }

        // Required binaries present, data dir placement sane, port free.
        self.layout.validate().await?;

        // Stale-lock gate, before anything is spawned.
        let outcome = clean_stale_or_fail(&self.layout).await?;
        match outcome {
            LockOutcome::Live(pid) => {
                self.push_event(SupervisorEvent::LiveLockRefused {
                    at_ms: self.clock.now_ms(),
                    pid,
                });
                return Err(SupervisorError::LivePostmaster {
                    pid,
                    path: self.layout.postmaster_pid_path().display().to_string(),
                });
            }
            LockOutcome::CleanedStale(pid) => {
                self.push_event(SupervisorEvent::StaleLockCleaned {
                    at_ms: self.clock.now_ms(),
                    pid,
                });
            }
            LockOutcome::Absent => {}
        }

        {
            // A fresh supervision round: budget cleared, restart log emptied.
            let mut internal = self.internal.lock().await;
            internal.restart_times_ms.clear();
            internal.budget_exhausted = false;
        }
        if matches!(self.state(), SupervisorState::Failed { .. }) {
            let _ = self.state_tx.send(SupervisorState::Stopped);
        }

        let proxy = self.proxy();
        let (stop_tx, stop_rx) = mpsc::unbounded_channel();
        let child = proxy.spawn_postgres().await?;
        proxy.set_state(SupervisorState::Starting).await;
        self.spawn_monitor(proxy.clone(), stop_rx, child);

        Ok(Handle {
            state_rx: self.state_tx.subscribe(),
            event_tx: self.event_tx.clone(),
            stop_tx,
        })
    }

    pub(crate) fn push_event(&self, event: SupervisorEvent) {
        let _ = self.event_tx.send(event);
    }

    pub(crate) fn proxy(&self) -> SupervisorProxy {
        SupervisorProxy {
            layout: self.layout.clone(),
            settings: self.settings.clone(),
            config: self.config.clone(),
            clock: Arc::clone(&self.clock),
            state_tx: self.state_tx.clone(),
            event_tx: self.event_tx.clone(),
            internal: Arc::clone(&self.internal),
            extension_provisioner: self.extension_provisioner.clone(),
        }
    }

    /// The monitor task owns the child through stop, crash and restart generations.
    fn spawn_monitor(
        &self,
        proxy: SupervisorProxy,
        stop_rx: mpsc::UnboundedReceiver<StopRequest>,
        child: tokio::process::Child,
    ) {
        tokio::spawn(monitor_loop(proxy, stop_rx, child));
    }
}

/// The monitor task's view of the supervisor; per-generation clones share the watch and
/// broadcast channels and the internal bookkeeping.
#[derive(Clone)]
pub(crate) struct SupervisorProxy {
    pub(crate) layout: PgLayout,
    pub(crate) settings: PgSettings,
    pub(crate) config: SupervisorConfig,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) state_tx: watch::Sender<SupervisorState>,
    pub(crate) event_tx: broadcast::Sender<SupervisorEvent>,
    pub(crate) internal: Arc<Mutex<InternalState>>,
    pub(crate) extension_provisioner: Option<Arc<dyn ExtensionProvisioner>>,
}

impl SupervisorProxy {
    pub(crate) async fn set_state(&self, state: SupervisorState) {
        let at_ms = self.clock.now_ms();
        let _ = self.state_tx.send(state.clone());
        let _ = self
            .event_tx
            .send(SupervisorEvent::StateChanged { at_ms, state });
    }

    pub(crate) fn push_event(&self, event: SupervisorEvent) {
        let _ = self.event_tx.send(event);
    }

    /// Exponential restart backoff: 1 s, 2 s, 4 s, ..., capped at 30 s.
    pub(crate) fn backoff_for_attempt(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(5);
        Duration::from_secs(1u64 << shift)
    }

    /// Spawns `postgres -D <data_dir>`, output piped into the rotating log files.
    pub(crate) async fn spawn_postgres(&self) -> Result<tokio::process::Child, SupervisorError> {
        tokio::fs::create_dir_all(&self.layout.log_dir)
            .await
            .map_err(|source| SupervisorError::ChildProcess {
                context: "creating the log directory".to_string(),
                source,
            })?;
        let mut cmd = tokio::process::Command::new(self.layout.postgres_bin());
        cmd.arg("-D").arg(&self.layout.data_dir);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        // If the monitor task is dropped mid-loop (runtime shutdown), tokio kills the
        // child; the spawn-level guarantees (PDEATHSIG / job object) cover the harsher
        // case of the whole supervisor process dying.
        cmd.kill_on_drop(true);
        // The bundled postmaster binds the layout port through the standard PGPORT.
        cmd.env("PGPORT", self.settings.port.to_string());
        for (key, value) in &self.config.child_env {
            cmd.env(key, value);
        }
        platform::configure_spawn(&mut cmd);
        // A respawn right after a kill can race the dying generation still holding
        // the binary open for exec (ETXTBSY); retry on real wall time (this is an OS
        // process transition, not supervisor logic, so the Clock does not gate it).
        let mut spawned_child = None;
        for attempt in 0..10u32 {
            match cmd.spawn() {
                Ok(spawned) => {
                    spawned_child = Some(spawned);
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    tracing::warn!(attempt, "spawn raced a dying generation; retrying");
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(source) => {
                    return Err(SupervisorError::ChildProcess {
                        context: format!("spawning {}", self.layout.postgres_bin().display()),
                        source,
                    });
                }
            }
        }
        let Some(mut child) = spawned_child else {
            return Err(SupervisorError::ChildProcess {
                context: format!("spawning {}", self.layout.postgres_bin().display()),
                source: std::io::Error::other("the postgres binary stayed busy after ten attempts"),
            });
        };
        platform::after_spawn(&child);
        if let Some(stdout) = child.stdout.take() {
            spawn_log_pump(stdout, self.log_target(concat!("server", ".log")));
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_log_pump(stderr, self.log_target(concat!("server", ".err", ".log")));
        }
        Ok(child)
    }

    fn log_target(&self, name: &str) -> PathBuf {
        self.layout.log_dir.join(name)
    }
}

/// One readiness probe evaluation round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbeProgress {
    /// No TCP connection seen yet.
    NoConnection,
    /// TCP accepted; running the optional probe each tick (or no probe configured).
    ConnectionOnly,
}

/// The outcome of trying to reach readiness.
#[derive(Debug)]
enum Readiness {
    /// Port accepted (and probe passed) — the state is now usable.
    Ready,
    /// The stop request finished with a graceful exit.
    StoppedGracefully,
    /// The stop request ended in a kill after the grace period.
    StoppedForced,
    /// The child crashed while starting or waiting failed (message is the reason).
    Crashed(String),
    /// The start timeout elapsed before readiness.
    StartTimeout,
    /// Every handle and the supervisor are gone; the test harness is shutting down.
    SupervisorGone,
}

/// What to do after the stable (ready) phase observed the child.
enum Stable {
    /// The stop request finished gracefully.
    ExitedWhileStopping,
    /// The stop request was forced.
    ExitedForced,
    /// The child crashed while ready (message is the reason).
    Crashed(String),
    /// Every handle and the supervisor are gone.
    SupervisorGone,
}

/// The terminal publication after a supervision round.
enum RoundEnd {
    /// Published `Stopped`.
    Stopped,
    /// Published `Stopped` after a `ForcedStop` event.
    StoppedForced,
    /// Published `Failed { reason }`.
    Failed(String),
    /// Nothing to publish (the supervisor itself is gone).
    SupervisorGone,
}

/// The monitor loop: one call per supervision round from `start()`; across crash
/// restarts it re-enters the readiness phase with a respawned child.
pub(crate) async fn monitor_loop(
    proxy: SupervisorProxy,
    mut stop_rx: mpsc::UnboundedReceiver<StopRequest>,
    mut child: tokio::process::Child,
) {
    tracing::debug!("monitor loop started");
    // The extension gate runs once per supervision round, on the first `Ready` only: a
    // crash restart brings the same cluster back, and the extension is a property of the
    // data directory, not of the process generation.
    let mut extensions_provisioned = false;
    let end = loop {
        // Phase 1: become ready.
        let reached = readiness_phase(&proxy, &mut child, &mut stop_rx).await;
        match reached {
            Readiness::Ready => {
                // Phase 1.5 (ADR-087 section 4.3): the data directory is not reported
                // usable until PostGIS exists in it. A failure stops the child and the
                // round ends `Failed`; `Ready` is never published.
                if !extensions_provisioned {
                    extensions_provisioned = true;
                    if let Err(error) = provision_required_extensions(&proxy).await {
                        let _ = perform_graceful_stop(&proxy, &mut child, proxy.config.stop_grace)
                            .await;
                        break RoundEnd::Failed(error.to_string());
                    }
                }
                proxy.set_state(SupervisorState::Ready).await;
            }
            Readiness::StoppedGracefully => break RoundEnd::Stopped,
            Readiness::StoppedForced => break RoundEnd::StoppedForced,
            Readiness::StartTimeout => {
                platform::kill_child(&mut child).await;
                break RoundEnd::Failed(format!(
                    "start timeout after {:?}: the child never became ready",
                    proxy.config.start_timeout
                ));
            }
            Readiness::Crashed(reason) => {
                tracing::warn!(%reason, "postgres exited while starting; evaluating restart");
                if !prepare_restart(&proxy).await {
                    break RoundEnd::Failed("restart budget exhausted".to_string());
                }
                match respawn_after_backoff(&proxy, &mut stop_rx).await {
                    Some(next) => {
                        child = next;
                        continue;
                    }
                    // A stop request (or a gone supervisor) ended the round during the
                    // backoff; the state was published there.
                    None => break RoundEnd::SupervisorGone,
                }
            }
            Readiness::SupervisorGone => break RoundEnd::SupervisorGone,
        }

        // Phase 2: stay ready; watch for crashes, stops and the stability window.
        match stable_phase(&proxy, &mut child, &mut stop_rx).await {
            Stable::ExitedWhileStopping => break RoundEnd::Stopped,
            Stable::ExitedForced => break RoundEnd::StoppedForced,
            Stable::Crashed(reason) => {
                tracing::warn!(%reason, "postgres exited while ready; evaluating restart");
                if !prepare_restart(&proxy).await {
                    break RoundEnd::Failed("restart budget exhausted".to_string());
                }
                match respawn_after_backoff(&proxy, &mut stop_rx).await {
                    Some(next) => {
                        child = next;
                        continue;
                    }
                    None => break RoundEnd::SupervisorGone,
                }
            }
            Stable::SupervisorGone => break RoundEnd::SupervisorGone,
        }
    };

    publish_round_end(&proxy, end, &mut child).await;
}

/// Maps an exit status to a loggable reason (never containing secrets).
fn crash_reason(status: std::process::ExitStatus) -> String {
    if let Some(code) = status.code() {
        format!("postgres exited with code {code} while not stopping")
    } else {
        "postgres was terminated by a signal (crash) while not stopping".to_string()
    }
}
/// Publishes the supervision round's terminal state and guarantees nothing survives
/// when the end was not a stop.
async fn publish_round_end(
    proxy: &SupervisorProxy,
    end: RoundEnd,
    child: &mut tokio::process::Child,
) {
    match end {
        RoundEnd::Stopped => proxy.set_state(SupervisorState::Stopped).await,
        RoundEnd::StoppedForced => {
            // The ForcedStop event was already published at the kill; the state is
            // simply not running any more.
            proxy.set_state(SupervisorState::Stopped).await;
        }
        RoundEnd::Failed(reason) => {
            proxy.set_state(SupervisorState::Failed { reason }).await;
        }
        RoundEnd::SupervisorGone => {
            // Every Handle (and thus every stop channel sender) is gone while the
            // monitor still ran. The platform death guarantees own the child only if
            // the whole supervisor process dies — in-process, stop it directly.
            platform::kill_child(child).await;
            proxy.set_state(SupervisorState::Stopped).await;
        }
    }
}

/// Phase 1.5 (ADR-087 section 4.3): hands every required extension to the host's
/// `extension.install` adapter while the child is up, before `Ready` is published.
///
/// One call per extension and per supervision round; the platform operation is
/// idempotent, so every later start takes its "already present" path. A failure is
/// returned as the typed [`SupervisorError::PostgisUnprovisionable`] and the caller ends
/// the round `Failed`: nothing here warns and continues.
async fn provision_required_extensions(proxy: &SupervisorProxy) -> Result<(), SupervisorError> {
    let Some(provisioner) = proxy.extension_provisioner.as_ref() else {
        // No adapter installed: this supervisor does not host a desktop data directory
        // (see `Supervisor::with_extension_provisioner`).
        return Ok(());
    };
    for name in REQUIRED_EXTENSIONS {
        match provisioner.provision(name).await {
            Ok(receipt) => record_provisioned(proxy, receipt),
            Err(failure) => {
                proxy.push_event(SupervisorEvent::ExtensionProvisionFailed {
                    at_ms: proxy.clock.now_ms(),
                    extension: failure.extension.clone(),
                    reason: failure.reason.clone(),
                });
                // The reason is the host's typed text; the supervisor adds no connection
                // string, no password and no row value of its own (ADR-44).
                tracing::error!(
                    extension = %failure.extension,
                    reason = %failure.reason,
                    "the required extension could not be installed; the data directory is not usable"
                );
                return Err(crate::extensions::unprovisionable(&failure));
            }
        }
    }
    Ok(())
}

/// One successful provisioning call: the event and the log line.
fn record_provisioned(proxy: &SupervisorProxy, receipt: ExtensionProvisioned) {
    tracing::info!(
        extension = %receipt.extension,
        version = %receipt.version,
        already_present = receipt.already_present,
        "extension provisioned"
    );
    proxy.push_event(SupervisorEvent::ExtensionProvisioned {
        at_ms: proxy.clock.now_ms(),
        extension: receipt.extension,
        version: receipt.version,
        already_present: receipt.already_present,
    });
}

/// Records a crash restart inside the budget window; `false` means the budget is
/// exhausted and the caller must publish `Failed`.
async fn prepare_restart(proxy: &SupervisorProxy) -> bool {
    let now = proxy.clock.now_ms();
    let window_ms = proxy.config.restart_budget.window.as_millis() as u64;
    let max = proxy.config.restart_budget.max_restarts;
    let mut internal = proxy.internal.lock().await;
    internal
        .restart_times_ms
        .retain(|t| now.saturating_sub(*t) <= window_ms);
    if internal.restart_times_ms.len() as u32 >= max {
        internal.budget_exhausted = true;
        return false;
    }
    internal.restart_times_ms.push(now);
    let attempt = internal.restart_times_ms.len() as u32;
    drop(internal);
    proxy.push_event(SupervisorEvent::Restart {
        at_ms: now,
        attempt,
    });
    true
}

/// Sleeps the exponential backoff (abortable by a stop request), then respawns.
/// Returns `None` when the round ended during the backoff.
async fn respawn_after_backoff(
    proxy: &SupervisorProxy,
    stop_rx: &mut mpsc::UnboundedReceiver<StopRequest>,
) -> Option<tokio::process::Child> {
    let attempt = proxy.internal.lock().await.restart_times_ms.len() as u32;
    proxy
        .set_state(SupervisorState::Restarting { attempt })
        .await;
    let backoff = proxy.backoff_for_attempt(attempt);
    tokio::select! {
        () = proxy.clock.sleep(backoff) => {}
        request = stop_rx.recv() => {
            match request {
                // No child is running during the backoff: a stop simply ends the round.
                Some(_) | None => {
                    proxy.set_state(SupervisorState::Stopped).await;
                    return None;
                }
            }
        }
    }
    match proxy.spawn_postgres().await {
        Ok(child) => {
            proxy.set_state(SupervisorState::Starting).await;
            Some(child)
        }
        Err(error) => {
            proxy
                .set_state(SupervisorState::Failed {
                    reason: error.to_string(),
                })
                .await;
            None
        }
    }
}

/// Phase 1: wait for readiness (TCP + optional probe), the start timeout, a stop
/// request or a crash — whichever comes first.
async fn readiness_phase(
    proxy: &SupervisorProxy,
    child: &mut tokio::process::Child,
    stop_rx: &mut mpsc::UnboundedReceiver<StopRequest>,
) -> Readiness {
    match tokio::time::timeout(
        proxy.config.start_timeout,
        readiness_poll(proxy, child, stop_rx),
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_elapsed) => Readiness::StartTimeout,
    }
}

/// The readiness poll itself (bounded by the caller's timeout).
async fn readiness_poll(
    proxy: &SupervisorProxy,
    child: &mut tokio::process::Child,
    stop_rx: &mut mpsc::UnboundedReceiver<StopRequest>,
) -> Readiness {
    let mut progress = ProbeProgress::NoConnection;
    let mut tick = tokio::time::interval(READINESS_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(status) => return Readiness::Crashed(crash_reason(status)),
                    Err(error) => {
                        return Readiness::Crashed(format!("postgres wait failed: {error}"));
                    }
                }
            }
            request = stop_rx.recv() => {
                match request {
                    Some(request) => {
                        return match perform_graceful_stop(proxy, child, request.grace).await {
                            GracefulStop::Graceful => Readiness::StoppedGracefully,
                            GracefulStop::Forced => Readiness::StoppedForced,
                        };
                    }
                    None => return Readiness::SupervisorGone,
                }
            }
            _ = tick.tick() => {
                if progress == ProbeProgress::NoConnection
                    && tcp_probe(proxy).await
                {
                    tracing::debug!(port = proxy.settings.port, "readiness: TCP connection accepted");
                    progress = ProbeProgress::ConnectionOnly;
                }
                if progress == ProbeProgress::ConnectionOnly {
                    let passed = match proxy.config.ready_probe.as_ref() {
                        // No configured probe: the accepted TCP connection is readiness.
                        None => true,
                        Some(probe) => (probe)().await,
                    };
                    if passed {
                        return Readiness::Ready;
                    }
                    // A false probe holds the state at `Starting` and is retried on
                    // the next tick.
                }
            }
        }
    }
}

/// One TCP connect to the configured probe address; releases the connection at once.
async fn tcp_probe(proxy: &SupervisorProxy) -> bool {
    use tokio::net::TcpStream;
    let addr = proxy.layout.listen.first_probe_addr();
    match TcpStream::connect((addr, proxy.settings.port)).await {
        Ok(stream) => {
            drop(stream);
            true
        }
        Err(_) => false,
    }
}

/// Phase 2: the child is ready. Watch for crashes, stop requests and the stability
/// window (a restart older than the window no longer counts against the budget).
async fn stable_phase(
    proxy: &SupervisorProxy,
    child: &mut tokio::process::Child,
    stop_rx: &mut mpsc::UnboundedReceiver<StopRequest>,
) -> Stable {
    let mut tick = tokio::time::interval(READINESS_TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            status = child.wait() => {
                match status {
                    Ok(status) => return Stable::Crashed(crash_reason(status)),
                    Err(error) => {
                        return Stable::Crashed(format!("postgres wait failed: {error}"));
                    }
                }
            }
            request = stop_rx.recv() => {
                match request {
                    Some(request) => {
                        return match perform_graceful_stop(proxy, child, request.grace).await {
                            GracefulStop::Graceful => Stable::ExitedWhileStopping,
                            GracefulStop::Forced => Stable::ExitedForced,
                        };
                    }
                    None => return Stable::SupervisorGone,
                }
            }
            _ = tick.tick() => {
                prune_restart_window(proxy).await;
            }
        }
    }
}

/// Drops restart timestamps older than the stability window (the brief's counter
/// reset after a restart stays ready for the whole window).
async fn prune_restart_window(proxy: &SupervisorProxy) {
    if proxy.config.stable_window.is_zero() {
        return;
    }
    let now = proxy.clock.now_ms();
    let window_ms = proxy.config.stable_window.as_millis() as u64;
    let mut internal = proxy.internal.lock().await;
    internal
        .restart_times_ms
        .retain(|t| now.saturating_sub(*t) <= window_ms);
}

/// How a graceful stop ended.
enum GracefulStop {
    /// The child exited inside the grace period.
    Graceful,
    /// The child was killed after the grace period elapsed.
    Forced,
}

/// Asks the child for a fast shutdown, waits for the grace period, kills afterwards
/// and publishes the `ForcedStop` event on the kill path. Called from the monitor with
/// the state already at `Stopping`... is set here.
async fn perform_graceful_stop(
    proxy: &SupervisorProxy,
    child: &mut tokio::process::Child,
    grace: Duration,
) -> GracefulStop {
    proxy.set_state(SupervisorState::Stopping).await;
    platform::request_fast_stop(child, &proxy.layout.pg_ctl_bin(), &proxy.layout.data_dir);
    match tokio::time::timeout(grace, child.wait()).await {
        Ok(_) => GracefulStop::Graceful,
        Err(_elapsed) => {
            platform::kill_child(child).await;
            proxy.push_event(SupervisorEvent::ForcedStop {
                at_ms: proxy.clock.now_ms(),
            });
            GracefulStop::Forced
        }
    }
}

/// Pipes one child stream into its rotating log file. Logging never blocks the
/// supervisor; a failed append is warned and the chunk dropped.
pub(crate) fn spawn_log_pump(
    stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    path: PathBuf,
) {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut data = stream;
        let mut buf = vec![0u8; 4096];
        loop {
            match data.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if let Err(error) = append_rotated(&path, &buf[..n]).await {
                        tracing::warn!(%error, path = %path.display(), "server log append failed");
                    }
                }
            }
        }
    });
}

/// Appends to `path`, rotating to `path.1` ... `path.3` when the file passes 1 MiB.
async fn append_rotated(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    const MAX_LOG_BYTES: u64 = 1024 * 1024;
    const KEEP_LOGS: u32 = 3;
    let len = tokio::fs::metadata(path)
        .await
        .map(|meta| meta.len())
        .unwrap_or(0);
    if len.saturating_add(bytes.len() as u64) > MAX_LOG_BYTES {
        for index in (1..KEEP_LOGS).rev() {
            let from = rotated(path, index);
            let to = rotated(path, index + 1);
            if tokio::fs::try_exists(&from).await.unwrap_or(false) {
                tokio::fs::rename(&from, &to).await?;
            }
        }
        tokio::fs::rename(path, rotated(path, 1)).await?;
        tokio::fs::write(path, b"").await?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    file.write_all(bytes).await?;
    Ok(())
}

fn rotated(path: &std::path::Path, index: u32) -> std::path::PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".{index}"));
    path.with_file_name(name)
}

/// Live view of one supervision round plus the control channel to the monitor task.
pub struct Handle {
    state_rx: watch::Receiver<SupervisorState>,
    event_tx: broadcast::Sender<SupervisorEvent>,
    stop_tx: mpsc::UnboundedSender<StopRequest>,
}

impl Handle {
    /// Watches the published state.
    pub fn subscribe(&self) -> watch::Receiver<SupervisorState> {
        self.state_rx.clone()
    }

    /// Streams supervision events (state changes, restarts, forced stops, stale-lock
    /// cleanups). Each stream carries its own event cursor.
    pub fn events(&self) -> EventStream {
        EventStream {
            rx: self.event_tx.subscribe(),
        }
    }

    /// Waits until the supervisor reaches `Ready`.
    ///
    /// # Errors
    /// [`SupervisorError::FailedState`] when the supervisor reached `Failed` first,
    /// [`SupervisorError::InvalidState`] when it stopped instead.
    pub async fn ready(&self) -> Result<(), SupervisorError> {
        let mut state_rx = self.state_rx.clone();
        loop {
            {
                let state = state_rx.borrow().clone();
                match state {
                    SupervisorState::Ready => return Ok(()),
                    SupervisorState::Failed { reason } => {
                        return Err(SupervisorError::FailedState { reason });
                    }
                    SupervisorState::Stopped => {
                        return Err(SupervisorError::InvalidState {
                            action: "wait for readiness".to_string(),
                            state,
                        });
                    }
                    SupervisorState::Starting
                    | SupervisorState::Stopping
                    | SupervisorState::Restarting { .. } => {}
                }
            }
            state_rx
                .changed()
                .await
                .map_err(|_| SupervisorError::FailedState {
                    reason: "the supervisor was dropped".to_string(),
                })?;
        }
    }

    /// Asks for a fast shutdown, waits `grace` for the child to exit and kills it
    /// afterwards (reporting [`StoppedHow::StoppedForced`]). Calling `stop` twice is
    /// fine: the second call observes the terminal state and returns at once.
    ///
    /// # Errors
    /// [`SupervisorError::FailedState`] when the supervision round failed first (e.g.
    /// start timeout): there is a state to read, but no child to stop.
    pub async fn stop(&self, grace: Duration) -> Result<StoppedHow, SupervisorError> {
        match self.state_rx.borrow().clone() {
            // Calling stop twice is fine; the second call sees the terminal state.
            SupervisorState::Stopped | SupervisorState::Failed { .. } => {
                return Ok(StoppedHow::Graceful);
            }
            _ => {}
        }
        let _ = self.stop_tx.send(StopRequest { grace });
        // Events must be subscribed BEFORE the stop could act so the cursor covers
        // the ForcedStop marker the monitor publishes before its terminal state.
        let mut events = self.events();
        let (result, forced) = self.await_terminal(&mut events).await;
        match result {
            Ok(()) => Ok(if forced {
                StoppedHow::StoppedForced
            } else {
                StoppedHow::Graceful
            }),
            Err(reason) => {
                // The round failed before the stop could act; the reason names why.
                Err(SupervisorError::FailedState { reason })
            }
        }
    }

    /// Waits for the terminal state and determines from the buffered events whether
    /// the stop ended in a kill. Program order on the monitor side guarantees the
    /// `ForcedStop` event is published before the terminal `Stopped` state, so once
    /// the state is terminal the event is already buffered in `events` and a
    /// non-blocking drain sees it.
    async fn await_terminal(&self, events: &mut EventStream) -> (Result<(), String>, bool) {
        let mut state_rx = self.state_rx.clone();
        loop {
            if is_terminal(&state_rx.borrow().clone()) {
                break;
            }
            if state_rx.changed().await.is_err() {
                // The supervisor was dropped while we waited: over.
                break;
            }
        }
        let mut forced = false;
        while let Ok(event) = events.try_recv() {
            if matches!(event, SupervisorEvent::ForcedStop { .. }) {
                forced = true;
            }
        }
        let reason = match state_rx.borrow().clone() {
            SupervisorState::Failed { reason } => Err(reason),
            _ => Ok(()),
        };
        (reason, forced)
    }
}

/// `true` for the states a stop wait may end on.
fn is_terminal(state: &SupervisorState) -> bool {
    matches!(
        state,
        SupervisorState::Stopped | SupervisorState::Failed { .. }
    )
}
