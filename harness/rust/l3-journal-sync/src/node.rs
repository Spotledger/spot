//! [`Node`]: one **in-process** host — `spot_cli::start_with`, the same assembly the `spot`
//! binary's `serve` command runs, started as a task in this test process, not as a separate
//! OS process — with its own PostgreSQL database, root and journal folder, and lifecycle
//! controls for the later journal/sync tasks to drive.
//!
//! The node's "sync side" has no real listener yet (no sync engine exists on `dev` to host):
//! [`Node::sync_addr_for`] is a loopback proxy in front of a plain echo loop, standing in for
//! where a later task's real sync listener will be. It exists so [`Node::partition_from`] and
//! [`Node::heal`] can be proved against a real TCP connection today, not to claim sync traffic
//! is exercised.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::json;
use spot_auth::Password;
use spot_cli::SiteKind;
use spot_postgres::testing::TestDb;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

use crate::client::Client;
use crate::proxy::Proxy;
use crate::wait_until;

/// What a node is in the cluster: later tasks read this (and `extra_config_toml`) through
/// [`role_fragment`]. This harness carries the value; it has no behaviour of its own yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    /// No hub, no edges: a single node with nothing else to sync with.
    Standalone,
    /// The side edges connect to.
    Hub,
    /// Syncs through one hub, named here.
    Edge {
        /// The hub's node name.
        hub: String,
    },
}

/// What [`Cluster::add_node`](crate::Cluster::add_node) needs to start one node.
#[derive(Clone, Debug)]
pub struct NodeSpec {
    /// The node's name within the cluster (used for its database, root and site names).
    pub name: String,
    /// The node's role.
    pub role: Role,
    /// Extra TOML a later task wants carried alongside the role fragment (see
    /// [`role_fragment`]). Empty for this harness's own tests.
    pub extra_config_toml: String,
}

/// The role fragment for `spec`, built in one place so every later task that gives a node a
/// real role changes this function instead of its own copy.
///
/// Today `spot_cli::config::Common` and `Site` both reject unknown TOML keys
/// (`#[serde(deny_unknown_fields)]`), and neither declares a `[node]` table: splicing this text
/// into `common.toml` or `site.toml` as *live* configuration would refuse every node's start
/// before any role exists to read it. [`Node::start_fresh`] therefore writes this fragment's
/// text to a side file (`<root>/role.toml`, never read by `spot_cli::config::load`) rather than
/// into the parsed config; see this crate's `README.md` and `## Open questions` in the PR. The
/// text this function returns is the real, uncommented fragment a later task's own config
/// change can splice in once it exists; only the *write site* is a placeholder.
#[must_use]
pub fn role_fragment(spec: &NodeSpec) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("[node]\n");
    match &spec.role {
        Role::Standalone => out.push_str("role = \"standalone\"\n"),
        Role::Hub => out.push_str("role = \"hub\"\n"),
        Role::Edge { hub } => {
            out.push_str("role = \"edge\"\n");
            let _ = writeln!(out, "hub = {hub:?}");
        }
    }
    if !spec.extra_config_toml.is_empty() {
        out.push('\n');
        out.push_str(&spec.extra_config_toml);
        if !spec.extra_config_toml.ends_with('\n') {
            out.push('\n');
        }
    }
    out
}

/// The sample app's demo doctype [`crate::seed::documents`] creates: a `project`, exactly as
/// `harness/rust/l3-http`'s fixture uses it.
pub(crate) const SEED_DOCTYPE: &str = "project";

/// The Administrator's password, derived from the scratch database's own (already unique)
/// name, never from the wall clock or an OS random source this crate would have to read itself.
fn admin_password(db_name: &str) -> String {
    let tail = db_name
        .get(db_name.len().saturating_sub(12)..)
        .unwrap_or(db_name);
    format!("ljs-{tail}-Quartz9")
}

/// One real `spot serve` process and everything a later task's test needs to drive it: its own
/// database, root, journal folder, loopback API and sync addresses, and an administrator client.
pub struct Node {
    name: String,
    spec: NodeSpec,
    root: PathBuf,
    site: String,
    /// The site's actual PostgreSQL database name (`spot_cli::new_site_of` derives it from the
    /// site name, hyphens turned to underscores): where the sample app's tables, and anything
    /// a later journal/sync task writes, actually live. The scratch `db` below is a second,
    /// otherwise-unused database this harness creates only for its unique run id and its own
    /// drop guard, mirroring `harness/rust/l3-http`'s fixture.
    site_db: String,
    /// The admin connection URL (`SPOT_TEST_PG_URL`), kept to open a connection to `site_db`
    /// and to drop it at teardown (new-site's own database is not the scratch `db`, so its
    /// own `Drop` cannot do that part).
    admin_url: String,
    admin_password: String,
    admin_key: String,
    db: Option<TestDb>,
    log: PathBuf,
    admin_id: Mutex<String>,
    client: Mutex<Option<Client>>,
    running: Mutex<Option<spot_cli::Running>>,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    sync_upstream_addr: SocketAddr,
    sync_upstream_shutdown: Mutex<Option<oneshot::Sender<()>>>,
    /// One loopback proxy per peer that dials this node's sync address, keyed by the peer's
    /// name and created lazily (on first [`Node::sync_addr_for`] or [`Node::partition_from`]
    /// call naming that peer). Pairwise: pausing the entry for `"edge-a"` never touches the
    /// entry for `"edge-b"`, so partitioning two nodes in a larger cluster leaves every other
    /// pair's traffic flowing.
    proxies: Mutex<BTreeMap<String, Proxy>>,
}

impl Node {
    /// Creates the database, the root, the site and its one sample app, starts the host, and
    /// signs the Administrator in. `None` when no PostgreSQL is configured (the caller prints
    /// why, following `spot_postgres::testing::TestDb::create`'s own convention).
    ///
    /// `repo` is a checkout to read `apps/platform` and `apps/sample` from
    /// ([`crate::Cluster::add_node`] always passes this crate's own repository); exposed with
    /// an explicit `repo` so `tests/harness_self.rs` can prove a failed start removes its root
    /// by pointing it at a checkout that is missing those apps, without needing a second kind
    /// of break.
    ///
    /// # Panics
    /// When any step after the database exists fails (a missing `apps/sample`, a site that
    /// cannot be created, a host that cannot start): the root this call created is removed by
    /// a guard armed until the returned [`Node`] owns it, so a failure here never leaks a root,
    /// exactly as `harness/rust/l3-http`'s fixture guarantees for its own root.
    pub async fn create(repo: &Path, spec: NodeSpec) -> Option<Node> {
        let db = TestDb::create().await?;
        let run = db.name().trim_start_matches("spot_test_").to_owned();
        let slug = sanitize(&spec.name);
        let root = std::env::temp_dir().join(format!("l3_journal_sync_{slug}_{run}"));
        std::fs::create_dir_all(&root)
            .unwrap_or_else(|e| panic!("cannot create the node root {}: {e}", root.display()));
        let guard = RootGuard {
            path: root.clone(),
            armed: !keep(),
        };
        // The site name carries the whole run id, not just a prefix: `run` is a UUIDv7's hex
        // digits, whose first 12 characters are *purely* a millisecond timestamp (the known
        // layout of UUIDv7) with no randomness at all — truncating to 12 (as `l3-http`'s
        // fixture does, for a single node per run) lets two nodes created in the same
        // millisecond collide on the same site name. This harness creates several nodes per
        // test and runs several tests concurrently in one binary, where that collision is
        // observable in practice, not just theoretical.
        let site = format!("ljs-{slug}-{run}");
        std::fs::write(root.join("role.toml"), role_fragment(&spec))
            .unwrap_or_else(|e| panic!("cannot write role.toml: {e}"));
        let url = std::env::var("SPOT_TEST_PG_URL").unwrap_or_else(|_| {
            panic!("SPOT_TEST_PG_URL disappeared between TestDb::create and here")
        });
        std::fs::write(
            root.join("common.toml"),
            format!(
                "apps = [{:?}, \"sample\"]\n\
                 [server]\n\
                 bind = [\"127.0.0.1:0\"]\n\
                 shutdown_grace = 5\n\
                 [postgres]\n\
                 admin_url = {url:?}\n\
                 pool_max_size = 16\n",
                repo.join("apps/platform").display().to_string()
            ),
        )
        .unwrap_or_else(|e| panic!("cannot write common.toml: {e}"));
        copy_dir(&repo.join("apps/sample"), &root.join("sample"));
        let log = root.join("l3-journal-sync.log");
        let secret = spot_cli::new_site_of(&root, &site, "en", SiteKind::HostWide)
            .await
            .unwrap_or_else(|e| panic!("new-site `{site}`: {e}"));
        let admin_key = secret.text().to_owned();
        let password = admin_password(db.name());
        spot_cli::users::set_password(&root, &site, "administrator", &Password::new(&password))
            .await
            .unwrap_or_else(|e| panic!("set-password administrator: {e}"));
        let output = spot_cli::ops::install_app(
            &root,
            "sample",
            None,
            spot_cli::ops::Install {
                apply: true,
                seed: false,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("install-app sample: {e}"));
        let _ = output; // the exact wording is not this harness's concern, only that it did not error

        // The node's own sync-side listener: a plain echo loop, independent of `spot serve` (no
        // sync service exists to host yet). It outlives a `stop`/`start` cycle of the host, so
        // the proxy and the public sync address stay the same across a restart.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|e| panic!("cannot bind the node's sync listener: {e}"));
        let sync_upstream_addr = listener
            .local_addr()
            .unwrap_or_else(|e| panic!("cannot read the sync listener's address: {e}"));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        tokio::spawn(sync_echo_loop(listener, shutdown_rx));

        let mut node = Node {
            name: spec.name.clone(),
            spec,
            root,
            site: site.clone(),
            site_db: site.replace('-', "_"),
            admin_url: url,
            admin_password: password,
            admin_key,
            db: Some(db),
            log,
            admin_id: Mutex::new(String::new()),
            client: Mutex::new(None),
            running: Mutex::new(None),
            stop: Mutex::new(None),
            sync_upstream_addr,
            sync_upstream_shutdown: Mutex::new(Some(shutdown_tx)),
            proxies: Mutex::new(BTreeMap::new()),
        };
        node.boot_host().await;
        // The node owns the root from here: its own Drop removes it (or `L3_KEEP=1` keeps
        // it), so the start guard above must not act too.
        std::mem::forget(guard);
        Some(node)
    }

    /// Starts (or restarts) the host over this node's existing root and database, waits for
    /// readiness, and signs the Administrator back in (sessions are in memory and do not
    /// survive a restart).
    async fn boot_host(&mut self) {
        let (stop, stopped) = oneshot::channel();
        let running =
            spot_cli::start_with(&self.root, false, spot_cli::Options::default(), async {
                let _ = stopped.await;
            })
            .await
            .unwrap_or_else(|e| panic!("node `{}` did not start: {e}", self.name));
        let addr = *running
            .local_addrs()
            .first()
            .unwrap_or_else(|| panic!("node `{}` bound no address", self.name));
        *self.running.lock().unwrap_or_else(|p| p.into_inner()) = Some(running);
        *self.stop.lock().unwrap_or_else(|p| p.into_inner()) = Some(stop);
        let anonymous = Client::new(addr, self.site.clone());
        self.note(&format!("host listening on {addr}"));
        wait_until(Duration::from_secs(30), || {
            let anonymous = anonymous.clone();
            async move {
                matches!(anonymous.health("ready").await, Ok(reply) if reply.status == 200)
            }
        })
        .await
        .unwrap_or_else(|e| panic!("node `{}` never became ready: {e}", self.name));
        let admin = anonymous
            .sign_in("administrator", &self.admin_password)
            .await
            .unwrap_or_else(|e| panic!("node `{}` administrator sign-in: {e}", self.name));
        let session = admin
            .session()
            .await
            .unwrap_or_else(|e| panic!("node `{}` administrator session: {e}", self.name));
        session
            .expect(200)
            .unwrap_or_else(|e| panic!("node `{}`: {e}", self.name));
        let admin_id = session
            .data()
            .get("user")
            .and_then(|user| user.get("id"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_else(|| panic!("node `{}`: no user in the session", self.name))
            .to_owned();
        *self.admin_id.lock().unwrap_or_else(|p| p.into_inner()) = admin_id;
        *self.client.lock().unwrap_or_else(|p| p.into_inner()) = Some(admin);
    }

    /// The node's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The node's spec, as given to [`crate::Cluster::add_node`].
    #[must_use]
    pub fn spec(&self) -> &NodeSpec {
        &self.spec
    }

    /// The node's root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The node's journal folder: later tasks point their journal store at it; this harness
    /// only creates the directory.
    #[must_use]
    pub fn journal_dir(&self) -> PathBuf {
        let dir = self.root.join("journal");
        std::fs::create_dir_all(&dir)
            .unwrap_or_else(|e| panic!("cannot create the journal folder: {e}"));
        dir
    }

    /// The node's site name.
    #[must_use]
    pub fn site(&self) -> &str {
        &self.site
    }

    /// The node's actual PostgreSQL database name: the one `spot_cli::new_site_of` created for
    /// this site, where every document and (once a later task wires it) every journal and sync
    /// row live.
    #[must_use]
    pub fn db_name(&self) -> &str {
        &self.site_db
    }

    /// A plain driver connection to this node's own database (the site's database, not the
    /// scratch run-id database), for [`crate::read_conditions`], [`crate::read_audit`] and
    /// [`crate::fingerprint`].
    ///
    /// # Panics
    /// When the connection cannot be made: a read-only helper that cannot reach the node's own
    /// database is a test environment problem worth failing loudly on, not a value to hide
    /// behind a `Result` only to `unwrap` a line later.
    pub(crate) async fn raw_client(&self) -> tokio_postgres::Client {
        connect(&self.admin_url, &self.site_db).await
    }

    /// The administrator client, once the host has started. `None` while the node is stopped
    /// or killed.
    #[must_use]
    pub fn client(&self) -> Option<Client> {
        self.client
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The administrator's record id.
    #[must_use]
    pub fn admin_id(&self) -> String {
        self.admin_id
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    /// The node's API address, once started.
    #[must_use]
    pub fn api_addr(&self) -> Option<SocketAddr> {
        self.client().map(|client| client.addr())
    }

    /// The site's one-time first sign-in secret, printed once by `spot_cli::new_site_of`: kept
    /// for later tasks that need the operator API-key credential path, not used by this
    /// harness's own tests.
    #[must_use]
    pub fn admin_key(&self) -> &str {
        &self.admin_key
    }

    /// The node's internal sync-side listener address (what the proxy forwards to). Exposed
    /// for diagnostics; a peer dials [`Node::sync_addr`] instead.
    #[must_use]
    pub fn sync_upstream_addr(&self) -> SocketAddr {
        self.sync_upstream_addr
    }

    /// Ensures a loopback proxy exists for `peer` (creating it on first use), and returns it.
    async fn ensure_proxy(&self, peer: &str) {
        if self
            .proxies
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(peer)
        {
            return;
        }
        let candidate = Proxy::spawn(self.sync_upstream_addr).await;
        let mut proxies = self.proxies.lock().unwrap_or_else(|p| p.into_inner());
        proxies.entry(peer.to_owned()).or_insert(candidate);
    }

    /// The address `peer` dials to reach this node's (stand-in) sync listener: a proxy created
    /// just for that pair. A different peer gets a different proxy, so [`Node::partition_from`]
    /// on one pair never touches another's traffic.
    pub async fn sync_addr_for(&self, peer: &str) -> SocketAddr {
        self.ensure_proxy(peer).await;
        self.proxies.lock().unwrap_or_else(|p| p.into_inner())[peer].addr()
    }

    /// Whether the host is serving right now.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .is_some()
    }

    /// Writes one line to the node's log file (teardown diagnostics, lifecycle notes).
    pub fn note(&self, text: &str) {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
        {
            let _ = writeln!(file, "node {}: {text}", self.name);
        }
    }

    // ------------------------------------------------------------------- lifecycle

    /// Graceful stop: signals the host to drain, then waits for it to finish. The database and
    /// the root are untouched, so [`Node::start`] resumes with the same data.
    ///
    /// By the time this returns, the host's own serving loop has genuinely ended (`join`
    /// resolved): a fresh connection to the address it was serving on is refused. See
    /// `tests/harness_self.rs::stop_closes_the_old_address_but_kill_does_not_wait_to`.
    pub async fn stop(&mut self) {
        if let Some(stop) = self.stop.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let _ = stop.send(());
        }
        let running = self
            .running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some(running) = running {
            let _ = tokio::time::timeout(Duration::from_secs(15), running.join()).await;
        }
        *self.client.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.note("stopped (graceful, awaited)");
    }

    /// An un-awaited stop: drops this `Node`'s own reference to the host without waiting for
    /// it to drain, and returns at once.
    ///
    /// **This is not a forceful abort**, and the doc comment used to claim otherwise (fixed
    /// per the lead's review of this PR): `spot_cli::Running`/`ServerHandle` hold their
    /// server-loop task behind a private `JoinHandle` with no abort method, and the shutdown
    /// future this harness hands `spot_cli::start_with` resolves as soon as its `oneshot`
    /// sender is dropped (whether or not it was ever sent) — so the host starts the same
    /// graceful drain [`Node::stop`] triggers, just in the background, unobserved and
    /// unawaited by the caller. The one real, provable difference from [`Node::stop`] is that
    /// difference itself: a caller cannot wait for `kill`'s drain (there is nothing to await),
    /// while [`Node::stop`] guarantees the drain is complete, and the old address refused, by
    /// the time it returns.
    ///
    /// A true "no drain, every in-flight request lost instantly" crash model needs either a
    /// forceful abort primitive on `spot_cli::ServerHandle` (not in this task's owned paths)
    /// or hosting each node as a real, killable OS process instead of an in-process task (a
    /// materially larger redesign than this task's size). Both are recorded under `## Open
    /// questions` in this task's PR for the lead to schedule.
    pub fn kill(&mut self) {
        let _ = self.stop.lock().unwrap_or_else(|p| p.into_inner()).take();
        let running = self
            .running
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        drop(running);
        *self.client.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.note("killed (un-awaited stop, drains in the background — not a forceful abort)");
    }

    /// Starts the host again over the same database and root (the same identity). Panics if the
    /// node is already running: call [`Node::stop`] or [`Node::kill`] first.
    pub async fn start(&mut self) {
        assert!(
            !self.is_running(),
            "node `{}` is already running; stop or kill it first",
            self.name
        );
        self.boot_host().await;
        self.note("started");
    }

    /// Cuts this node's and `other`'s sync traffic in both directions, mid-batch if a transfer
    /// is in flight: only the proxy pair between these two nodes pauses (see
    /// [`Node::sync_addr_for`]), so a third node's traffic with either of them is untouched.
    /// See [`Node::heal`].
    pub async fn partition_from(&self, other: &Node) {
        self.ensure_proxy(other.name()).await;
        other.ensure_proxy(self.name()).await;
        self.proxies.lock().unwrap_or_else(|p| p.into_inner())[other.name()].pause();
        other.proxies.lock().unwrap_or_else(|p| p.into_inner())[self.name()].pause();
        self.note(&format!("partitioned from `{}` (pairwise)", other.name()));
    }

    /// Restores sync traffic a prior [`Node::partition_from`] cut, for this pair only.
    pub fn heal(&self, other: &Node) {
        if let Some(p) = self
            .proxies
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(other.name())
        {
            p.resume();
        }
        if let Some(p) = other
            .proxies
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(self.name())
        {
            p.resume();
        }
        self.note(&format!("healed with `{}`", other.name()));
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        if let Ok(mut stop) = self.stop.lock()
            && let Some(stop) = stop.take()
        {
            let _ = stop.send(());
        }
        if let Ok(mut shutdown) = self.sync_upstream_shutdown.lock()
            && let Some(shutdown) = shutdown.take()
        {
            let _ = shutdown.send(());
        }
        if keep() {
            if let Some(db) = self.db.take() {
                std::mem::forget(db);
            }
            println!(
                "L3_KEEP=1: kept node `{}` database {} and root {}",
                self.name,
                self.db_name(),
                self.root.display()
            );
            return;
        }
        self.db.take();
        drop_site_database(&self.admin_url, &self.site_db);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn keep() -> bool {
    std::env::var("L3_KEEP").as_deref() == Ok("1")
}

/// Drops `dbname` on a short-lived thread with its own runtime, the same shape
/// `spot_postgres::testing::TestDb`'s own `Drop` uses: `Node::drop` may run while a `#[tokio::
/// test]`'s runtime is already shutting down, so this cannot `block_on` the caller's runtime.
/// `new_site_of` creates the site's database directly (not through `TestDb`), so nothing else
/// removes it; best effort, like `TestDb`'s own sweep for whatever this misses.
fn drop_site_database(admin_url: &str, dbname: &str) {
    let admin_url = admin_url.to_owned();
    let dbname = dbname.to_owned();
    let job = std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return;
        };
        runtime.block_on(async move {
            let client = connect(&admin_url, "postgres").await;
            // `dbname` is this node's own site database name, never caller input.
            let _ = client
                .batch_execute(&format!(
                    "DROP DATABASE IF EXISTS \"{dbname}\" WITH (FORCE)"
                ))
                .await;
        });
    });
    let _ = job.join();
}

/// A plain `tokio_postgres` connection to `dbname` on the server named by `url` (an admin
/// connection string), driving its background I/O task on a spawned task. No TLS: every caller
/// here is a loopback scratch database for tests, the same assumption
/// `spot_postgres::testing::TestDb` makes for its own raw connections.
async fn connect(url: &str, dbname: &str) -> tokio_postgres::Client {
    let mut config: tokio_postgres::Config = url
        .parse()
        .unwrap_or_else(|e| panic!("`{url}` is not a usable postgres:// URL: {e}"));
    config.dbname(dbname);
    let (client, connection) = config
        .connect(tokio_postgres::NoTls)
        .await
        .unwrap_or_else(|e| panic!("cannot connect to database `{dbname}`: {e}"));
    let dbname = dbname.to_owned();
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            eprintln!("l3-journal-sync: connection to `{dbname}` ended: {error}");
        }
    });
    client
}

/// Removes the root a [`Node::create`] call created when it fails before the returned [`Node`]
/// owns it (a panic unwinds through this guard); disarmed (never constructed as armed) once
/// the node is built. Mirrors `harness/rust/l3-http`'s `fixture::RootGuard`.
struct RootGuard {
    path: PathBuf,
    armed: bool,
}

impl Drop for RootGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

/// A plain echo loop standing in for a real sync listener (none exists yet): enough for
/// [`Node::partition_from`] and [`Node::heal`] to prove the proxy mechanism against a real
/// node, without any journal or sync business logic.
async fn sync_echo_loop(listener: TcpListener, mut shutdown: oneshot::Receiver<()>) {
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((mut socket, _)) = accepted else { continue; };
                tokio::spawn(async move {
                    let mut buffer = [0u8; 4096];
                    loop {
                        match socket.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => {
                                if socket.write_all(&buffer[..n]).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                });
            }
            _ = &mut shutdown => return,
        }
    }
}

fn sanitize(name: &str) -> String {
    let mut out: String = name
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    if out.is_empty() {
        out.push_str("node");
    }
    out
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap_or_else(|e| panic!("cannot create {}: {e}", to.display()));
    for entry in
        std::fs::read_dir(from).unwrap_or_else(|e| panic!("cannot read {}: {e}", from.display()))
    {
        let entry = entry.unwrap_or_else(|e| panic!("cannot read a directory entry: {e}"));
        let target = to.join(entry.file_name());
        let file_type = entry
            .file_type()
            .unwrap_or_else(|e| panic!("cannot stat an entry: {e}"));
        if file_type.is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target)
                .unwrap_or_else(|e| panic!("cannot copy {}: {e}", entry.path().display()));
        }
    }
}

/// `seed::documents` builds a `project` body from `n`, with a title and no business meaning
/// beyond being distinct records a test can count.
pub(crate) fn seed_project_body(unique: &str, owner: &str) -> serde_json::Value {
    json!({
        "code": unique,
        "title": format!("l3-journal-sync seed {unique}"),
        "status": "Active",
        "owner": owner,
        "currency": "USD",
        "budget": "0.00",
    })
}
