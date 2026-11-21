//! The script side of a debug session: transpile, instrument, run, and pause at hooks.
//!
//! One [`ScriptDebugger`] owns one execution. It compiles the script into its own [`ScriptEngine`]
//! (never the site's engine, so a debug compile cannot disturb production code paths) and runs it
//! on a thread of its own. The injected `host.__dap_*` functions are answered by [`DebugBridge`],
//! which blocks **that** thread while paused; every other request, worker and script carries on.
//!
//! The session waits in [`ScriptDebugger::start`] for `configurationDone` before the script's
//! first instruction, as DAP asks. A `disconnect` before then runs nothing at all.

use std::collections::BTreeSet;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde_json::{Map, Value};
use spot_core::SiteId;
use spot_ts::{
    Compiled, EngineConfig, HostBridge, HostError, HostFn, HostSurface, Limits, ScriptEngine,
    ScriptError, ScriptId, Shape,
};
use spot_ts_transpile::Options;

use super::instrument::{HOOK_ENTER, HOOK_EXIT, HOOK_LINE, instrument};
use super::sourcemap::{OriginalPosition, Remap};
use crate::error::{DevToolsError, Result};
use crate::safety::DebugPolicy;

/// The closed host surface of the dev site: the functions a script may call, and the answer for
/// whether the site is production.
///
/// The dev server implements this once per site; the debugger calls it for `get_doc` and friends
/// exactly as the site's own bridge would, so a breakpoint sees real document handles.
pub trait DebugHost: Send + Sync {
    /// The site being debugged.
    fn site(&self) -> SiteId;

    /// Whether the site is marked production (the debugger refuses to start there).
    fn production(&self) -> bool;

    /// The host functions the site's scripts may call.
    fn surface(&self) -> HostSurface;

    /// The actor every host call of a debug session runs as: the user the development token
    /// stands for. Asked once, after the token has been verified, and never taken from the
    /// client's request, so a debug session cannot swap its authority.
    ///
    /// # Errors
    ///
    /// A [`HostError`] when the host has no actor for the session; the launch is then refused
    /// (fail closed), not run with the host's own authority.
    fn session_actor(&self) -> std::result::Result<SessionActor, HostError>;

    /// Calls one host function with one JSON argument, **as `actor`**. The host must run the
    /// call through its permission checks for that actor, exactly as it would for the same user's
    /// own request.
    ///
    /// # Errors
    ///
    /// Any [`HostError`] the site's bridge reports (a permission denial included); the script
    /// sees it as a thrown `HostError`.
    fn call(
        &self,
        actor: &SessionActor,
        name: &str,
        args: Value,
    ) -> std::result::Result<Value, HostError>;

    /// The invocation time a script sees, in milliseconds since the Unix epoch. A dev site keeps
    /// it fixed for a debug session.
    fn now_millis(&self) -> i64 {
        1_790_000_000_000
    }

    /// The deterministic random seed of a debug session.
    fn random_seed(&self) -> u64 {
        1
    }
}

/// The user a debug session acts as: an opaque identifier the host chose, never a value from the
/// debugger client's request. It carries no credential, so printing it is safe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionActor(String);

impl SessionActor {
    /// An actor with the host's identifier for the user.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The host's identifier for the user.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.0
    }
}

/// What a `launch` asks to debug: the TypeScript source, where it came from, which entry to call
/// and what to pass it.
#[derive(Clone, Debug, PartialEq)]
pub struct LaunchRequest {
    /// The TypeScript as the author wrote it.
    pub source: String,
    /// The script's path, as breakpoints and stack frames name it.
    pub source_path: String,
    /// The exported function to call.
    pub entry: String,
    /// The argument to the entry, as JSON.
    pub args: Value,
    /// Breakpoints to set before the first instruction, in source lines (1-based).
    pub breakpoints: Vec<u32>,
    /// Stop at the script's first statement instead of running to the first breakpoint.
    pub stop_on_entry: bool,
}

impl LaunchRequest {
    /// A request to call `entry` of `source` (named `source_path`), with no initial breakpoints.
    #[must_use]
    pub fn new(
        source: impl Into<String>,
        source_path: impl Into<String>,
        entry: impl Into<String>,
    ) -> Self {
        Self {
            source: source.into(),
            source_path: source_path.into(),
            entry: entry.into(),
            args: Value::Null,
            breakpoints: Vec::new(),
            stop_on_entry: false,
        }
    }
}

/// Why the debugger stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StopReason {
    /// A breakpoint was reached.
    Breakpoint,
    /// A step ended.
    Step,
    /// The client asked to pause.
    Pause,
    /// `stopOnEntry` was set: the first statement.
    Entry,
}

impl StopReason {
    /// The DAP `reason` text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Breakpoint => "breakpoint",
            Self::Step => "step",
            Self::Pause => "pause",
            Self::Entry => "entry",
        }
    }
}

/// One frame of a paused script, newest first in a [`PausedSnapshot`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    /// The frame id, used by `scopes` and `stackTrace`.
    pub id: u32,
    /// The function name (`script` for module top level).
    pub name: String,
    /// The source path breakpoints name.
    pub source: String,
    /// The source line (the TypeScript line when a map is available).
    pub line: u32,
    /// The source column.
    pub column: u32,
    /// The frame's locals, as the hook reported them.
    pub locals: Value,
}

/// A paused script: the stack, the locals and the generation of the pause.
#[derive(Clone, Debug, PartialEq)]
pub struct PausedSnapshot {
    /// Why it stopped.
    pub reason: StopReason,
    /// The stack, innermost frame first.
    pub frames: Vec<Frame>,
    /// A new number for every pause, so a client's variable handles cannot outlive it.
    pub generation: u64,
}

/// A breakpoint the adapter accepted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BreakpointResult {
    /// The source line the client asked for.
    pub line: u32,
    /// Whether at least one generated line can host it.
    pub verified: bool,
    /// The generated lines it was set on.
    pub generated: Vec<u32>,
}

/// A debugger command, the DAP resume family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DebugCommand {
    /// Run to the next breakpoint.
    Continue,
    /// Run to the next line in this frame or above.
    StepOver,
    /// Run to the next line anywhere (into a called function).
    StepIn,
    /// Run to the next line in a caller.
    StepOut,
    /// Stop at the next hook.
    Pause,
    /// End the session and resume the script with an error it cannot keep running from.
    Disconnect,
}

/// What the session does now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionPhase {
    /// Launched, waiting for `configurationDone`.
    Prepared,
    /// The script is executing.
    Running,
    /// The script's thread is blocked in a hook.
    Paused,
    /// The script ended (or never started).
    Terminated,
}

/// An event the session sends to its debugger client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DebugEvent {
    /// The script stopped at `reason`.
    Stopped {
        /// Why.
        reason: StopReason,
    },
    /// The script wrote something worth showing (its result, a diagnostic).
    Output {
        /// The text.
        text: String,
    },
    /// The debugger had to act on its own; the client should show this.
    Warning {
        /// The text.
        text: String,
    },
    /// The script ended.
    Terminated {
        /// 0 when the entry returned, 1 when it threw or a limit stopped it.
        exit_code: i64,
        /// The result JSON, or the error.
        output: String,
    },
}

/// Receives a session's events. The DAP server implements this by writing framed events; a test
/// implements it with a channel.
pub trait EventSink: Send + Sync {
    /// Emits one event. Called from the script's thread for `Stopped`/`Terminated`, so it must
    /// not block on the paused script.
    fn emit(&self, event: DebugEvent);
}

/// A sink that discards events, for a test that reads the session directly.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoopSink;

impl EventSink for NoopSink {
    fn emit(&self, _event: DebugEvent) {}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Continue,
    Over(usize),
    In(usize),
    Out(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Prepared,
    Running,
    Paused,
    Terminated,
}

struct RawFrame {
    name: String,
    line: u32,
    locals: Value,
}

struct Control {
    phase: Phase,
    configuration_done: bool,
    breakpoints: BTreeSet<u32>,
    step: Step,
    pause_requested: bool,
    stop_on_entry: bool,
    seen_line: bool,
    killed: bool,
    frames: Vec<RawFrame>,
    generation: u64,
    stopped: StopReason,
}

struct RuntimeState {
    sink: Arc<dyn EventSink>,
    remap: Remap,
    source_path: String,
    pause_timeout: Duration,
    control: Mutex<Control>,
    wake: Condvar,
}

impl RuntimeState {
    fn lock(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn on_enter(&self, name: String, line: u32) -> std::result::Result<(), HostError> {
        let mut control = self.lock();
        if control.killed {
            return Err(disconnected());
        }
        control.frames.push(RawFrame {
            name,
            line,
            locals: Value::Object(Map::new()),
        });
        Ok(())
    }

    fn on_exit(&self) -> std::result::Result<(), HostError> {
        let mut control = self.lock();
        if control.killed {
            return Err(disconnected());
        }
        if control.frames.len() > 1 {
            control.frames.pop();
        }
        Ok(())
    }

    fn on_line(&self, line: u32, locals: Value) -> std::result::Result<(), HostError> {
        let mut control = self.lock();
        if control.killed {
            return Err(disconnected());
        }
        if matches!(control.phase, Phase::Terminated) {
            return Ok(());
        }
        if let Some(frame) = control.frames.last_mut() {
            frame.line = line;
            frame.locals = locals;
        }
        let depth = control.frames.len();
        let reason = if !control.seen_line {
            control.seen_line = true;
            control.stop_on_entry.then_some(StopReason::Entry)
        } else {
            None
        }
        .or_else(|| {
            control
                .breakpoints
                .contains(&line)
                .then_some(StopReason::Breakpoint)
        })
        .or_else(|| match control.step {
            Step::Over(from) if depth <= from => Some(StopReason::Step),
            Step::In(_) => Some(StopReason::Step),
            Step::Out(from) if depth < from => Some(StopReason::Step),
            _ => None,
        })
        .or_else(|| control.pause_requested.then_some(StopReason::Pause));
        let Some(reason) = reason else {
            return Ok(());
        };
        control.step = Step::Continue;
        control.pause_requested = false;
        control.phase = Phase::Paused;
        control.stopped = reason;
        control.generation = control.generation.wrapping_add(1);
        drop(control);

        self.sink.emit(DebugEvent::Stopped { reason });
        self.wait_for_command();
        Ok(())
    }

    /// Blocks until a command resumes this pause, or the pause timeout does. On timeout the script
    /// is resumed with a warning: a forgotten breakpoint must not hold a worker.
    fn wait_for_command(&self) {
        let mut control = self.lock();
        let mut timed_out = false;
        while matches!(control.phase, Phase::Paused) {
            let (guard, outcome) = match self.wake.wait_timeout(control, self.pause_timeout) {
                Ok(pair) => pair,
                Err(poisoned) => poisoned.into_inner(),
            };
            control = guard;
            if outcome.timed_out() && matches!(control.phase, Phase::Paused) {
                timed_out = true;
                break;
            }
        }
        if timed_out {
            control.phase = Phase::Running;
            control.step = Step::Continue;
            control.pause_requested = false;
            drop(control);
            self.sink.emit(DebugEvent::Warning {
                text: format!(
                    "the script was resumed after {} ms without a debugger command",
                    self.pause_timeout.as_millis()
                ),
            });
        }
    }

    fn snapshot_of(&self, control: &Control, reason: StopReason) -> PausedSnapshot {
        let mut frames = Vec::with_capacity(control.frames.len());
        for (index, frame) in control.frames.iter().rev().enumerate() {
            let original: Option<OriginalPosition> = self.remap.original(frame.line);
            frames.push(Frame {
                id: u32::try_from(index).unwrap_or(u32::MAX),
                name: frame.name.clone(),
                source: original.as_ref().map_or_else(
                    || self.source_path.clone(),
                    |position| position.source.clone(),
                ),
                line: original
                    .as_ref()
                    .map_or(frame.line, |position| position.line),
                column: original.as_ref().map_or(1, |position| position.column),
                locals: frame.locals.clone(),
            });
        }
        PausedSnapshot {
            reason,
            frames,
            generation: control.generation,
        }
    }

    fn snapshot(&self) -> Option<PausedSnapshot> {
        let control = self.lock();
        if !matches!(control.phase, Phase::Paused) {
            return None;
        }
        Some(self.snapshot_of(&control, control.stopped))
    }

    fn phase(&self) -> SessionPhase {
        match self.lock().phase {
            Phase::Prepared => SessionPhase::Prepared,
            Phase::Running => SessionPhase::Running,
            Phase::Paused => SessionPhase::Paused,
            Phase::Terminated => SessionPhase::Terminated,
        }
    }

    fn configuration_done(&self) {
        let mut control = self.lock();
        control.configuration_done = true;
        self.wake.notify_all();
    }

    fn wait_for_start(&self) -> bool {
        let mut control = self.lock();
        while !control.configuration_done && !control.killed {
            control = match self.wake.wait(control) {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
        }
        !control.killed
    }

    fn command(&self, command: DebugCommand) {
        let mut control = self.lock();
        match command {
            DebugCommand::Pause => {
                if matches!(control.phase, Phase::Running | Phase::Prepared) {
                    control.pause_requested = true;
                }
                return;
            }
            DebugCommand::Disconnect => {
                control.killed = true;
                control.phase = Phase::Running;
                control.step = Step::Continue;
                self.wake.notify_all();
                return;
            }
            DebugCommand::Continue => {
                control.step = Step::Continue;
                control.pause_requested = false;
            }
            DebugCommand::StepOver => {
                control.step = Step::Over(control.frames.len());
                control.pause_requested = false;
            }
            DebugCommand::StepIn => {
                control.step = Step::In(control.frames.len());
                control.pause_requested = false;
            }
            DebugCommand::StepOut => {
                control.step = Step::Out(control.frames.len());
                control.pause_requested = false;
            }
        }
        if matches!(control.phase, Phase::Paused) {
            control.phase = Phase::Running;
            control.generation = control.generation.wrapping_add(1);
        }
        control.configuration_done = true;
        self.wake.notify_all();
    }

    fn set_breakpoints(&self, mapped: BTreeSet<u32>) -> usize {
        let mut control = self.lock();
        control.breakpoints = mapped;
        control.breakpoints.len()
    }

    fn finish(&self, outcome: std::result::Result<Value, ScriptError>) {
        {
            let mut control = self.lock();
            control.phase = Phase::Terminated;
            control.frames.clear();
            self.wake.notify_all();
        }
        let (exit_code, output) = match outcome {
            Ok(value) => (0, value.to_string()),
            Err(error) => (1, error.to_string()),
        };
        self.sink.emit(DebugEvent::Terminated { exit_code, output });
    }
}

fn disconnected() -> HostError {
    HostError::new("debugger", "the debugger disconnected the session")
}

/// The bridge the debug script runs against: the site's functions plus the three hooks.
struct DebugBridge {
    host: Arc<dyn DebugHost>,
    actor: SessionActor,
    state: Arc<RuntimeState>,
}

impl HostBridge for DebugBridge {
    fn call(&self, name: &str, args: Value) -> std::result::Result<Value, HostError> {
        match name {
            HOOK_LINE => {
                let line = args
                    .get("line")
                    .and_then(Value::as_u64)
                    .and_then(|line| u32::try_from(line).ok())
                    .unwrap_or(0);
                let locals = args.get("locals").cloned().unwrap_or(Value::Null);
                self.state.on_line(line, locals)?;
                Ok(Value::Null)
            }
            HOOK_ENTER => {
                let name = args
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("anonymous")
                    .to_owned();
                let line = args
                    .get("line")
                    .and_then(Value::as_u64)
                    .and_then(|line| u32::try_from(line).ok())
                    .unwrap_or(0);
                self.state.on_enter(name, line)?;
                Ok(Value::Null)
            }
            HOOK_EXIT => {
                self.state.on_exit()?;
                Ok(Value::Null)
            }
            other => self.host.call(&self.actor, other, args),
        }
    }

    fn now_millis(&self) -> i64 {
        self.host.now_millis()
    }

    fn random_seed(&self) -> u64 {
        self.host.random_seed()
    }
}

/// One live debug session.
pub struct ScriptDebugger {
    state: Arc<RuntimeState>,
    source_path: String,
    remap: Remap,
}

impl ScriptDebugger {
    /// Validates the policy, transpiles and instruments `launch`, and starts the execution thread
    /// (which waits for [`ScriptDebugger::configuration_done`]).
    ///
    /// # Errors
    ///
    /// [`DevToolsError::Production`], [`DevToolsError::ReleaseBuild`],
    /// [`DevToolsError::Script`] (transpile, compile or thread start) or
    /// [`DevToolsError::Instrument`].
    pub fn start(
        host: Arc<dyn DebugHost>,
        policy: &DebugPolicy,
        launch: LaunchRequest,
        sink: Arc<dyn EventSink>,
    ) -> Result<Self> {
        policy.check_build()?;
        policy.check_site(&host.site(), host.production())?;
        // Fail closed: no actor, no session. Every host call below runs as this actor.
        let actor = host.session_actor().map_err(|_| DevToolsError::Session {
            message: "the host named no actor for this debug session".into(),
        })?;
        let transpiled =
            spot_ts_transpile::transpile(&launch.source, &launch.source_path, &Options::default())
                .map_err(|diagnostics| {
                    let detail = diagnostics.first().map_or_else(
                        || "no diagnostics".to_owned(),
                        |d| format!("{}: {}", d.code, d.message),
                    );
                    DevToolsError::Script {
                        message: format!("{} does not transpile ({detail})", launch.source_path),
                    }
                })?;
        let remap = match Remap::parse(&transpiled.source_map) {
            Ok(remap) => remap,
            Err(_) => Remap::identity(&launch.source_path),
        };
        let instrumented = instrument(&transpiled.js);

        let surface = hook_surface(host.surface());
        let engine = ScriptEngine::new(EngineConfig {
            surface,
            keep_bytecode: false,
            ..EngineConfig::default()
        });
        let compiled: Arc<Compiled> = engine
            .compile(
                &instrumented.source,
                ScriptId::new("devtools", &launch.source_path, 1),
                None,
            )
            .map_err(|error| DevToolsError::Script {
                message: format!("the instrumented script does not compile: {error}"),
            })?;

        let state = Arc::new(RuntimeState {
            sink,
            remap: remap.clone(),
            source_path: launch.source_path.clone(),
            pause_timeout: policy.pause_timeout,
            control: Mutex::new(Control {
                phase: Phase::Prepared,
                configuration_done: false,
                breakpoints: BTreeSet::new(),
                step: Step::Continue,
                pause_requested: false,
                stop_on_entry: launch.stop_on_entry,
                seen_line: false,
                killed: false,
                frames: vec![RawFrame {
                    name: "script".to_owned(),
                    line: 1,
                    locals: Value::Object(Map::new()),
                }],
                generation: 0,
                stopped: StopReason::Breakpoint,
            }),
            wake: Condvar::new(),
        });
        let debugger = Self {
            state: Arc::clone(&state),
            source_path: launch.source_path.clone(),
            remap: remap.clone(),
        };
        debugger.apply_breakpoints(Some(&launch.source_path), &launch.breakpoints);
        let bridge = Arc::new(DebugBridge {
            host,
            actor,
            state: Arc::clone(&state),
        });
        let entry = launch.entry.clone();
        let args = launch.args.clone();
        let limits = debug_limits();
        std::thread::Builder::new()
            .name("spot-devtools-script".to_owned())
            .spawn(move || {
                if !state.wait_for_start() {
                    state.finish(Ok(Value::Null));
                    return;
                }
                let outcome = engine.run_blocking(&compiled, &entry, &args, bridge, limits);
                state.finish(outcome);
            })
            .map_err(|error| DevToolsError::Script {
                message: format!("cannot start the script's debug thread: {error}"),
            })?;
        Ok(debugger)
    }

    /// Sets breakpoints on `source_path` (matched by file name) at `lines`. Unmapped lines are
    /// returned as unverified.
    pub fn set_breakpoints(
        &self,
        source_path: Option<&str>,
        lines: &[u32],
    ) -> Vec<BreakpointResult> {
        self.apply_breakpoints(source_path, lines)
    }

    fn apply_breakpoints(&self, source_path: Option<&str>, lines: &[u32]) -> Vec<BreakpointResult> {
        let mut mapped = BTreeSet::new();
        let mut results = Vec::with_capacity(lines.len());
        for line in lines {
            let generated = self.remap.generated_lines(source_path, *line);
            mapped.extend(generated.iter().copied());
            results.push(BreakpointResult {
                line: *line,
                verified: !generated.is_empty(),
                generated,
            });
        }
        self.state.set_breakpoints(mapped);
        results
    }

    /// Lets the prepared script start (the DAP `configurationDone` request).
    pub fn configuration_done(&self) {
        self.state.configuration_done();
    }

    /// Applies a resume-family command.
    ///
    /// # Errors
    ///
    /// [`DevToolsError::Session`] when the session is over.
    pub fn command(&self, command: DebugCommand) -> Result<()> {
        if matches!(self.state.phase(), SessionPhase::Terminated) {
            return Err(DevToolsError::Session {
                message: "the script has already ended".into(),
            });
        }
        self.state.command(command);
        Ok(())
    }

    /// The paused snapshot, if the script is paused now.
    #[must_use]
    pub fn snapshot(&self) -> Option<PausedSnapshot> {
        self.state.snapshot()
    }

    /// What the session is doing.
    #[must_use]
    pub fn phase(&self) -> SessionPhase {
        self.state.phase()
    }

    /// The source path frames and breakpoints use.
    #[must_use]
    pub fn source_path(&self) -> &str {
        &self.source_path
    }

    /// The source map in use.
    #[must_use]
    pub fn remap(&self) -> &Remap {
        &self.remap
    }
}

fn hook_surface(mut surface: HostSurface) -> HostSurface {
    for name in [HOOK_LINE, HOOK_ENTER, HOOK_EXIT] {
        if !surface
            .functions
            .iter()
            .any(|function| function.name == name)
        {
            surface.functions.push(HostFn::new(name, Shape::Any));
        }
    }
    surface
}

/// The limits a debug run uses: generous wall time and instruction budget (a paused script must
/// survive its pause), but the same memory and output caps as a normal run.
fn debug_limits() -> Limits {
    Limits {
        memory_bytes: 64 * 1024 * 1024,
        stack_bytes: 512 * 1024,
        wall_clock: Duration::from_secs(24 * 60 * 60),
        instructions: 1_000_000_000_000,
        host_calls: u32::MAX,
        output_bytes: 4 * 1024 * 1024,
    }
}
