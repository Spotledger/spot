//! Shared helpers for the tests that run a whole host over real PostgreSQL: a root with the
//! platform and a private copy of the sample app, an in-process `spot serve`, a small HTTP
//! client that keeps bodies as bytes, and a WebSocket client for the realtime route.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]
use serde_json::{Value as Json, json};
use spot_cli::{Options, Running};
use spot_login::LoginConfig;
use spot_postgres::testing::TestDb;
use spot_sessions::SessionPolicy;
use spot_sessions::throttle::ThrottleConfig;
use spot_storage_contract::RecordId;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const ADMIN_PASSWORD: &str = "violet-lantern-staple-9-Quartz";

pub mod notes;

pub struct Root(pub PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

pub fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

pub fn run(root: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_spot"))
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(text) = stdin {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

pub fn ok(root: &Path, args: &[&str]) -> String {
    let out = run(root, args, None);
    assert!(
        out.status.success(),
        "{args:?}: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

pub fn secret(out: &str) -> String {
    out.trim().rsplit(' ').next().unwrap().to_owned()
}

pub struct Env {
    pub root: Root,
    pub site: String,
    pub url: String,
    pub services: String,
    pub _db: TestDb,
}

impl Env {
    /// A root with the platform and the sample app. `services` is appended to `common.toml`
    /// (`[services.jobs]` and the like). `None` when no PostgreSQL is configured.
    pub async fn new(services: &str) -> Option<Env> {
        let db = TestDb::create().await?;
        let url = std::env::var("SPOT_TEST_PG_URL").unwrap();
        let root = Root(std::env::temp_dir().join(format!("spot_cli_t8_{}", RecordId::new_v7())));
        copy_dir(&repo().join("apps/sample"), &root.0.join("sample"));
        // The same two fixture patches `e2e.rs` makes: the confidential group of Task's
        // `details`, and the `created_by` field the owner rule compares.
        let task = root.0.join("sample/doctypes/task.toml");
        let text = std::fs::read_to_string(&task)
            .unwrap()
            .replace(
                "title_field = \"subject\"",
                "title_field = \"subject\"\n\n[[protection_groups]]\nname = \"confidential\"\nlabel = \"Confidential\"\ntrace = true",
            )
            .replace(
                "name = \"details\"\nlabel = \"Details\"\ntype = \"long_text\"",
                "name = \"details\"\nlabel = \"Details\"\ntype = \"long_text\"\nprotection_group = \"confidential\"",
            );
        std::fs::write(&task, text).unwrap();
        let entry = root.0.join("sample/doctypes/time_entry.toml");
        let text = std::fs::read_to_string(&entry).unwrap()
            + "\n[[fields]]\nname = \"created_by\"\nlabel = \"Created By\"\ntype = \"text\"\nmax_chars = 64\n";
        std::fs::write(&entry, text).unwrap();
        let site = format!(
            "spot-test-{}",
            RecordId::new_v7().to_string().replace('-', "")
        );
        let env = Env {
            root,
            site,
            url,
            services: services.to_owned(),
            _db: db,
        };
        env.write_common(&["'sample'"], services);
        Some(env)
    }

    /// Rewrites `common.toml` with the platform and `apps` (quoted names or paths).
    pub fn write_common(&self, apps: &[&str], services: &str) {
        std::fs::write(
            self.root.0.join("common.toml"),
            format!(
                "apps = [{:?}, {}]\n[server]\nbind = ['127.0.0.1:0']\nshutdown_grace = 2\n[postgres]\nadmin_url = {:?}\n{services}\n[trust]\nrequire_signed = false\n",
                repo().join("apps/platform").display().to_string(),
                apps.join(", "),
                self.url,
            ),
        )
        .unwrap();
    }

    /// Creates the site with every app; returns the one-time secret.
    pub fn site(&self) -> String {
        secret(&ok(&self.root.0, &["new-site", &self.site]))
    }

    pub fn user(&self, email: &str, password: &str) {
        ok(&self.root.0, &["add-user", &self.site, email]);
        let out = run(
            &self.root.0,
            &["set-password", &self.site, email, "--password-stdin"],
            Some(&format!("{password}\n")),
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// Chooses a password for a user of `site` (the way `spot set-password` does).
    pub fn set_password(&self, site: &str, email: &str, password: &str) {
        let out = run(
            &self.root.0,
            &["set-password", site, email, "--password-stdin"],
            Some(&format!("{password}\n")),
        );
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// The path of a file in the site's folder.
    pub fn site_dir(&self) -> PathBuf {
        self.root.0.join("sites").join(&self.site)
    }
}

pub fn fast_options() -> Options {
    Options {
        clock: None,
        login: LoginConfig {
            session_policy: SessionPolicy::DEFAULT,
            throttle: ThrottleConfig {
                backoff_after: 20,
                lockout_after: 30,
                window: Duration::from_secs(60),
                backoff_base: Duration::from_millis(1),
                backoff_multiplier: 1,
                backoff_cap: Duration::from_millis(1),
                lockout_duration: Duration::from_secs(60),
            },
            cache_ttl: Duration::ZERO,
            ..LoginConfig::for_tests()
        },
    }
}

pub struct Started {
    pub running: Running,
    pub addr: SocketAddr,
    pub stop: tokio::sync::oneshot::Sender<()>,
}

impl Started {
    /// Stops the host and waits for its services to drain.
    pub async fn finish(self) {
        let _ = self.stop.send(());
        let _ = self.running.join().await;
    }
}

pub async fn start(env: &Env) -> Started {
    start_with(env, fast_options()).await
}

pub async fn start_with(env: &Env, options: Options) -> Started {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running = spot_cli::start_with(&env.root.0, false, options, async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = running.local_addrs()[0];
    Started {
        running,
        addr,
        stop,
    }
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub etag: String,
    pub bytes: Vec<u8>,
    pub body: Json,
}

impl Reply {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

fn dechunk(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") else {
            return out;
        };
        let size = usize::from_str_radix(
            std::str::from_utf8(&rest[..line_end])
                .unwrap()
                .split(';')
                .next()
                .unwrap()
                .trim(),
            16,
        )
        .unwrap_or(0);
        rest = &rest[line_end + 2..];
        if size == 0 || rest.len() < size {
            return out;
        }
        out.extend_from_slice(&rest[..size]);
        rest = rest.get(size + 2..).unwrap_or(&[]);
    }
}

pub async fn http_raw(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    (content_type, body): (&str, &[u8]),
) -> Reply {
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    for (k, v) in headers {
        request.push_str(&format!("{k}: {v}\r\n"));
    }
    request.push_str("\r\n");
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).await.unwrap();
    let split = bytes
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("no header end in {:?}", String::from_utf8_lossy(&bytes)));
    let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
    let mut raw_body = bytes[split + 4..].to_vec();
    let mut lines = head.lines();
    let status: u16 = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect();
    if headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("transfer-encoding") && v.contains("chunked"))
    {
        raw_body = dechunk(&raw_body);
    }
    let etag = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("etag"))
        .map(|(_, v)| v.clone())
        .unwrap_or_default();
    let json = serde_json::from_slice(&raw_body).unwrap_or(Json::Null);
    Reply {
        status,
        headers,
        etag,
        bytes: raw_body,
        body: json,
    }
}

pub async fn http(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Json>,
) -> Reply {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    http_raw(
        addr,
        host,
        token,
        method,
        path,
        headers,
        ("application/json", body.as_bytes()),
    )
    .await
}

#[track_caller]
pub fn expect(reply: &Reply, status: u16) {
    assert_eq!(
        reply.status,
        status,
        "{}",
        String::from_utf8_lossy(&reply.bytes[..reply.bytes.len().min(400)])
    );
}

pub async fn login(addr: SocketAddr, host: &str, user: &str, password: &str) -> Reply {
    http(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({"user": user, "password": password})),
    )
    .await
}

pub async fn token_of(addr: SocketAddr, host: &str, user: &str, password: &str) -> String {
    let reply = login(addr, host, user, password).await;
    expect(&reply, 200);
    reply.body["data"]["token"].as_str().unwrap().to_owned()
}

/// The id of the user a session token belongs to.
pub async fn me(addr: SocketAddr, host: &str, token: &str) -> String {
    let reply = http(
        addr,
        host,
        Some(token),
        "GET",
        "/api/v1/auth/session",
        &[],
        None,
    )
    .await;
    expect(&reply, 200);
    reply.body["data"]["user"]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Signs the Administrator in with the one-time secret, chooses `ADMIN_PASSWORD`, and returns
/// the session token that replaced the restricted one.
pub async fn admin(addr: SocketAddr, host: &str, first_secret: &str) -> String {
    let first = login(addr, host, "administrator", first_secret).await;
    expect(&first, 200);
    let token = first.body["data"]["token"].as_str().unwrap().to_owned();
    let changed = http(
        addr,
        host,
        Some(&token),
        "POST",
        "/api/v1/auth/password",
        &[],
        Some(json!({"current": first_secret, "new": ADMIN_PASSWORD})),
    )
    .await;
    expect(&changed, 200);
    changed.body["data"]["token"].as_str().unwrap().to_owned()
}

pub async fn user_id(addr: SocketAddr, host: &str, admin: &str, email: &str) -> String {
    let reply = http(
        addr,
        host,
        Some(admin),
        "GET",
        "/api/v1/resource/user?limit=100",
        &[],
        None,
    )
    .await;
    expect(&reply, 200);
    reply.body["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["email"] == email)
        .map(|row| row["id"].as_str().unwrap().to_owned())
        .unwrap()
}

pub async fn role(addr: SocketAddr, host: &str, admin: &str, name: &str) -> String {
    let reply = http(
        addr,
        host,
        Some(admin),
        "POST",
        "/api/v1/resource/role",
        &[],
        Some(json!({"role_name": name, "origin": "reference"})),
    )
    .await;
    expect(&reply, 201);
    reply.body["data"]["id"].as_str().unwrap().to_owned()
}

pub async fn assign(addr: SocketAddr, host: &str, admin: &str, user: &str, role: &str) {
    let reply = http(
        addr,
        host,
        Some(admin),
        "POST",
        "/api/v1/resource/role_assignment",
        &[],
        Some(json!({
            "user": user, "role": role, "source": "manual", "state": "active",
            "reason": "Assigned by the test operator."
        })),
    )
    .await;
    expect(&reply, 201);
}

/// What a WebSocket read returned.
#[derive(Debug)]
pub enum Frame {
    Text(Json),
    Close(u16),
    /// Nothing arrived in time.
    Quiet,
    /// The server closed the TCP connection without a close frame.
    Gone,
}

/// A minimal RFC 6455 client: masked text frames out, unmasked frames in.
pub struct Ws {
    stream: tokio::net::TcpStream,
    buffer: Vec<u8>,
}

impl Ws {
    pub async fn connect(addr: SocketAddr, host: &str) -> Ws {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let request = format!(
            "GET /api/v1/realtime HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: spot.v1\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        assert!(head.starts_with("HTTP/1.1 101"), "upgrade refused: {head}");
        Ws {
            stream,
            buffer: Vec::new(),
        }
    }

    pub async fn send(&mut self, message: Json) {
        let payload = message.to_string().into_bytes();
        let mut frame = vec![0x81u8];
        let mask = [0x12u8, 0x34, 0x56, 0x78];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame).await.unwrap();
    }

    /// The next frame that is not a ping or pong, within `wait`.
    pub async fn next(&mut self, wait: Duration) -> Frame {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some((opcode, payload, used)) = self.parse() {
                self.buffer.drain(..used);
                match opcode {
                    0x1 => return Frame::Text(serde_json::from_slice(&payload).unwrap()),
                    0x8 => {
                        let code = if payload.len() >= 2 {
                            u16::from_be_bytes([payload[0], payload[1]])
                        } else {
                            1005
                        };
                        return Frame::Close(code);
                    }
                    _ => continue,
                }
            }
            let mut chunk = [0u8; 4096];
            match tokio::time::timeout_at(deadline, self.stream.read(&mut chunk)).await {
                Err(_) => return Frame::Quiet,
                Ok(Ok(0) | Err(_)) => return Frame::Gone,
                Ok(Ok(n)) => self.buffer.extend_from_slice(&chunk[..n]),
            }
        }
    }

    fn parse(&self) -> Option<(u8, Vec<u8>, usize)> {
        let b = &self.buffer;
        if b.len() < 2 {
            return None;
        }
        let opcode = b[0] & 0x0f;
        let mut len = usize::from(b[1] & 0x7f);
        let mut at = 2;
        if len == 126 {
            len = usize::from(u16::from_be_bytes([*b.get(2)?, *b.get(3)?]));
            at = 4;
        } else if len == 127 {
            return None;
        }
        if b.len() < at + len {
            return None;
        }
        Some((opcode, b[at..at + len].to_vec(), at + len))
    }

    /// Reads text frames until one has `type == kind`, within `wait`.
    pub async fn expect_type(&mut self, kind: &str, wait: Duration) -> Json {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match self.next(left).await {
                Frame::Text(message) if message["type"] == kind => return message,
                Frame::Text(_) => continue,
                other => panic!("waiting for `{kind}`, got {other:?}"),
            }
        }
    }

    /// Authenticates and waits for `ready`.
    pub async fn sign_in(&mut self, token: &str) -> Json {
        self.send(json!({"type": "auth", "token": token})).await;
        self.expect_type("ready", Duration::from_secs(5)).await
    }

    /// Joins `room`; returns the `joined` or `join_error` reply.
    pub async fn join(&mut self, room: &str) -> Json {
        self.send(json!({"type": "join", "ref": room, "room": room}))
            .await;
        let deadline = Duration::from_secs(5);
        loop {
            match self.next(deadline).await {
                Frame::Text(m) if m["type"] == "joined" || m["type"] == "join_error" => return m,
                Frame::Text(_) => continue,
                other => panic!("waiting for a join reply, got {other:?}"),
            }
        }
    }
}
