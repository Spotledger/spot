//! A DAP client driving the real adapter against a real TypeScript script on `spot-ts`.
#![allow(clippy::unwrap_used, clippy::expect_used)]
//!
//! The tests prove the acceptance sentence for scripts: a breakpoint set on a TypeScript line
//! reaches the right statement through the source map, a document handle returned by a host
//! function is inspectable as a typed object, `next`/`stepIn`/`stepOut` walk the frames, `evaluate`
//! is read-only, a production site and a wrong token are refused, a forgotten pause resumes after
//! the policy's timeout, and the runtime outside the paused script keeps working.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};
use spot_core::SiteId;
use spot_devtools::debug::protocol::{read_message, write_message};
use spot_devtools::{
    DebugHost, DebugPolicy, DebugServer, DebugServerConfig, DevToken, SessionActor,
};
use spot_ts::{HostError, HostFn, HostSurface, Shape};

// [ Test host ]

/// The dev site: one `get_doc` host function and a fixed time.
struct Site {
    production: bool,
    documents: Mutex<Vec<(String, String)>>,
    /// The user the dev token stands for.
    actor: &'static str,
    /// Which actor each host call ran as.
    callers: Mutex<Vec<String>>,
}

impl Site {
    fn dev() -> Arc<Self> {
        Arc::new(Self {
            production: false,
            documents: Mutex::new(Vec::new()),
            actor: "developer",
            callers: Mutex::new(Vec::new()),
        })
    }

    /// A dev site whose token stands for a user who lacks the permission to read documents.
    fn viewer() -> Arc<Self> {
        Arc::new(Self {
            production: false,
            documents: Mutex::new(Vec::new()),
            actor: "viewer",
            callers: Mutex::new(Vec::new()),
        })
    }

    fn production() -> Arc<Self> {
        Arc::new(Self {
            production: true,
            documents: Mutex::new(Vec::new()),
            actor: "developer",
            callers: Mutex::new(Vec::new()),
        })
    }
}

impl DebugHost for Site {
    fn site(&self) -> SiteId {
        SiteId::new("acme.example").expect("valid site id")
    }

    fn production(&self) -> bool {
        self.production
    }

    fn surface(&self) -> HostSurface {
        HostSurface::new(vec![HostFn::new(
            "get_doc",
            Shape::Object {
                required: vec![("doctype".into(), Shape::Text), ("id".into(), Shape::Text)],
                open: false,
            },
        )])
    }

    fn session_actor(&self) -> Result<SessionActor, HostError> {
        Ok(SessionActor::new(self.actor))
    }

    fn call(&self, actor: &SessionActor, name: &str, args: Value) -> Result<Value, HostError> {
        if let Ok(mut callers) = self.callers.lock() {
            callers.push(actor.id().to_owned());
        }
        match name {
            "get_doc" => {
                // The site's own permission check, for the actor the call runs as.
                if actor.id() != "developer" {
                    return Err(HostError::new("permission_denied", "no read permission"));
                }
                let doctype = args["doctype"].as_str().unwrap_or("").to_owned();
                let id = args["id"].as_str().unwrap_or("").to_owned();
                if let Ok(mut documents) = self.documents.lock() {
                    documents.push((doctype, id));
                }
                Ok(json!({
                    "name": "Acme",
                    "credit": 100,
                    "tags": ["vip"],
                    "password": "hunter2-hidden-field",
                    "memo": "Bearer spk_live_sealed_value_0123456789",
                }))
            }
            other => Err(HostError::new("unknown", other)),
        }
    }
}

// [ In-memory transport ]

struct ChannelReader {
    receiver: Receiver<Vec<u8>>,
    buffer: Vec<u8>,
    position: usize,
}

impl Read for ChannelReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.buffer.len() {
            match self.receiver.recv_timeout(Duration::from_secs(20)) {
                Ok(chunk) => {
                    self.buffer = chunk;
                    self.position = 0;
                }
                Err(_) => return Err(io::Error::other("the debug adapter did not answer")),
            }
        }
        let count = (self.buffer.len() - self.position).min(out.len());
        out[..count].copy_from_slice(&self.buffer[self.position..self.position + count]);
        self.position += count;
        Ok(count)
    }
}

struct ChannelWriter {
    sender: Sender<Vec<u8>>,
}

impl Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.sender
            .send(data.to_vec())
            .map(|()| data.len())
            .map_err(|_| io::Error::other("the debug adapter went away"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// [ DAP client ]

struct Client {
    reader: Box<dyn BufRead>,
    writer: Box<dyn Write>,
    sequence: u64,
    events: Vec<Value>,
    server: Option<JoinHandle<()>>,
}

impl Client {
    fn channels(server: DebugServer) -> Self {
        let (to_server, from_client) = channel();
        let (to_client, from_server) = channel();
        let handle = std::thread::spawn(move || {
            let reader = BufReader::new(ChannelReader {
                receiver: from_client,
                buffer: Vec::new(),
                position: 0,
            });
            let writer = ChannelWriter { sender: to_client };
            let _ = server.serve_pair(reader, writer);
        });
        Self {
            reader: Box::new(BufReader::new(ChannelReader {
                receiver: from_server,
                buffer: Vec::new(),
                position: 0,
            })),
            writer: Box::new(ChannelWriter { sender: to_server }),
            sequence: 0,
            events: Vec::new(),
            server: Some(handle),
        }
    }

    fn tcp(stream: TcpStream) -> Self {
        let writer = stream.try_clone().expect("clone the tcp stream");
        Self {
            reader: Box::new(BufReader::new(
                writer.try_clone().expect("clone for reading"),
            )),
            writer: Box::new(writer),
            sequence: 0,
            events: Vec::new(),
            server: None,
        }
    }

    fn send(&mut self, command: &str, arguments: Value) -> u64 {
        self.sequence += 1;
        let request = json!({
            "seq": self.sequence,
            "type": "request",
            "command": command,
            "arguments": arguments,
        });
        write_message(&mut self.writer, &request).expect("write a request");
        self.sequence
    }

    fn read(&mut self) -> Value {
        let body = read_message(&mut self.reader)
            .expect("framing")
            .expect("the adapter closed the stream");
        serde_json::from_slice(&body).expect("json message")
    }

    fn request(&mut self, command: &str, arguments: Value) -> Value {
        let sequence = self.send(command, arguments);
        for _ in 0..200 {
            let message = self.read();
            if message["type"] == "response" && message["request_seq"] == sequence {
                return message;
            }
            self.events.push(message);
        }
        panic!("no response to {command}");
    }

    fn ok(&mut self, command: &str, arguments: Value) -> Value {
        let response = self.request(command, arguments);
        assert_eq!(response["success"], true, "{command}: {response}");
        response["body"].clone()
    }

    fn wait_event(&mut self, event: &str) -> Value {
        if let Some(position) = self
            .events
            .iter()
            .position(|message| message["type"] == "event" && message["event"] == event)
        {
            return self.events.remove(position);
        }
        for _ in 0..400 {
            let message = self.read();
            if message["type"] == "event" && message["event"] == event {
                return message;
            }
            self.events.push(message);
        }
        panic!("no {event} event");
    }

    fn wait_output(&mut self, needle: &str) -> Value {
        for _ in 0..400 {
            let event = self.wait_event("output");
            if event["body"]["output"]
                .as_str()
                .unwrap_or("")
                .contains(needle)
            {
                return event;
            }
        }
        panic!("no output containing {needle:?}");
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        let _ = write_message(
            &mut self.writer,
            &json!({"seq": 9999, "type": "request", "command": "disconnect"}),
        );
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

// [ Fixtures ]

const TOKEN: &str = "dev-token-0123456789";

const SCRIPT: &str = r#"export function on_validate(doc) {
    const customer = host.get_doc({ doctype: "Customer", id: "c-1" });
    const base = doc.total;
    const total = add(base, customer.credit);
    return { total, customer };
}

function add(a, b) {
    const sum = a + b;
    return sum;
}
"#;

fn server(site: Arc<Site>) -> DebugServer {
    server_with(site, Duration::from_secs(30))
}

fn server_with(site: Arc<Site>, pause_timeout: Duration) -> DebugServer {
    let token = DevToken::new(TOKEN).expect("long enough");
    let policy = DebugPolicy::new(token).with_pause_timeout(pause_timeout);
    DebugServer::new(site, DebugServerConfig::new(policy))
}

fn launch_arguments() -> Value {
    json!({
        "devToken": TOKEN,
        "source": SCRIPT,
        "sourcePath": "scripts/on_validate.ts",
        "entry": "on_validate",
        "args": {"total": 10},
    })
}

fn set_breakpoints(client: &mut Client, lines: &[u64]) {
    let breakpoints: Vec<Value> = lines.iter().map(|line| json!({"line": line})).collect();
    let body = client.ok(
        "setBreakpoints",
        json!({
            "source": {"path": "scripts/on_validate.ts"},
            "breakpoints": breakpoints,
        }),
    );
    for breakpoint in body["breakpoints"].as_array().expect("array") {
        assert_eq!(breakpoint["verified"], true, "{breakpoint}");
    }
}

// [ Tests ]

#[test]
fn breakpoints_steps_and_variables_on_a_typescript_script() {
    let site = Site::dev();
    let mut client = Client::channels(server(Arc::clone(&site)));

    client.ok(
        "initialize",
        json!({"adapterID": "spot-devtools", "linesStartAt1": true}),
    );
    client.ok("launch", launch_arguments());
    // TypeScript line 3: the handle is already assigned, so it is inspectable.
    set_breakpoints(&mut client, &[3]);
    client.ok("configurationDone", json!({}));

    let stopped = client.wait_event("stopped");
    assert_eq!(stopped["body"]["reason"], "breakpoint", "{stopped}");

    let stack = client.ok("stackTrace", json!({"threadId": 1}));
    let frames = stack["stackFrames"].as_array().expect("frames");
    assert_eq!(frames[0]["name"], "on_validate");
    assert_eq!(frames[0]["line"], 3, "the sourcemapped breakpoint line");
    assert_eq!(frames[0]["source"]["path"], "scripts/on_validate.ts");
    let frame_id = frames[0]["id"].as_u64().expect("frame id");

    let scopes = client.ok("scopes", json!({"frameId": frame_id}));
    let locals = scopes["scopes"][0]["variablesReference"]
        .as_u64()
        .expect("locals reference");
    let variables = client.ok("variables", json!({"variablesReference": locals}));
    let customer = variables["variables"]
        .as_array()
        .expect("variables")
        .iter()
        .find(|variable| variable["name"] == "customer")
        .expect("customer handle");
    assert_eq!(customer["type"], "object");
    let handle = customer["variablesReference"].as_u64().expect("handle");
    let fields = client.ok("variables", json!({"variablesReference": handle}));
    let fields = fields["variables"].as_array().expect("fields");
    assert!(fields.iter().any(|field| field["name"] == "name"));
    let credit = fields
        .iter()
        .find(|field| field["name"] == "credit")
        .expect("credit");
    assert_eq!(credit["value"], "100");
    assert_eq!(credit["type"], "number");

    // The host function was really called once, with the document key the script asked for.
    assert_eq!(
        site.documents.lock().expect("documents").as_slice(),
        &[("Customer".to_owned(), "c-1".to_owned())]
    );

    // `evaluate` is read-only, over the paused frame's snapshot.
    let evaluated = client.ok(
        "evaluate",
        json!({"frameId": frame_id, "expression": "customer.credit"}),
    );
    assert_eq!(evaluated["result"], "100");
    let refused = client.request(
        "evaluate",
        json!({"frameId": frame_id, "expression": "customer.credit = 0"}),
    );
    assert_eq!(refused["success"], false, "{refused}");

    // Step over the `base` line: the next stop is line 4 (the call to add).
    client.ok("next", json!({"threadId": 1}));
    let stopped = client.wait_event("stopped");
    assert_eq!(stopped["body"]["reason"], "step");
    let stack = client.ok("stackTrace", json!({"threadId": 1}));
    assert_eq!(stack["stackFrames"][0]["line"], 4);

    // Step into `add`: two frames, the callee's locals are a and b.
    client.ok("stepIn", json!({"threadId": 1}));
    let stopped = client.wait_event("stopped");
    assert_eq!(stopped["body"]["reason"], "step");
    let stack = client.ok("stackTrace", json!({"threadId": 1}));
    let frames = stack["stackFrames"].as_array().expect("frames");
    assert_eq!(frames.len(), 3, "{stack}");
    assert_eq!(frames[0]["name"], "add");
    assert_eq!(frames[1]["name"], "on_validate");
    assert_eq!(frames[2]["name"], "script");
    assert_eq!(frames[0]["line"], 9);
    let scopes = client.ok("scopes", json!({"frameId": frames[0]["id"]}));
    let variables = client.ok(
        "variables",
        json!({"variablesReference": scopes["scopes"][0]["variablesReference"]}),
    );
    let names: Vec<&str> = variables["variables"]
        .as_array()
        .expect("variables")
        .iter()
        .filter_map(|variable| variable["name"].as_str())
        .collect();
    assert!(names.contains(&"a"), "{names:?}");
    assert!(names.contains(&"b"), "{names:?}");

    // Step out: back in on_validate at the return line.
    client.ok("stepOut", json!({"threadId": 1}));
    let stopped = client.wait_event("stopped");
    assert_eq!(stopped["body"]["reason"], "step");
    let stack = client.ok("stackTrace", json!({"threadId": 1}));
    let frames = stack["stackFrames"].as_array().expect("frames");
    assert_eq!(frames.len(), 2, "{stack}");
    assert_eq!(frames[0]["name"], "on_validate");
    assert_eq!(frames[1]["name"], "script");
    assert_eq!(frames[0]["line"], 5);

    // Continue to the end: the entry returns, and the result is shown before termination.
    client.ok("continue", json!({"threadId": 1}));
    let terminated = client.wait_event("terminated");
    assert_eq!(terminated["body"]["exitCode"], 0, "{terminated}");
    let results = client
        .events
        .iter()
        .filter(|message| message["type"] == "event" && message["event"] == "output")
        .map(|message| message["body"]["output"].as_str().unwrap_or("").to_owned());
    assert!(
        results
            .into_iter()
            .any(|text| text.contains("\"total\":110")),
        "the script result was not shown"
    );
}

#[test]
fn the_runtime_keeps_working_while_a_script_is_paused() {
    let site = Site::dev();
    let mut client = Client::channels(server(Arc::clone(&site)));
    client.ok("initialize", json!({}));
    client.ok("launch", launch_arguments());
    set_breakpoints(&mut client, &[2]);
    client.ok("configurationDone", json!({}));
    client.wait_event("stopped");

    // The paused script's thread is blocked, but the adapter answers and other work runs.
    let counter = Arc::new(AtomicUsize::new(0));
    let worker = {
        let counter = Arc::clone(&counter);
        std::thread::spawn(move || {
            for _ in 0..10_000 {
                counter.fetch_add(1, Ordering::Relaxed);
            }
        })
    };
    client.ok("threads", json!({}));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while counter.load(Ordering::Relaxed) < 10_000 && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    worker.join().expect("worker");
    assert_eq!(counter.load(Ordering::Relaxed), 10_000);

    // And the script is still paused where it was.
    let stack = client.ok("stackTrace", json!({"threadId": 1}));
    assert_eq!(stack["stackFrames"][0]["line"], 2);
}

#[test]
fn pause_stops_at_the_next_hook_and_continue_finishes_the_script() {
    let site = Site::dev();
    let mut client = Client::channels(server(site));
    client.ok("initialize", json!({}));
    client.ok("launch", launch_arguments());
    // Pause before the first instruction: the adapter stops at the first hook it reaches.
    client.ok("pause", json!({"threadId": 1}));
    client.ok("configurationDone", json!({}));
    let stopped = client.wait_event("stopped");
    assert_eq!(stopped["body"]["reason"], "pause", "{stopped}");
    let stack = client.ok("stackTrace", json!({"threadId": 1}));
    assert_eq!(stack["stackFrames"][0]["name"], "script");

    client.ok("continue", json!({"threadId": 1}));
    let terminated = client.wait_event("terminated");
    assert_eq!(terminated["body"]["exitCode"], 0, "{terminated}");
}

#[test]
fn a_production_site_is_refused() {
    let site = Site::production();
    let mut client = Client::channels(server(site));
    client.ok("initialize", json!({}));
    let response = client.request("launch", launch_arguments());
    assert_eq!(response["success"], false, "{response}");
    let message = response["message"].as_str().unwrap_or("");
    assert!(message.contains("devtools.production"), "{message}");
}

#[test]
fn a_wrong_token_is_refused() {
    let site = Site::dev();
    let mut client = Client::channels(server(site));
    client.ok("initialize", json!({}));
    let mut arguments = launch_arguments();
    arguments["devToken"] = Value::String("not-the-token-000000".into());
    let response = client.request("launch", arguments);
    assert_eq!(response["success"], false, "{response}");
    assert!(
        response["message"]
            .as_str()
            .unwrap_or("")
            .contains("devtools.token"),
        "{response}"
    );
}

#[test]
fn a_forgotten_pause_resumes_after_the_timeout() {
    let site = Site::dev();
    let mut client = Client::channels(server_with(site, Duration::from_millis(150)));
    client.ok("initialize", json!({}));
    client.ok("launch", launch_arguments());
    set_breakpoints(&mut client, &[2]);
    client.ok("configurationDone", json!({}));
    client.wait_event("stopped");

    // No continue is sent: the adapter resumes on its own and says so.
    let warning = client.wait_output("resumed after");
    assert!(
        warning["body"]["output"]
            .as_str()
            .unwrap_or("")
            .contains("150 ms"),
        "{warning}"
    );
    let terminated = client.wait_event("terminated");
    assert_eq!(terminated["body"]["exitCode"], 0, "{terminated}");
}

#[test]
fn the_tcp_transport_is_loopback_and_speaks_dap() {
    let site = Site::dev();
    let token = DevToken::new(TOKEN).expect("long enough");
    let server = DebugServer::new(
        site,
        DebugServerConfig::new(DebugPolicy::new(token))
            .with_address("127.0.0.1:0".parse().expect("address")),
    );
    let listener = server.bind().expect("bind loopback");
    let address = listener.local_addr().expect("local address");
    assert!(address.ip().is_loopback());
    let serving = server.clone();
    let acceptor = std::thread::spawn(move || {
        let _ = serving.serve_one(listener);
    });
    let stream = TcpStream::connect(address).expect("connect");
    let mut client = Client::tcp(stream);
    client.ok("initialize", json!({}));
    let mut arguments = launch_arguments();
    arguments["devToken"] = Value::String("short".into());
    let response = client.request("launch", arguments);
    assert_eq!(response["success"], false, "{response}");
    drop(client);
    let _ = acceptor.join();
}

#[test]
fn a_non_loopback_bind_needs_an_explicit_opt_in() {
    let site = Site::dev();
    let token = DevToken::new(TOKEN).expect("long enough");
    let config = DebugServerConfig::new(DebugPolicy::new(token))
        .with_address("0.0.0.0:0".parse().expect("address"));
    let server = DebugServer::new(site, config);
    let error = server.bind().expect_err("remote bind refused");
    assert_eq!(error.code(), "devtools.not_loopback");
}

// [ Session security ]

#[test]
fn host_calls_run_as_the_session_actor_and_a_denied_actor_is_denied() {
    let site = Site::viewer();
    let mut client = Client::channels(server(Arc::clone(&site)));
    client.ok("initialize", json!({}));
    // A client that tries to swap the authority in the request has no effect on it.
    let mut arguments = launch_arguments();
    arguments["actor"] = Value::String("developer".into());
    arguments["args"] = json!({"total": 10, "actor": "developer"});
    client.ok("launch", arguments);
    client.ok("configurationDone", json!({}));
    let terminated = client.wait_event("terminated");
    assert_eq!(terminated["body"]["exitCode"], 1, "{terminated}");
    assert_eq!(*site.callers.lock().unwrap(), vec!["viewer".to_owned()]);
    assert!(site.documents.lock().unwrap().is_empty());
}

#[test]
fn a_host_that_names_no_actor_refuses_the_launch() {
    struct NoActor;
    impl DebugHost for NoActor {
        fn site(&self) -> SiteId {
            SiteId::new("acme.example").expect("valid site id")
        }
        fn production(&self) -> bool {
            false
        }
        fn surface(&self) -> HostSurface {
            HostSurface::new(vec![])
        }
        fn session_actor(&self) -> Result<SessionActor, HostError> {
            Err(HostError::new("no_actor", "not signed in"))
        }
        fn call(&self, _: &SessionActor, name: &str, _: Value) -> Result<Value, HostError> {
            Err(HostError::new("unexpected", name))
        }
    }
    let token = DevToken::new(TOKEN).expect("long enough");
    let server = DebugServer::new(
        Arc::new(NoActor),
        DebugServerConfig::new(DebugPolicy::new(token)),
    );
    let mut client = Client::channels(server);
    client.ok("initialize", json!({}));
    let response = client.request("launch", launch_arguments());
    assert_eq!(response["success"], false, "{response}");
    assert!(
        response["message"]
            .as_str()
            .unwrap_or("")
            .contains("devtools.session")
    );
}

#[test]
fn the_script_result_reaches_the_client_redacted() {
    let site = Site::dev();
    let mut client = Client::channels(server(site));
    client.ok("initialize", json!({}));
    client.ok("launch", launch_arguments());
    client.ok("configurationDone", json!({}));
    let terminated = client.wait_event("terminated");
    assert_eq!(terminated["body"]["exitCode"], 0, "{terminated}");
    let wire: String = client
        .events
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(wire.contains("[redacted"), "no marker on the wire: {wire}");
    assert!(!wire.contains("hunter2-hidden-field"), "{wire}");
    assert!(!wire.contains("spk_live_sealed_value_0123456789"), "{wire}");
}

#[test]
fn variables_and_evaluate_are_redacted_too() {
    let site = Site::dev();
    let mut client = Client::channels(server(site));
    client.ok("initialize", json!({}));
    client.ok("launch", launch_arguments());
    set_breakpoints(&mut client, &[4]);
    client.ok("configurationDone", json!({}));
    let _ = client.wait_event("stopped");
    let stack = client.ok("stackTrace", json!({"threadId": 1}));
    let frame_id = stack["stackFrames"][0]["id"].clone();
    let evaluated = client.ok(
        "evaluate",
        json!({"frameId": frame_id, "expression": "customer.password"}),
    );
    assert!(
        !evaluated.to_string().contains("hunter2-hidden-field"),
        "{evaluated}"
    );
    let memo = client.ok(
        "evaluate",
        json!({"frameId": frame_id, "expression": "customer.memo"}),
    );
    assert!(
        !memo.to_string().contains("spk_live_sealed_value"),
        "{memo}"
    );
    let scopes = client.ok("scopes", json!({"frameId": frame_id}));
    let locals = client.ok(
        "variables",
        json!({"variablesReference": scopes["scopes"][0]["variablesReference"]}),
    );
    let customer = locals["variables"]
        .as_array()
        .expect("variables")
        .iter()
        .find(|variable| variable["name"] == "customer")
        .expect("customer");
    let fields = client.ok(
        "variables",
        json!({"variablesReference": customer["variablesReference"]}),
    );
    let text = fields.to_string();
    assert!(!text.contains("hunter2-hidden-field"), "{text}");
    assert!(!text.contains("spk_live_sealed_value"), "{text}");
    assert!(text.contains("redacted"), "{text}");
    client.ok("disconnect", json!({}));
}

#[test]
fn without_a_redactor_nothing_is_sent_raw() {
    let site = Site::dev();
    let token = DevToken::new(TOKEN).expect("long enough");
    let mut config = DebugServerConfig::new(DebugPolicy::new(token));
    config.redactor = None;
    let mut client = Client::channels(DebugServer::new(site, config));
    client.ok("initialize", json!({}));
    client.ok("launch", launch_arguments());
    client.ok("configurationDone", json!({}));
    let _ = client.wait_event("terminated");
    let wire: String = client.events.iter().map(Value::to_string).collect();
    assert!(!wire.contains("Acme"), "{wire}");
    assert!(wire.contains("withheld"), "{wire}");
}

fn program_server(root: &std::path::Path) -> DebugServer {
    let token = DevToken::new(TOKEN).expect("long enough");
    let config = DebugServerConfig::new(DebugPolicy::new(token)).with_program_root(root);
    DebugServer::new(Site::dev(), config)
}

fn program_launch(program: &str) -> Value {
    json!({"devToken": TOKEN, "program": program, "entry": "on_validate", "args": {"total": 10}})
}

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("spot-devtools-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir.canonicalize().expect("canonical scratch dir")
}

#[test]
fn a_program_inside_the_root_launches_and_every_escape_is_refused() {
    let base = scratch("program");
    let root = base.join("scripts");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::write(root.join("ok.ts"), SCRIPT).expect("script");
    std::fs::write(base.join("secret.ts"), SCRIPT).expect("outside script");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(base.join("secret.ts"), root.join("link.ts")).expect("link");
        std::os::unix::fs::symlink(&base, root.join("up")).expect("dir link");
    }

    let mut client = Client::channels(program_server(&root));
    client.ok("initialize", json!({}));
    client.ok("launch", program_launch("ok.ts"));
    client.ok("disconnect", json!({}));

    let outside = base.join("secret.ts").display().to_string();
    let mut escapes = vec![
        "../secret.ts".to_owned(),
        "a/../../secret.ts".to_owned(),
        "..\\secret.ts".to_owned(),
        outside,
        "/etc/passwd".to_owned(),
        "C:\\secret.ts".to_owned(),
        "ok.ts\0.png".to_owned(),
        String::new(),
        "missing.ts".to_owned(),
    ];
    #[cfg(unix)]
    escapes.extend(["link.ts".to_owned(), "up/secret.ts".to_owned()]);
    for program in escapes {
        let mut client = Client::channels(program_server(&root));
        client.ok("initialize", json!({}));
        let response = client.request("launch", program_launch(&program));
        assert_eq!(response["success"], false, "{program:?}: {response}");
        let message = response["message"].as_str().unwrap_or("");
        assert!(
            !message.contains(base.to_str().unwrap_or("?")),
            "{program:?} leaked a path: {message}"
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn a_program_launch_needs_a_configured_root() {
    let mut client = Client::channels(server(Site::dev()));
    client.ok("initialize", json!({}));
    let response = client.request("launch", program_launch("ok.ts"));
    assert_eq!(response["success"], false, "{response}");
    assert!(
        response["message"]
            .as_str()
            .unwrap_or("")
            .contains("program root")
    );
}
