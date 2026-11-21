//! A three-node cluster on one PostgreSQL (WP-TST-15.T6): N `spot serve` processes of one site
//! on distinct ports, a health-aware round-robin load balancer, an optional Redis, and the
//! helpers the proofs use to kill, pause, partition and restart a node.
//!
//! This file is included by `tests/cluster3.rs` with `#[path]` (it is not declared in
//! `lib.rs`), so it names the harness as `l9_scale::`.
//!
//! Layout: one base directory holds `common.toml`, the apps and the sites made with the `spot`
//! commands an operator runs; every node then gets its own copy of that directory with its own
//! `common.toml` (bind address, node id, the cluster profile, and a PostgreSQL URL that goes
//! through that node's [`TcpProxy`]). Cutting the proxy partitions the node from PostgreSQL
//! without privilege; no `iptables` is used.
//!
//! Nothing here reads a wall clock: waits are bounded loops of `tokio::time::sleep`, and the
//! caller passes the clock it wants to measure with.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use l9_scale::http::{Conn, Target};
use l9_scale::launcher;
use serde_json::{Value as Json, json};
use spot_postgres::testing::TestDb;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::watch;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
/// The node ids, in node order.
pub const NODE_IDS: [&str; 5] = ["node-a", "node-b", "node-c", "node-d", "node-e"];

/// What to build.
#[derive(Debug, Clone)]
pub struct ClusterOptions {
    /// The repository root (`apps/platform`, `apps/sample`).
    pub repo: PathBuf,
    /// The `spot` binary; found like the launcher does when `None`.
    pub spot_bin: Option<PathBuf>,
    /// Nodes, 1 to 5.
    pub nodes: usize,
    /// The run id: lowercase letters and digits.
    pub run_id: String,
    /// Sites in the cluster (the first is the one the proofs load; a second proves isolation).
    pub sites: usize,
    /// Use Redis for the coordinator, cache and bus (`redis-server` must be on the path).
    pub redis: bool,
    /// Scheduler and consumer lease time to live, milliseconds.
    pub lease_ttl_ms: u64,
}

fn free_port() -> Result<u16, String> {
    let listener =
        std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free port: {e}"))?;
    listener
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("no local address: {e}"))
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
    let entries =
        std::fs::read_dir(from).map_err(|e| format!("cannot read {}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = to.join(entry.file_name());
        if entry.file_type().map_err(|e| e.to_string())?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("cannot copy {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

/// The host and port of a `postgres://user:password@host:port/db` URL, and the URL with them
/// replaced by `host:port`. `None` for a URL without a TCP host (a Unix socket).
pub fn split_pg_url(url: &str) -> Option<(String, u16, String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let (credentials, after) = match rest.rsplit_once('@') {
        Some((c, a)) => (format!("{c}@"), a),
        None => (String::new(), rest),
    };
    let end = after.find(['/', '?']).unwrap_or(after.len());
    let (authority, tail) = after.split_at(end);
    if authority.is_empty() || authority.starts_with('/') || authority.contains("%2F") {
        return None;
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse().ok()?),
        None => (authority.to_owned(), 5432),
    };
    Some((
        host,
        port,
        format!("{scheme}://{credentials}"),
        tail.to_owned(),
    ))
}

/// The same URL pointed at `127.0.0.1:port`.
pub fn pg_url_via(prefix: &str, tail: &str, port: u16) -> String {
    format!("{prefix}127.0.0.1:{port}{tail}")
}

// --- TCP proxy: PostgreSQL partitions and the load balancer ---------------------------------------

/// A TCP relay whose traffic can be cut: while cut, new connections are closed at once and
/// every open relay is dropped.
pub struct TcpProxy {
    /// The loopback port it listens on.
    pub port: u16,
    cut: watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl TcpProxy {
    /// Relays `127.0.0.1:<port>` to `upstream`.
    pub async fn start(upstream: String) -> Result<TcpProxy, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("proxy bind: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let (cut, watcher) = watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    return;
                };
                if *watcher.borrow() {
                    let _ = client.shutdown().await;
                    continue;
                }
                let upstream = upstream.clone();
                let mut watcher = watcher.clone();
                tokio::spawn(async move {
                    let Ok(mut server) = TcpStream::connect(&upstream).await else {
                        return;
                    };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut client, &mut server) => {}
                        _ = watcher.wait_for(|cut| *cut) => {}
                    }
                });
            }
        });
        Ok(TcpProxy { port, cut, task })
    }

    /// Cuts (`true`) or restores (`false`) the traffic.
    pub fn set_cut(&self, cut: bool) {
        let _ = self.cut.send(cut);
    }
}

impl Drop for TcpProxy {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// A round-robin load balancer over the nodes' HTTP ports (a TCP relay per connection): a
/// connection goes to the next node a health probe last found ready. A client that keeps a
/// connection open stays on its node until the node goes away.
pub struct LoadBalancer {
    /// The loopback port clients use.
    pub port: u16,
    healthy: Arc<Vec<AtomicBool>>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl LoadBalancer {
    /// Starts the balancer and its probes (every 500 ms, `GET /api/health/ready`).
    pub async fn start(nodes: Vec<Target>) -> Result<LoadBalancer, String> {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| format!("lb bind: {e}"))?;
        let port = listener.local_addr().map_err(|e| e.to_string())?.port();
        let healthy: Arc<Vec<AtomicBool>> =
            Arc::new(nodes.iter().map(|_| AtomicBool::new(false)).collect());
        let mut tasks = Vec::new();
        for (i, target) in nodes.iter().cloned().enumerate() {
            let healthy = healthy.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    let mut conn = Conn::new();
                    let ok = conn
                        .request(
                            &target,
                            "GET",
                            "/api/health/ready",
                            &[],
                            None,
                            Duration::from_secs(2),
                        )
                        .await
                        .is_ok_and(|r| r.status == 200);
                    if let Some(flag) = healthy.get(i) {
                        flag.store(ok, Ordering::SeqCst);
                    }
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
            }));
        }
        let addrs: Vec<SocketAddr> = nodes.iter().map(|t| t.addr).collect();
        let flags = healthy.clone();
        let next = Arc::new(AtomicUsize::new(0));
        tasks.push(tokio::spawn(async move {
            loop {
                let Ok((mut client, _)) = listener.accept().await else {
                    return;
                };
                let start = next.fetch_add(1, Ordering::SeqCst);
                let pick = (0..addrs.len())
                    .map(|k| (start + k) % addrs.len())
                    .find(|i| flags.get(*i).is_some_and(|f| f.load(Ordering::SeqCst)));
                let Some(addr) = pick.and_then(|i| addrs.get(i).copied()) else {
                    let _ = client.shutdown().await;
                    continue;
                };
                tokio::spawn(async move {
                    if let Ok(mut server) = TcpStream::connect(addr).await {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    }
                });
            }
        }));
        Ok(LoadBalancer {
            port,
            healthy,
            tasks,
        })
    }

    /// Whether the probe found node `i` ready.
    pub fn is_healthy(&self, i: usize) -> bool {
        self.healthy
            .get(i)
            .is_some_and(|f| f.load(Ordering::SeqCst))
    }
}

impl Drop for LoadBalancer {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

// --- nodes -------------------------------------------------------------------------------------

/// One `spot serve` process.
pub struct Node {
    /// Its index, 0-based.
    pub index: usize,
    /// Its node id (`node-a`, ...).
    pub id: String,
    /// The port it serves on.
    pub port: u16,
    root: PathBuf,
    log: PathBuf,
    child: Option<Child>,
    /// The proxy its PostgreSQL connections go through.
    pub pg: TcpProxy,
}

impl Node {
    /// The Host header and address to reach this node directly, for `site`.
    pub fn target(&self, site: &str) -> Target {
        Target {
            addr: SocketAddr::from(([127, 0, 0, 1], self.port)),
            host: site.to_owned(),
        }
    }

    /// The process id while it runs.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().and_then(Child::id)
    }

    /// Whether the process is still running.
    pub fn running(&mut self) -> bool {
        match self.child.as_mut() {
            Some(c) => matches!(c.try_wait(), Ok(None)),
            None => false,
        }
    }
}

/// A running cluster.
pub struct Cluster {
    /// The first site's name (the Host header).
    pub site: String,
    /// The other sites.
    pub other_sites: Vec<String>,
    /// The Administrator's password (all sites).
    pub admin_password: String,
    /// The nodes.
    pub nodes: Vec<Node>,
    /// The balancer in front of them.
    pub lb: LoadBalancer,
    /// The Redis server, when one was asked for.
    pub redis: Option<Redis>,
    bin: PathBuf,
    base: PathBuf,
    db: TestDb,
    databases: Vec<String>,
    keep: bool,
}

/// A `redis-server` child.
pub struct Redis {
    /// Its port.
    pub port: u16,
    child: Option<Child>,
}

impl Redis {
    async fn spawn(port: u16) -> Result<Child, String> {
        Command::new("redis-server")
            .args([
                "--port",
                &port.to_string(),
                "--bind",
                "127.0.0.1",
                "--save",
                "",
                "--appendonly",
                "no",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start redis-server: {e}"))
    }

    /// Starts one on a free port, or `None` when `redis-server` is not installed.
    pub async fn start() -> Option<Redis> {
        let port = free_port().ok()?;
        let child = Self::spawn(port).await.ok()?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        Some(Redis {
            port,
            child: Some(child),
        })
    }

    /// Kills it.
    pub async fn kill(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.start_kill();
            let _ = c.wait().await;
        }
    }

    /// Starts it again on the same port.
    pub async fn restart(&mut self) -> Result<(), String> {
        self.kill().await;
        self.child = Some(Self::spawn(self.port).await?);
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(())
    }
}

async fn spot(
    bin: &Path,
    root: &Path,
    args: &[&str],
    stdin: Option<&str>,
) -> Result<String, String> {
    let mut command = Command::new(bin);
    command
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", bin.display()))?;
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(format!("{text}\n").as_bytes()).await;
        drop(pipe);
    }
    let output = child.wait_with_output().await.map_err(|e| {
        format!(
            "`spot {}` did not finish: {e}",
            args.first().copied().unwrap_or("")
        )
    })?;
    if !output.status.success() {
        // Standard output of some commands carries one-time secrets: only the error stream.
        return Err(format!(
            "`spot {}` failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn common_toml(
    repo: &Path,
    port: u16,
    pg_url: &str,
    node_id: Option<&str>,
    lease_ttl_ms: u64,
    redis_port: Option<u16>,
) -> String {
    let platform = repo.join("apps/platform");
    let mut text = format!(
        "apps = [{:?}, \"sample\"]\n[server]\nbind = [\"127.0.0.1:{port}\"]\nshutdown_grace = 5\n\
         [postgres]\nadmin_url = {pg_url:?}\n[trust]\nrequire_signed = false\n",
        platform.display().to_string()
    );
    if let Some(id) = node_id {
        text.push_str(&format!(
            "[telemetry]\nnode_id = {id:?}\n[services]\nprofile = \"cluster\"\n\
             [services.metadata]\nbus = \"polling\"\npoll_ms = 500\n\
             [services.jobs]\nstore = \"postgres\"\nlease_ttl_ms = {lease_ttl_ms}\n\
             [services.scheduler]\ntick_ms = 500\nlease_ttl_ms = {lease_ttl_ms}\n"
        ));
        if let Some(rp) = redis_port {
            text.push_str(&format!(
                "[services.cluster_redis]\nenabled = true\nurl = \"redis://127.0.0.1:{rp}\"\nallow_plaintext = true\n"
            ));
        }
    }
    text
}

impl Cluster {
    /// Builds and starts the cluster. `Ok(None)` when `SPOT_TEST_PG_URL` is unset (printed by
    /// the scratch-database helper) or the PostgreSQL URL has no TCP host (a partition proxy
    /// needs one).
    pub async fn start(options: &ClusterOptions) -> Result<Option<Cluster>, String> {
        launcher::validate_run_id(&options.run_id)?;
        if options.nodes == 0 || options.nodes > NODE_IDS.len() {
            return Err("nodes must be between 1 and 5".to_owned());
        }
        let Some(db) = TestDb::create().await else {
            return Ok(None);
        };
        let pg_url = std::env::var("SPOT_TEST_PG_URL")
            .map_err(|_| "SPOT_TEST_PG_URL vanished".to_owned())?;
        let Some((host, port, prefix, tail)) = split_pg_url(&pg_url) else {
            println!("SKIPPED: the cluster proofs need a TCP PostgreSQL URL in SPOT_TEST_PG_URL");
            return Ok(None);
        };
        let bin = launcher::locate_spot(&options.repo, options.spot_bin.as_deref()).await?;
        let base = std::env::temp_dir().join(format!("l9_cluster_{}", options.run_id));
        let _ = std::fs::remove_dir_all(&base);
        copy_dir(&options.repo.join("apps/sample"), &base.join("base/sample"))?;
        let base_root = base.join("base");
        std::fs::write(
            base_root.join("common.toml"),
            common_toml(
                &options.repo,
                free_port()?,
                &pg_url,
                None,
                options.lease_ttl_ms,
                None,
            ),
        )
        .map_err(|e| format!("cannot write common.toml: {e}"))?;

        let admin_password = launcher::random_password();
        let mut sites = Vec::new();
        let mut databases = Vec::new();
        for n in 0..options.sites.max(1) {
            let site = format!("l9-cluster-{}-{n}", options.run_id);
            databases.push(site.replace('-', "_"));
            spot(&bin, &base_root, &["new-site", &site], None).await?;
            spot(
                &bin,
                &base_root,
                &["set-password", &site, "administrator", "--password-stdin"],
                Some(&admin_password),
            )
            .await?;
            spot(
                &bin,
                &base_root,
                &[
                    "install-app",
                    "sample",
                    "--site",
                    &site,
                    "--apply",
                    "--seed",
                ],
                None,
            )
            .await?;
            sites.push(site);
        }

        let redis = if options.redis {
            Redis::start().await
        } else {
            None
        };
        if options.redis && redis.is_none() {
            println!("SKIPPED: the cluster-redis proofs need `redis-server` on the path");
            return Ok(None);
        }
        let mut nodes = Vec::new();
        for (i, node_id) in NODE_IDS.iter().enumerate().take(options.nodes) {
            let id = (*node_id).to_owned();
            let root = base.join(&id);
            copy_dir(&base_root, &root)?;
            let node_port = free_port()?;
            let pg = TcpProxy::start(format!("{host}:{port}")).await?;
            std::fs::write(
                root.join("common.toml"),
                common_toml(
                    &options.repo,
                    node_port,
                    &pg_url_via(&prefix, &tail, pg.port),
                    Some(&id),
                    options.lease_ttl_ms,
                    redis.as_ref().map(|r| r.port),
                ),
            )
            .map_err(|e| format!("cannot write common.toml: {e}"))?;
            nodes.push(Node {
                index: i,
                id,
                port: node_port,
                log: root.join("spot-serve.log"),
                root,
                child: None,
                pg,
            });
        }
        let first = sites.first().cloned().unwrap_or_default();
        let targets: Vec<Target> = nodes.iter().map(|n| n.target(&first)).collect();
        let lb = LoadBalancer::start(targets).await?;
        let mut cluster = Cluster {
            site: first,
            other_sites: sites.into_iter().skip(1).collect(),
            admin_password,
            nodes,
            lb,
            redis,
            bin,
            base,
            db,
            databases,
            keep: false,
        };
        for i in 0..cluster.nodes.len() {
            cluster.start_node(i).await?;
        }
        Ok(Some(cluster))
    }

    /// The load balancer as a target for `site`.
    pub fn lb_target(&self, site: &str) -> Target {
        Target {
            addr: SocketAddr::from(([127, 0, 0, 1], self.lb.port)),
            host: site.to_owned(),
        }
    }

    /// Node `i` as a target for `site`.
    pub fn node_target(&self, i: usize, site: &str) -> Target {
        self.nodes
            .get(i)
            .map(|n| n.target(site))
            .unwrap_or_else(|| self.lb_target(site))
    }

    /// Starts (or restarts) node `i` and waits until it answers ready.
    pub async fn start_node(&mut self, i: usize) -> Result<(), String> {
        let (bin, site) = (self.bin.clone(), self.site.clone());
        let node = self.nodes.get_mut(i).ok_or("no such node")?;
        let log =
            std::fs::File::create(&node.log).map_err(|e| format!("cannot create the log: {e}"))?;
        let log_err = log.try_clone().map_err(|e| e.to_string())?;
        node.child = Some(
            Command::new(&bin)
                .arg("--root")
                .arg(&node.root)
                .arg("serve")
                .stdin(Stdio::null())
                .stdout(Stdio::from(log))
                .stderr(Stdio::from(log_err))
                .kill_on_drop(true)
                .spawn()
                .map_err(|e| format!("cannot start `spot serve`: {e}"))?,
        );
        let target = node.target(&site);
        wait_ready(&target, 600)
            .await
            .map_err(|e| format!("{} ({}): {e}; log {}", node.id, i, node.log.display()))
    }

    /// Kills node `i` (SIGKILL) and waits for the process to go.
    pub async fn kill_node(&mut self, i: usize) {
        if let Some(node) = self.nodes.get_mut(i)
            && let Some(mut child) = node.child.take()
        {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }

    /// Stops node `i` the way an operator does (SIGTERM) and waits for it to exit.
    pub async fn stop_node(&mut self, i: usize) {
        if let Some(node) = self.nodes.get_mut(i)
            && let Some(mut child) = node.child.take()
        {
            if let Some(pid) = child.id() {
                let _ = signal(pid, "TERM").await;
            }
            if tokio::time::timeout(Duration::from_secs(30), child.wait())
                .await
                .is_err()
            {
                let _ = child.start_kill();
                let _ = child.wait().await;
            }
        }
    }

    /// Pauses (`SIGSTOP`) or resumes (`SIGCONT`) node `i`.
    pub async fn pause_node(&self, i: usize, paused: bool) -> Result<(), String> {
        let pid = self
            .nodes
            .get(i)
            .and_then(Node::pid)
            .ok_or("the node is not running")?;
        signal(pid, if paused { "STOP" } else { "CONT" }).await
    }

    /// Cuts or restores node `i`'s connection to PostgreSQL (the proxy-level partition).
    pub fn partition_node(&self, i: usize, cut: bool) {
        if let Some(node) = self.nodes.get(i) {
            node.pg.set_cut(cut);
        }
    }

    /// Signs in as the Administrator on `target`; the bearer header value.
    pub async fn admin_bearer(&self, target: &Target) -> Result<String, String> {
        let body = json!({ "user": "administrator", "password": self.admin_password }).to_string();
        let mut conn = Conn::new();
        let r = conn
            .request(
                target,
                "POST",
                "/api/v1/auth/login",
                &[("Content-Type", "application/json")],
                Some(body.as_bytes()),
                HTTP_TIMEOUT,
            )
            .await
            .map_err(|e| format!("sign-in failed: {e:?}"))?;
        if r.status != 200 {
            return Err(format!(
                "the Administrator's sign-in answered HTTP {}",
                r.status
            ));
        }
        r.json()
            .pointer("/data/token")
            .and_then(Json::as_str)
            .map(|t| format!("Bearer {t}"))
            .ok_or_else(|| "the sign-in returned no token".to_owned())
    }

    /// Stops everything and drops the databases, unless `keep` was set.
    pub async fn shutdown(mut self) {
        for i in 0..self.nodes.len() {
            self.kill_node(i).await;
        }
        if let Some(r) = self.redis.as_mut() {
            r.kill().await;
        }
        if self.keep {
            println!("l9 cluster: kept {}", self.base.display());
            return;
        }
        let client = self.db.admin_client().await;
        for database in &self.databases {
            let _ = client
                .batch_execute(&format!(
                    "DROP DATABASE IF EXISTS \"{database}\" WITH (FORCE)"
                ))
                .await;
        }
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

async fn signal(pid: u32, name: &str) -> Result<(), String> {
    let status = Command::new("kill")
        .arg(format!("-{name}"))
        .arg(pid.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map_err(|e| format!("cannot run kill: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("kill -{name} {pid} failed"))
    }
}

/// Waits (up to `tries` x 100 ms) until `/api/health/ready` answers 200 on `target`.
pub async fn wait_ready(target: &Target, tries: u32) -> Result<(), String> {
    for _ in 0..tries {
        let mut conn = Conn::new();
        if conn
            .request(
                target,
                "GET",
                "/api/health/ready",
                &[],
                None,
                Duration::from_secs(2),
            )
            .await
            .is_ok_and(|r| r.status == 200)
        {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err("not ready in time".to_owned())
}

/// One request with a bearer; `(status, body)`.
pub async fn call(
    target: &Target,
    bearer: &str,
    method: &str,
    path: &str,
    body: Option<&Json>,
) -> Result<(u16, Vec<u8>), String> {
    let bytes = body.map(|b| b.to_string().into_bytes());
    let mut conn = Conn::new();
    let r = conn
        .request(
            target,
            method,
            path,
            &[
                ("Authorization", bearer),
                ("Content-Type", "application/json"),
            ],
            bytes.as_deref(),
            HTTP_TIMEOUT,
        )
        .await
        .map_err(|e| format!("{method} {path} failed: {e:?}"))?;
    Ok((r.status, r.body))
}

/// The lease gauges and lease-lost counter read from a node's `/metrics`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LeaseView {
    /// `spot_lease_held{lease="scheduler"}` is 1.
    pub scheduler_held: bool,
    /// `spot_lease_held{lease="consumer"}` is 1.
    pub consumer_held: bool,
    /// Sum of `spot_lease_events_total{event="lost"}`.
    pub lost: u64,
}

/// Parses the lease metrics out of Prometheus text.
pub fn parse_leases(text: &str) -> LeaseView {
    let held = |kind: &str| {
        text.lines().any(|l| {
            l.starts_with("spot_lease_held{")
                && l.contains(&format!("lease=\"{kind}\""))
                && l.rsplit(' ').next().is_some_and(|v| v.starts_with('1'))
        })
    };
    let lost = text
        .lines()
        .filter(|l| l.starts_with("spot_lease_events_total{") && l.contains("event=\"lost\""))
        .filter_map(|l| l.rsplit(' ').next())
        .filter_map(|v| v.split('.').next()?.parse::<u64>().ok())
        .fold(0, u64::saturating_add);
    LeaseView {
        scheduler_held: held("scheduler"),
        consumer_held: held("consumer"),
        lost,
    }
}

impl Cluster {
    /// The lease view of node `i`, or `None` when its metrics cannot be read.
    pub async fn leases_of(&self, i: usize) -> Option<LeaseView> {
        let target = self.node_target(i, &self.site);
        let bearer = self.admin_bearer(&target).await.ok()?;
        match call(&target, &bearer, "GET", "/metrics", None).await {
            Ok((200, raw)) => Some(parse_leases(&String::from_utf8_lossy(&raw))),
            _ => None,
        }
    }
}
