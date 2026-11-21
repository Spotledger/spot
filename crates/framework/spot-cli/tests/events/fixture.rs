const PASSWORD: &str = "violet-lantern-staple-9-Quartz";

struct Reply {
    status: u16,
    headers: String,
    body: Json,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
}

async fn http(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    extra: &[(&str, &str)],
    body: Option<Json>,
) -> Reply {
    let body = body.map(|b| b.to_string()).unwrap_or_default();
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(token) = token {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    for (key, value) in extra {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str(&format!("\r\n{body}"));
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let raw = String::from_utf8(bytes).unwrap();
    let (headers, payload) = raw.split_once("\r\n\r\n").unwrap();
    let status = headers.split_whitespace().nth(1).unwrap().parse().unwrap();
    Reply {
        status,
        headers: headers.to_owned(),
        body: serde_json::from_str(payload).unwrap_or(Json::Null),
    }
}

/// An open `event_stream` connection read straight off the socket.
struct Sse {
    stream: tokio::net::TcpStream,
    seen: String,
}

impl Sse {
    async fn open(
        addr: SocketAddr,
        host: &str,
        token: &str,
        query: &str,
        extra: &[(&str, &str)],
    ) -> Sse {
        let mut head = format!(
            "GET /api/v1/events/stream?{query} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {token}\r\nAccept: text/event-stream\r\n"
        );
        for (key, value) in extra {
            head.push_str(&format!("{key}: {value}\r\n"));
        }
        head.push_str("\r\n");
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(head.as_bytes()).await.unwrap();
        Sse {
            stream,
            seen: String::new(),
        }
    }

    /// Reads until `needle` has arrived (anywhere in what was read), or `within` passes.
    async fn until(&mut self, needle: &str, within: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + within;
        let mut buf = [0u8; 8192];
        while !self.seen.contains(needle) {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return false;
            }
            match tokio::time::timeout(left, self.stream.read(&mut buf)).await {
                Ok(Ok(0)) | Err(_) => return self.seen.contains(needle),
                Ok(Ok(n)) => self.seen.push_str(&String::from_utf8_lossy(&buf[..n])),
                Ok(Err(_)) => return false,
            }
        }
        true
    }

    fn lease(&self) -> String {
        self.seen
            .lines()
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("x-consumer-lease")
                    .then(|| v.trim().to_owned())
            })
            .unwrap_or_default()
    }

    /// The `id:` of every event frame, in order.
    fn ids(&self) -> Vec<String> {
        self.seen
            .lines()
            .filter_map(|l| l.strip_prefix("id: "))
            .map(str::to_owned)
            .collect()
    }
}

/// The Administrator's full session: the one-time secret only chooses a password.
async fn administrator(addr: SocketAddr, host: &str, secret: &str) -> String {
    let first = http(
        addr,
        host,
        None,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({"user": "administrator", "password": secret})),
    )
    .await;
    assert_eq!(first.status, 200, "{:?}", first.body);
    let token = first.body["data"]["token"].as_str().unwrap().to_owned();
    let changed = http(
        addr,
        host,
        Some(&token),
        "POST",
        "/api/v1/auth/password",
        &[],
        Some(json!({"current": secret, "new": PASSWORD})),
    )
    .await;
    assert_eq!(changed.status, 200, "{:?}", changed.body);
    changed.body["data"]["token"].as_str().unwrap().to_owned()
}

struct Stack {
    env: Env,
    addr: SocketAddr,
    host: String,
    admin: String,
    server: Option<Running>,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
}

fn options() -> Options {
    Options {
        clock: None,
        login: LoginConfig::for_tests(),
    }
}

async fn serve(env: &Env) -> (Running, SocketAddr, tokio::sync::oneshot::Sender<()>) {
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let running = spot_cli::start_with(&env.root, false, options(), async {
        let _ = stopped.await;
    })
    .await
    .unwrap();
    let addr = running.local_addrs()[0];
    (running, addr, stop)
}

impl Stack {
    /// A bare site with the event stream tuned for the tests, and the host serving it.
    async fn start(users: &[(&str, &str)]) -> Option<Stack> {
        let env = Env::new().await?;
        let secret = env
            .ok(&["new-site", &env.site])
            .trim()
            .rsplit(' ')
            .next()
            .unwrap()
            .to_owned();
        let toml = env.root.join("common.toml");
        let mut text = std::fs::read_to_string(&toml).unwrap();
        text.push_str(
            "\n[services.events]\nenabled = true\npoll_ms = 50\nheartbeat_seconds = 1\nmax_idle_seconds = 30\nlease_seconds = 40\nmax_streams = 4\nmax_streams_per_user = 2\nconsumer_roles = [\"Event Reader\"]\nstream_roles = [\"Event Streamer\"]\n",
        );
        std::fs::write(&toml, text).unwrap();
        for (email, _) in users {
            spot_cli::users::add_user(&env.root, &env.site, email, None, &[])
                .await
                .unwrap();
            spot_cli::users::set_password(
                &env.root,
                &env.site,
                email,
                &spot_auth::Password::new(PASSWORD),
            )
            .await
            .unwrap();
        }
        let (server, addr, stop) = serve(&env).await;
        let host = env.site.clone();
        let admin = administrator(addr, &host, &secret).await;
        let stack = Stack {
            env,
            addr,
            host,
            admin,
            server: Some(server),
            stop: Some(stop),
        };
        // Each (user, role) pair gets a role of that name and an assignment.
        for (email, role) in users {
            if role.is_empty() {
                continue;
            }
            stack.assign(email, role).await;
        }
        Some(stack)
    }

    async fn call(
        &self,
        token: &str,
        method: &str,
        path: &str,
        extra: &[(&str, &str)],
        body: Option<Json>,
    ) -> Reply {
        http(
            self.addr,
            &self.host,
            Some(token),
            method,
            path,
            extra,
            body,
        )
        .await
    }

    async fn admin(&self, method: &str, path: &str, body: Option<Json>) -> Reply {
        self.call(&self.admin, method, path, &[], body).await
    }

    async fn role_id(&self, role: &str) -> String {
        let listed = self
            .admin("GET", "/api/v1/resource/role?limit=100", None)
            .await;
        let found = listed.body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|r| r["role_name"] == role)
            .and_then(|r| r["id"].as_str())
            .map(str::to_owned);
        match found {
            Some(id) => id,
            None => {
                let made = self
                    .admin(
                        "POST",
                        "/api/v1/resource/role",
                        Some(json!({"role_name": role, "origin": "reference"})),
                    )
                    .await;
                assert_eq!(made.status, 201, "{:?}", made.body);
                made.body["data"]["id"].as_str().unwrap().to_owned()
            }
        }
    }

    async fn assign(&self, email: &str, role: &str) {
        let users = self
            .admin("GET", "/api/v1/resource/user?limit=100", None)
            .await;
        let user = users.body["data"]
            .as_array()
            .unwrap()
            .iter()
            .find(|u| u["email"] == email || u["name"] == email)
            .and_then(|u| u["id"].as_str())
            .unwrap_or_else(|| panic!("no user {email}: {:?}", users.body))
            .to_owned();
        let role = self.role_id(role).await;
        let made = self
            .admin(
                "POST",
                "/api/v1/resource/role_assignment",
                Some(json!({"user": user, "role": role, "source": "manual", "state": "active", "reason": "events test"})),
            )
            .await;
        assert_eq!(made.status, 201, "{:?}", made.body);
    }

    async fn session(&self, email: &str) -> String {
        let reply = http(
            self.addr,
            &self.host,
            None,
            "POST",
            "/api/v1/auth/login",
            &[],
            Some(json!({"user": email, "password": PASSWORD})),
        )
        .await;
        assert_eq!(reply.status, 200, "{:?}", reply.body);
        reply.body["data"]["token"].as_str().unwrap().to_owned()
    }

    /// A new role document: one `doc_created` event about the entity `role`.
    async fn make_role(&self, name: &str) -> String {
        let made = self
            .admin(
                "POST",
                "/api/v1/resource/role",
                Some(json!({"role_name": name, "origin": "reference"})),
            )
            .await;
        assert_eq!(made.status, 201, "{:?}", made.body);
        made.body["data"]["id"].as_str().unwrap().to_owned()
    }

    async fn restart_host(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            server.join().await.unwrap();
        }
        let (server, addr, stop) = serve(&self.env).await;
        self.addr = addr;
        self.server = Some(server);
        self.stop = Some(stop);
        // Sessions live in memory: sign in again.
        self.admin = self.session("administrator").await;
    }

    async fn finish(mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(server) = self.server.take() {
            server.join().await.unwrap();
        }
    }
}

/// Base64url without padding: a position token is `v1.` and this of `v1|consumer|generation|seq`.
fn b64url(text: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in text.as_bytes().chunks(3) {
        let n =
            chunk.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize]));
        }
    }
    out
}

fn position(consumer: &str, generation: u64, seq: u64) -> String {
    format!(
        "v1.{}",
        b64url(&format!("v1|{consumer}|{generation}|{seq}"))
    )
}

fn spec(filter: &str, start: Json) -> Json {
    json!({
        "topics": ["doc_created"],
        "filter": filter,
        "partitions": 1,
        "payload_mode": "ids_only",
        "start": start,
    })
}

const ROLES_ONLY: &str = "doc.subject.entity == 'role'";

fn pull_body() -> Json {
    json!({"partition": 0, "limit": 100})
}

fn event_ids(reply: &Reply) -> Vec<String> {
    reply.body["data"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["event_id"].as_str().unwrap().to_owned())
        .collect()
}

fn subjects(reply: &Reply) -> Vec<String> {
    reply.body["data"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["subject"]["id"].as_str().unwrap().to_owned())
        .collect()
}
