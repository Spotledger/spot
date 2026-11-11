//! The DAP server: one connection, the request loop and the event writer.
//!
//! The server is deliberately small: [`DebugServer::serve_pair`] is the whole loop, generic over
//! any buffered reader and writer, so stdio, a TCP stream and a test's in-memory pipes take the
//! same path. Events come from the script's thread while the loop is reading the next request;
//! the shared writer mutex keeps both framed messages whole.
//!
//! Transport policy is checked before anything binds: [`DebugServerConfig::address`] must be
//! loopback unless [`crate::safety::DebugPolicy::allow_remote`] is set, the build must be a debug
//! build, and every `launch`/`attach` must present the site's development token. The script's own
//! safety (production refusal) is checked again in [`ScriptDebugger::start`].

use std::io::{BufRead, Write};
use std::net::SocketAddr;
#[cfg(feature = "dev-server")]
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::protocol::{
    Outgoing, Request, capabilities, parse_request, read_message, write_message,
};
use super::runtime::{
    DebugCommand, DebugEvent, DebugHost, EventSink, LaunchRequest, ScriptDebugger, SessionPhase,
};
use super::variables::VariableStore;
use super::wire::WireRedactor;
use crate::error::{DevToolsError, Result};
use crate::safety::{DEFAULT_DEBUG_ADDRESS, DebugPolicy, read_program};
use spot_redact::{RedactionPolicy, Redactor};

/// How a debug server is configured.
#[derive(Clone)]
pub struct DebugServerConfig {
    /// The safety policy: token, loopback rule, pause timeout.
    pub policy: DebugPolicy,
    /// The TCP address to listen on. Unused for stdio.
    pub address: SocketAddr,
    /// The directory a launch's `program` is resolved against. There is no default: without it a
    /// `program` launch is refused (an inline `source` still runs).
    pub program_root: Option<PathBuf>,
    /// The redactor in front of everything the client is sent. `None` withholds the text.
    pub redactor: Option<Arc<Redactor>>,
}

impl std::fmt::Debug for DebugServerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebugServerConfig")
            .field("policy", &self.policy)
            .field("address", &self.address)
            .field("program_root", &self.program_root)
            .field("redactor", &self.redactor.is_some())
            .finish()
    }
}

impl DebugServerConfig {
    /// A configuration on the default loopback address, with no program root and the default
    /// redaction policy.
    #[must_use]
    pub fn new(policy: DebugPolicy) -> Self {
        let redactor = Redactor::from_policy(&RedactionPolicy::default())
            .ok()
            .map(Arc::new);
        Self {
            policy,
            address: DEFAULT_DEBUG_ADDRESS,
            program_root: None,
            redactor,
        }
    }

    /// The same configuration resolving `program` against `root`.
    #[must_use]
    pub fn with_program_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.program_root = Some(root.into());
        self
    }

    /// The same configuration with the site's own redactor (its sealed values registered).
    #[must_use]
    pub fn with_redactor(mut self, redactor: Arc<Redactor>) -> Self {
        self.redactor = Some(redactor);
        self
    }

    /// The same configuration listening on another address.
    #[must_use]
    pub fn with_address(mut self, address: SocketAddr) -> Self {
        self.address = address;
        self
    }
}

/// The Debug Adapter Protocol server for one dev site.
#[derive(Clone)]
pub struct DebugServer {
    host: Arc<dyn DebugHost>,
    config: DebugServerConfig,
}

impl DebugServer {
    /// A server debugging `host` under `config`.
    #[must_use]
    pub fn new(host: Arc<dyn DebugHost>, config: DebugServerConfig) -> Self {
        Self { host, config }
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &DebugServerConfig {
        &self.config
    }

    /// Checks the build and address and binds the TCP listener.
    ///
    /// # Errors
    ///
    /// [`DevToolsError::ReleaseBuild`], [`DevToolsError::NotLoopback`] or
    /// [`DevToolsError::Bind`].
    #[cfg(feature = "dev-server")]
    pub fn bind(&self) -> Result<TcpListener> {
        self.config.policy.check_build()?;
        self.config.policy.check_address(self.config.address)?;
        TcpListener::bind(self.config.address).map_err(|error| DevToolsError::Bind {
            address: self.config.address.to_string(),
            message: error.to_string(),
        })
    }

    /// Serves stdio until the client disconnects (or closes the stream).
    ///
    /// # Errors
    ///
    /// Any protocol or I/O error; a clean disconnect is `Ok(())`.
    #[cfg(feature = "dev-server")]
    pub fn serve_stdio(self) -> Result<()> {
        let reader = std::io::BufReader::new(std::io::stdin());
        self.serve_pair(reader, std::io::stdout())
    }

    /// Binds and serves TCP until the listener fails. Each connection is served on its own thread;
    /// the token still guards every launch.
    ///
    /// # Errors
    ///
    /// See [`DebugServer::bind`]; a failed connection is logged by being dropped, not by stopping
    /// the listener.
    #[cfg(feature = "dev-server")]
    pub fn serve_tcp(self) -> Result<()> {
        let listener = self.bind()?;
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    let server = self.clone();
                    let _ = std::thread::Builder::new()
                        .name("spot-devtools-dap".to_owned())
                        .spawn(move || {
                            let _ = server.serve_connection(stream);
                        });
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    return Err(DevToolsError::Bind {
                        address: self.config.address.to_string(),
                        message: error.to_string(),
                    });
                }
            }
        }
    }

    /// Accepts exactly one connection and serves it (the testable half of [`DebugServer::serve_tcp`]).
    ///
    /// # Errors
    ///
    /// [`DevToolsError::Bind`] when accepting fails, or any connection error.
    #[cfg(feature = "dev-server")]
    pub fn serve_one(&self, listener: TcpListener) -> Result<()> {
        let (stream, _) = listener.accept().map_err(|error| DevToolsError::Bind {
            address: self.config.address.to_string(),
            message: error.to_string(),
        })?;
        self.serve_connection(stream)
    }

    /// Serves one accepted TCP connection.
    ///
    /// # Errors
    ///
    /// [`DevToolsError::Io`] when the stream cannot be cloned or served.
    #[cfg(feature = "dev-server")]
    pub fn serve_connection(&self, stream: TcpStream) -> Result<()> {
        let reader =
            std::io::BufReader::new(stream.try_clone().map_err(|error| DevToolsError::Io {
                path: "<dap tcp>".into(),
                message: error.to_string(),
            })?);
        self.serve_pair(reader, stream)
    }

    /// The request loop over any reader/writer pair.
    ///
    /// # Errors
    ///
    /// A framing error, an I/O error, or an error writing a response.
    pub fn serve_pair<R: BufRead, W: Write + Send + 'static>(
        &self,
        mut reader: R,
        writer: W,
    ) -> Result<()> {
        let writer = Arc::new(Mutex::new(writer));
        let sequence = Arc::new(AtomicU64::new(1));
        let mut session = Session::default();
        loop {
            let body = match read_message(&mut reader)? {
                Some(body) => body,
                None => return Ok(()),
            };
            let request = match parse_request(&body) {
                Ok(request) => request,
                Err(error) => {
                    let message = Outgoing::failed(0, "", &error);
                    send(&writer, &sequence, &message)?;
                    continue;
                }
            };
            match self.dispatch(&request, &mut session, &writer, &sequence) {
                Ok(message) => send(&writer, &sequence, &message)?,
                Err(error) => {
                    let message = Outgoing::failed(request.seq, &request.command, &error);
                    send(&writer, &sequence, &message)?;
                }
            }
            if session.close {
                return Ok(());
            }
        }
    }

    fn dispatch<W: Write + Send + 'static>(
        &self,
        request: &Request,
        session: &mut Session,
        writer: &Arc<Mutex<W>>,
        sequence: &Arc<AtomicU64>,
    ) -> Result<Outgoing> {
        self.handle(request, session, writer, sequence)
    }

    fn handle<W: Write + Send + 'static>(
        &self,
        request: &Request,
        session: &mut Session,
        writer: &Arc<Mutex<W>>,
        sequence: &Arc<AtomicU64>,
    ) -> Result<Outgoing> {
        let seq = request.seq;
        let command = request.command.as_str();
        match command {
            "initialize" => Ok(Outgoing::ok(seq, command, capabilities())),
            "launch" | "attach" => {
                // The token comes first: nothing in the request (not even whether a `program`
                // file exists) is acted on for a caller that has not proved it holds the
                // development token, and a connection that keeps guessing is closed.
                let presented = request
                    .arguments
                    .get("devToken")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if let Err(error) = self.config.policy.check_token(presented) {
                    session.failed_tokens = session.failed_tokens.saturating_add(1);
                    if session.failed_tokens >= MAX_TOKEN_ATTEMPTS {
                        session.close = true;
                    }
                    return Err(error);
                }
                let launch = parse_launch(
                    &request.arguments,
                    command,
                    self.config.program_root.as_deref(),
                )?;
                self.config.policy.check_token(&launch.token)?;
                let mut launch_request = LaunchRequest::new(
                    launch.source_text.clone(),
                    launch.source_path.clone(),
                    launch.entry,
                );
                launch_request.args = launch.args;
                launch_request.stop_on_entry = launch.stop_on_entry;
                launch_request.breakpoints = launch.breakpoints.clone();
                let sink: Arc<dyn EventSink> = Arc::new(DapSink {
                    writer: Arc::clone(writer),
                    sequence: Arc::clone(sequence),
                    redact: WireRedactor::new(self.config.redactor.clone()),
                });
                let debugger = ScriptDebugger::start(
                    Arc::clone(&self.host),
                    &self.config.policy,
                    launch_request,
                    sink,
                )?;
                if !session.pending_lines.is_empty() {
                    debugger
                        .set_breakpoints(session.pending_source.as_deref(), &session.pending_lines);
                    session.pending_lines.clear();
                }
                session.source = Some(launch.source_text);
                session.debugger = Some(debugger);
                session.variables = None;
                Ok(Outgoing::ok_empty(seq, command))
            }
            "setBreakpoints" => {
                let lines = breakpoint_lines(&request.arguments);
                let path = request
                    .arguments
                    .get("source")
                    .and_then(|source| source.get("path"))
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let results = match &session.debugger {
                    Some(debugger) => debugger.set_breakpoints(path.as_deref(), &lines),
                    None => {
                        session.pending_lines = lines.clone();
                        session.pending_source = path.clone();
                        lines
                            .iter()
                            .map(|line| super::runtime::BreakpointResult {
                                line: *line,
                                verified: true,
                                generated: Vec::new(),
                            })
                            .collect()
                    }
                };
                let body = json!({
                    "breakpoints": results
                        .iter()
                        .map(|result| json!({
                            "verified": result.verified,
                            "line": result.line,
                            "source": {"path": path},
                        }))
                        .collect::<Vec<_>>(),
                });
                Ok(Outgoing::ok(seq, command, body))
            }
            "setFunctionBreakpoints" | "setExceptionBreakpoints" => {
                Ok(Outgoing::ok_empty(seq, command))
            }
            "configurationDone" => {
                if let Some(debugger) = &session.debugger {
                    debugger.configuration_done();
                }
                Ok(Outgoing::ok_empty(seq, command))
            }
            "continue" | "next" | "stepIn" | "stepOut" | "pause" => {
                let debugger = session.debugger()?;
                let debug_command = match command {
                    "continue" => DebugCommand::Continue,
                    "next" => DebugCommand::StepOver,
                    "stepIn" => DebugCommand::StepIn,
                    "stepOut" => DebugCommand::StepOut,
                    _ => DebugCommand::Pause,
                };
                debugger.command(debug_command)?;
                Ok(Outgoing::ok_empty(seq, command))
            }
            "threads" => Ok(Outgoing::ok(
                seq,
                command,
                json!({"threads": [{"id": 1, "name": "script"}]}),
            )),
            "stackTrace" => {
                let snapshot = session.debugger()?.snapshot().ok_or_else(not_paused)?;
                let frames: Vec<Value> = snapshot
                    .frames
                    .iter()
                    .map(|frame| {
                        json!({
                            "id": frame.id,
                            "name": frame.name,
                            "source": {"name": frame.source, "path": frame.source},
                            "line": frame.line,
                            "column": frame.column,
                        })
                    })
                    .collect();
                Ok(Outgoing::ok(
                    seq,
                    command,
                    json!({"stackFrames": frames, "totalFrames": snapshot.frames.len()}),
                ))
            }
            "scopes" => {
                let frame_id = u32_arg(&request.arguments, "frameId")?;
                let snapshot = session.debugger()?.snapshot().ok_or_else(not_paused)?;
                if !snapshot.frames.iter().any(|frame| frame.id == frame_id) {
                    return Err(DevToolsError::Session {
                        message: format!("there is no frame {frame_id}"),
                    });
                }
                let reference = VariableStore::frame_reference(frame_id);
                Ok(Outgoing::ok(
                    seq,
                    command,
                    json!({
                        "scopes": [{
                            "name": "Locals",
                            "presentationHint": "locals",
                            "variablesReference": reference,
                            "expensive": false,
                        }],
                    }),
                ))
            }
            "variables" => {
                let reference = u64_arg(&request.arguments, "variablesReference")?;
                let store = session.store()?;
                let variables =
                    store
                        .variables(reference)
                        .ok_or_else(|| DevToolsError::Session {
                            message: "the variable reference belongs to an earlier pause".into(),
                        })?;
                let redact = WireRedactor::new(self.config.redactor.clone());
                let list: Vec<Value> = variables
                    .iter()
                    .map(|variable| {
                        let mut variable = variable.clone();
                        variable.value = redact.named(&variable.name, &variable.value);
                        variable.to_json()
                    })
                    .collect();
                Ok(Outgoing::ok(seq, command, json!({"variables": list})))
            }
            "evaluate" => {
                let frame_id = u32_arg(&request.arguments, "frameId")?;
                let expression = request
                    .arguments
                    .get("expression")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let variable =
                    session
                        .store()?
                        .evaluate(frame_id, expression)
                        .ok_or_else(|| {
                            DevToolsError::Session {
                        message:
                            "evaluate is read-only: pass a dotted path such as `customer.credit`"
                                .into(),
                    }
                        })?;
                let value = WireRedactor::new(self.config.redactor.clone())
                    .named(&variable.name, &variable.value);
                Ok(Outgoing::ok(
                    seq,
                    command,
                    json!({
                        "result": value,
                        "type": variable.type_name,
                        "variablesReference": variable.variables_reference,
                    }),
                ))
            }
            "source" => {
                let body = session
                    .source
                    .as_ref()
                    .map(|source| json!({"content": source}))
                    .unwrap_or_else(|| json!({"content": ""}));
                Ok(Outgoing::ok(seq, command, body))
            }
            "disconnect" | "terminate" => {
                if let Some(debugger) = &session.debugger {
                    let _ = debugger.command(DebugCommand::Disconnect);
                }
                session.close = true;
                Ok(Outgoing::ok_empty(seq, command))
            }
            other => Err(DevToolsError::Protocol {
                message: format!("unsupported command `{other}`"),
            }),
        }
    }
}

fn not_paused() -> DevToolsError {
    DevToolsError::Session {
        message: "the script is not paused".into(),
    }
}

fn u64_arg(arguments: &Value, name: &str) -> Result<u64> {
    arguments
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| DevToolsError::Protocol {
            message: format!("`{name}` is missing or not a number"),
        })
}

fn u32_arg(arguments: &Value, name: &str) -> Result<u32> {
    u32::try_from(u64_arg(arguments, name)?).map_err(|_| DevToolsError::Protocol {
        message: format!("`{name}` does not fit in 32 bits"),
    })
}

fn breakpoint_lines(arguments: &Value) -> Vec<u32> {
    arguments
        .get("breakpoints")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|breakpoint| breakpoint.get("line").and_then(Value::as_u64))
                .filter_map(|line| u32::try_from(line).ok())
                .collect()
        })
        .unwrap_or_default()
}

struct LaunchArgs {
    token: String,
    source_text: String,
    source_path: String,
    entry: String,
    args: Value,
    stop_on_entry: bool,
    breakpoints: Vec<u32>,
}

fn parse_launch(
    arguments: &Value,
    command: &str,
    program_root: Option<&Path>,
) -> Result<LaunchArgs> {
    let token = arguments
        .get("devToken")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let (source_text, source_path) = match (
        arguments.get("source").and_then(Value::as_str),
        arguments.get("program").and_then(Value::as_str),
    ) {
        (Some(source), _) => (
            source.to_owned(),
            arguments
                .get("sourcePath")
                .and_then(Value::as_str)
                .unwrap_or("scripts/default.ts")
                .to_owned(),
        ),
        (None, Some(program)) => {
            // Resolved against the configured root; the client only ever sees the relative name.
            let text = read_program(program_root, program)?;
            (text, program.to_owned())
        }
        (None, None) => {
            return Err(DevToolsError::Config {
                message: format!("{command} needs `program` or `source`"),
            });
        }
    };
    let entry = arguments
        .get("entry")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    if entry.is_empty() {
        return Err(DevToolsError::Config {
            message: "launch needs the exported `entry` function to call".into(),
        });
    }
    let breakpoints = arguments
        .get("breakpoints")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|breakpoint| match breakpoint {
                    Value::Number(number) => number.as_u64(),
                    Value::Object(object) => object.get("line").and_then(Value::as_u64),
                    _ => None,
                })
                .filter_map(|line| u32::try_from(line).ok())
                .collect()
        })
        .unwrap_or_default();
    let source = arguments
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or(&source_text)
        .to_owned();
    Ok(LaunchArgs {
        token,
        source_text: source,
        source_path,
        entry,
        args: arguments.get("args").cloned().unwrap_or(Value::Null),
        stop_on_entry: arguments
            .get("stopOnEntry")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        breakpoints,
    })
}

/// How many wrong tokens one connection may present before the server closes it.
const MAX_TOKEN_ATTEMPTS: u8 = 5;

#[derive(Default)]
struct Session {
    failed_tokens: u8,
    debugger: Option<ScriptDebugger>,
    variables: Option<VariableStore>,
    pending_lines: Vec<u32>,
    pending_source: Option<String>,
    source: Option<String>,
    close: bool,
}

impl Session {
    fn debugger(&self) -> Result<&ScriptDebugger> {
        self.debugger
            .as_ref()
            .ok_or_else(|| DevToolsError::Session {
                message: "no script has been launched".into(),
            })
    }

    fn store(&mut self) -> Result<&mut VariableStore> {
        let snapshot = self
            .debugger
            .as_ref()
            .ok_or_else(|| DevToolsError::Session {
                message: "no script has been launched".into(),
            })?
            .snapshot()
            .ok_or_else(not_paused)?;
        let stale = self
            .variables
            .as_ref()
            .is_none_or(|store| store.generation() != snapshot.generation);
        if stale {
            self.variables = Some(VariableStore::build(&snapshot));
        }
        self.variables
            .as_mut()
            .ok_or_else(|| DevToolsError::Session {
                message: "the variable store is unavailable".into(),
            })
    }
}

fn send<W: Write>(
    writer: &Arc<Mutex<W>>,
    sequence: &Arc<AtomicU64>,
    message: &Outgoing,
) -> Result<()> {
    let seq = sequence.fetch_add(1, Ordering::Relaxed);
    let json = message.to_json(seq);
    let mut writer = writer.lock().map_err(|_| DevToolsError::Io {
        path: "<dap stream>".into(),
        message: "the writer is poisoned".into(),
    })?;
    write_message(&mut *writer, &json)
}

/// Writes a session's events as DAP events.
struct DapSink<W: Write> {
    writer: Arc<Mutex<W>>,
    sequence: Arc<AtomicU64>,
    redact: WireRedactor,
}

impl<W: Write + Send + 'static> EventSink for DapSink<W> {
    fn emit(&self, event: DebugEvent) {
        let message = match event {
            DebugEvent::Stopped { reason } => Outgoing::event(
                "stopped",
                json!({
                    "reason": reason.as_str(),
                    "threadId": 1,
                    "allThreadsStopped": true,
                }),
            ),
            DebugEvent::Output { text } => Outgoing::event(
                "output",
                json!({"category": "stdout", "output": self.redact.output(&text)}),
            ),
            DebugEvent::Warning { text } => Outgoing::event(
                "output",
                json!({
                    "category": "console",
                    "output": format!("devtools: {}\n", self.redact.text(&text)),
                }),
            ),
            DebugEvent::Terminated { exit_code, output } => {
                let output = self.redact.output(&output);
                let _ = send(
                    &self.writer,
                    &self.sequence,
                    &Outgoing::event("output", json!({"category": "stdout", "output": output})),
                );
                Outgoing::event("terminated", json!({"exitCode": exit_code}))
            }
        };
        let _ = send(&self.writer, &self.sequence, &message);
    }
}

/// A one-line answer to "what is the session doing", for a status endpoint.
#[must_use]
pub fn phase_name(phase: SessionPhase) -> &'static str {
    match phase {
        SessionPhase::Prepared => "prepared",
        SessionPhase::Running => "running",
        SessionPhase::Paused => "paused",
        SessionPhase::Terminated => "terminated",
    }
}
