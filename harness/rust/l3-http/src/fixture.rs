//! The fixture: everything a case needs, and the teardown that leaves nothing behind.
//!
//! One fixture is one **run**: a fresh PostgreSQL database, a temporary root directory, a site
//! with the platform and sample apps installed and seeded, the users of each sample role, and a
//! real host serving that site on an ephemeral loopback port.
//!
//! * The database is created by `spot_postgres::testing::TestDb`, whose name carries a UUIDv7 —
//!   the run id — and whose creation drops databases earlier runs leaked. [`Fixture::run`] is
//!   that id, printed at the start of the run and stamped on every note this fixture writes, so
//!   a leaked database or root is traceable to the run that made it.
//! * The root directory holds `common.toml`, the site's `site.toml` and a private copy of the
//!   sample app (so a case may change a definition and reload, without touching the repository).
//!   `common.toml` sets `[services.events] enabled = true`: the event stream defaults to off
//!   (WP-INT-02.T6, opt-in), and the events cases (`src/cases/events.rs`) need the routes
//!   mounted.
//! * The host is started through `spot_cli::start_with` — the same assembly `spot serve` runs —
//!   on `127.0.0.1:0`, so ports never collide between concurrent runs.
//! * Teardown is a drop guard: the root is removed and the database dropped when the fixture is
//!   dropped, including when a case panics; the run's log is copied to the artifacts directory
//!   first, so a failing run keeps the server log it exists for. `L3_KEEP=1` keeps the root
//!   and the database instead. A start failure removes the root as well, and a root an earlier
//!   run leaked is swept at the start of the next one, like the databases.

use crate::failure;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};
use spot_auth::Password;
use spot_cli::SiteKind;
use spot_login::LoginConfig;
use spot_postgres::testing::TestDb;
use spot_sessions::SessionPolicy;
use spot_sessions::throttle::ThrottleConfig;

use crate::client::Client;
use crate::report::Failure;

/// The sample app's manager role (holds every right on the five DocTypes).
pub const MANAGER: &str = "Project Manager";
/// The sample app's member role (reads all, writes Tasks and their own Time Entries).
pub const MEMBER: &str = "Team Member";
/// The sample app's read-only role.
pub const VIEWER: &str = "Viewer";
/// A signed-in user with no role at all: every DocType denies it.
pub const OBSERVER: &str = "Observer";
/// The invited user that still holds its one-time first sign-in secret.
pub const INVITED: &str = "Invited";
/// A role-less user the lockout cases may lock without disturbing another case's sign-in.
pub const THROTTLED: &str = "Throttled";
/// The Administrator's password after the fixture completes the first sign-in.
pub const ADMIN_PASSWORD: &str = "violet-lantern-staple-9-Quartz";

/// The sample roles the fixture creates, in the order it creates them.
pub const ROLES: [&str; 3] = [MANAGER, MEMBER, VIEWER];

/// One user of the site.
#[derive(Debug, Clone)]
pub struct User {
    /// The user's immutable record id (what `created_by` and owner rules compare).
    pub id: String,
    /// The sign-in name (the e-mail address).
    pub email: String,
    /// A password set with the operator API (`None` for the invited user that still holds its
    /// one-time first sign-in secret).
    pub password: Option<String>,
    /// The one-time secret, when the user was never given a password.
    pub secret: Option<String>,
}

/// Everything a case needs from one run of the stack.
pub struct Fixture {
    run: String,
    root: PathBuf,
    site: String,
    admin_key: String,
    client: Client,
    admin: Client,
    admin_id: String,
    users: BTreeMap<&'static str, User>,
    role_ids: BTreeMap<&'static str, String>,
    log: PathBuf,
    counter: AtomicUsize,
    db: Option<TestDb>,
    running: Mutex<Option<spot_cli::Running>>,
    stop: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
}

impl Fixture {
    /// Creates a database, a site, its apps, its users and a running host, or `None` when no
    /// PostgreSQL is configured (`SKIPPED` is printed, and `SPOT_REQUIRE_PG=1` turns that into a
    /// panic inside `TestDb::create` instead).
    pub async fn start() -> Option<Fixture> {
        let db = TestDb::create().await?;
        let run = db.name().trim_start_matches("spot_test_").to_owned();
        sweep_roots();
        let root = std::env::temp_dir().join(format!("l3_http_{run}"));
        std::fs::create_dir_all(&root).expect("cannot create the fixture root");
        // A start failure used to leak this root forever: the database had a drop guard and a
        // sweep, the root had neither. The guard removes it on any early return or panic until
        // the fixture itself owns it.
        let guard = RootGuard {
            path: root.clone(),
            armed: !keep(),
        };
        let site = format!("l3-http-{}", &run[..12]);
        let repo = repo();
        // A private copy of the sample app: a case may add a field to a DocType and reload the
        // site without touching the repository's copy.
        copy_dir(&repo.join("apps/sample"), &root.join("sample"));
        patch_sample_gaps(&root.join("sample"));
        // The catalogue lists the platform app from the repository and the fixture's own copy of
        // the sample app — the same shape `apps/sample/README.md` documents. The platform app
        // *must* come from the catalogue: the copy embedded in the binary carries 11 DocTypes and
        // does not include `file`, which `project.attachments` needs (an `attach` field compiles
        // to a link to `file`), so a site built from the embedded set cannot compile the sample
        // app's metadata.
        let url = std::env::var("SPOT_TEST_PG_URL").expect("SPOT_TEST_PG_URL disappeared");
        std::fs::write(
            root.join("common.toml"),
            format!(
                "apps = [{:?}, \"sample\"]\n\
                 [server]\n\
                 bind = [\"127.0.0.1:0\"]\n\
                 shutdown_grace = 5\n\
                 [postgres]\n\
                 admin_url = {url:?}\n\
                 pool_max_size = 48\n\
                 [services.events]\n\
                 enabled = true\n\
                 [trust]\n\
                 require_signed = false\n",
                repo.join("apps/platform").display().to_string()
            ),
        )
        .expect("cannot write common.toml");
        let log = root.join("l3-http.log");

        // 1. The site, host-wide: the platform app plus every app in the catalogue.
        let secret = spot_cli::new_site_of(&root, &site, "en", SiteKind::HostWide)
            .await
            .unwrap_or_else(|e| panic!("new-site `{site}`: {e}"));
        let admin_key = secret.text().to_owned();
        // The Administrator's first sign-in is completed with the operator API before the host
        // starts: the seeded user is `invited`, and the sample app's `owner` lookup
        // (`enabled_only = true`) only accepts an active user. Completing it also gives the
        // fixture a session, which is the documented script path (`spot user set-password`).
        spot_cli::users::set_password(
            &root,
            &site,
            "administrator",
            &Password::new(ADMIN_PASSWORD),
        )
        .await
        .unwrap_or_else(|e| panic!("set-password administrator: {e}"));
        // 2. The sample app's demo data (projects, tasks, labels, draft time entries).
        let output = spot_cli::ops::install_app(
            &root,
            "sample",
            None,
            spot_cli::ops::Install {
                apply: true,
                seed: true,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("install-app sample: {e}"));
        assert!(
            output.contains("seeded"),
            "the sample app's seed did not run; install-app said: {output}"
        );
        // 3. The users, before the host starts: `set-password` touches the reload marker, and a
        //    marker written before start is part of the host's baseline (no reload race).
        let users = make_users(&root, &site).await;
        // 4. Serve.
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let running = spot_cli::start_with(&root, false, options(), async {
            let _ = stopped.await;
        })
        .await
        .unwrap_or_else(|e| panic!("the host did not start: {e}"));
        let addr = *running
            .local_addrs()
            .first()
            .expect("the host bound no address");
        let client = Client::new(addr, site.clone());
        let admin = client.clone();
        let mut fixture = Fixture {
            run,
            root,
            site,
            admin_key,
            client,
            admin,
            admin_id: String::new(),
            users,
            role_ids: BTreeMap::new(),
            log,
            counter: AtomicUsize::new(0),
            db: Some(db),
            running: Mutex::new(Some(running)),
            stop: Mutex::new(Some(stop)),
        };
        // The fixture owns the root from here: its drop guard removes it (or `L3_KEEP=1`
        // keeps it), so the start guard must not.
        std::mem::forget(guard);
        fixture.note(&format!(
            "run {}: site `{}`, root {}, host {}",
            fixture.run,
            fixture.site,
            fixture.root.display(),
            fixture.addr()
        ));
        fixture.wait_ready().await;
        // 5. The Administrator's session, signed in with the password set above.
        let admin = fixture
            .client
            .sign_in("administrator", ADMIN_PASSWORD)
            .await
            .unwrap_or_else(|e| panic!("the Administrator's sign-in: {e}"));
        fixture.admin = admin;
        let session = fixture
            .admin()
            .session()
            .await
            .unwrap_or_else(|e| panic!("the Administrator's session: {e}"));
        session.expect(200).unwrap_or_else(|e| panic!("{e}"));
        fixture.admin_id = session
            .data()
            .get("user")
            .and_then(|user| user.get("id"))
            .and_then(Json::as_str)
            .unwrap_or_else(|| panic!("no user in the session: {}", session.text()))
            .to_owned();
        // 6. The sample roles and their assignments (the roles are site records, not bootstrap
        //    rows: the platform seeds only its own reference roles).
        fixture
            .create_roles_and_assign()
            .await
            .unwrap_or_else(|e| panic!("the sample roles: {e}"));
        fixture.note("fixture ready");
        Some(fixture)
    }

    /// Polls `/api/health/ready` until the site answers 200 (30 s budget).
    async fn wait_ready(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(reply) = self.client.health("ready").await
                && reply.status == 200
            {
                self.note("host is ready");
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "the host did not become ready within 30 s; log: {}",
                    self.log.display()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Creates the three sample roles as site records and assigns each user its role.
    async fn create_roles_and_assign(&mut self) -> Result<(), Failure> {
        for role in ROLES {
            let reply = self
                .admin()
                .create("role", &json!({ "role_name": role, "origin": "reference" }))
                .await?;
            reply.expect(201)?;
            self.role_ids.insert(role, reply.id()?);
        }
        for role in ROLES {
            let user = self.user(role).id.clone();
            let role_id = self.role_ids[role].clone();
            let reply = self
                .admin()
                .create(
                    "role_assignment",
                    &json!({
                        "user": user,
                        "role": role_id,
                        "source": "manual",
                        "state": "active",
                        "reason": "Assigned by the l3-http fixture.",
                    }),
                )
                .await?;
            reply.expect(201)?;
        }
        // A link value must name a record the writer may read (WP-DOC-06.T2: an unreadable
        // target is reported exactly as a missing one). A Team Member creates Tasks that link a
        // Project (the sample's Task step then fills `project_owner`, a User), and a Project
        // Manager sets a Project's `owner` (a User). So the Team Member also holds `Viewer`
        // (reads projects) and the platform's `User Manager` (the only role that reads users),
        // and the Project Manager holds `User Manager`. Neither adds a right on the sample
        // DocTypes beyond reading Projects, which no permission case pins as denied.
        let roles = self.admin().list("role", "limit=100").await?;
        roles.expect(200)?;
        let user_manager = roles
            .rows()?
            .iter()
            .find(|r| r.get("role_name") == Some(&json!("User Manager")))
            .and_then(|r| r.get("id").and_then(|i| i.as_str()))
            .map(str::to_owned)
            .ok_or_else(|| failure!("the platform seeds no `User Manager` role"))?;
        for (who, role_id) in [
            (MEMBER, self.role_ids[VIEWER].clone()),
            (MEMBER, user_manager.clone()),
            (MANAGER, user_manager),
        ] {
            let reply = self
                .admin()
                .create(
                    "role_assignment",
                    &json!({
                        "user": self.user(who).id.clone(),
                        "role": role_id,
                        "source": "manual",
                        "state": "active",
                        "reason": "Read access for the links the cases write.",
                    }),
                )
                .await?;
            reply.expect(201)?;
        }
        Ok(())
    }

    /// The site's name.
    pub fn site(&self) -> &str {
        &self.site
    }

    /// The run id, derived from the database name's UUIDv7.
    pub fn run(&self) -> &str {
        &self.run
    }

    /// The fixture's root directory (removed at teardown unless `L3_KEEP=1`).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The fixture's log file.
    pub fn log(&self) -> &Path {
        &self.log
    }

    /// The host's bound address.
    pub fn addr(&self) -> SocketAddr {
        self.client.addr()
    }

    /// The run's scratch database, for the end-state invariants the API does not expose
    /// (the outbox). It is dropped with the fixture.
    pub fn db(&self) -> &TestDb {
        self.db.as_ref().expect("the fixture database is live")
    }

    /// The site `Host` header value.
    pub fn host(&self) -> &str {
        self.client.host()
    }

    /// The anonymous visitor.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The Administrator, authenticated with a session (the documented script path:
    /// `spot user set-password`, then a normal sign-in).
    pub fn admin(&self) -> Client {
        self.admin.clone()
    }

    /// The site's one-time first sign-in secret, for the API-key credential cases.
    pub fn admin_key(&self) -> &str {
        &self.admin_key
    }

    /// The number of engine calls the host has served (the unauthenticated case reads it).
    pub fn engine_ops(&self) -> usize {
        self.running
            .lock()
            .expect("fixture mutex")
            .as_ref()
            .map(spot_cli::Running::engine_ops)
            .unwrap_or(0)
    }

    /// Signals the host to shut down, without waiting: a case can watch the drain.
    pub fn begin_shutdown(&self) {
        if let Some(stop) = self.stop.lock().expect("fixture mutex").take() {
            let _ = stop.send(());
        }
    }

    /// Waits for the host's server loop to finish after [`Fixture::begin_shutdown`].
    pub async fn join_host(&self) {
        let running = self.running.lock().expect("fixture mutex").take();
        if let Some(running) = running {
            let _ = tokio::time::timeout(Duration::from_secs(15), running.join()).await;
        }
    }

    /// The Administrator's record id.
    pub fn admin_id(&self) -> &str {
        &self.admin_id
    }

    /// A role's record id.
    pub fn role_id(&self, role: &str) -> &str {
        &self.role_ids[role]
    }

    /// A user by role name (or [`OBSERVER`] / [`INVITED`]).
    pub fn user(&self, role: &str) -> &User {
        self.users
            .get(role)
            .unwrap_or_else(|| panic!("no fixture user for role `{role}`"))
    }

    /// Signs a user in and returns a client carrying the session.
    pub async fn sign_in(&self, role: &str) -> Result<Client, Failure> {
        let user = self.user(role);
        let password = user
            .password
            .as_deref()
            .ok_or_else(|| Failure::new(format!("{} has no password", user.email)))?;
        self.client.sign_in(&user.email, password).await
    }

    /// A fresh value unique within the run, for names, codes and subjects.
    pub fn unique(&self, prefix: &str) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{}-{n}", &self.run[..6])
    }

    /// Writes one line to the fixture log (teardown diagnostics, case notes).
    pub fn note(&self, text: &str) {
        use std::io::Write as _;
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
        {
            let _ = writeln!(file, "run {}: {text}", self.run);
        }
        if std::env::var("L3_VERBOSE").as_deref() == Ok("1") {
            println!("run {}: {text}", self.run);
        }
    }

    // ------------------------------------------------------------ data helpers

    /// Creates a Project as the Administrator and returns `(id, version, code)`.
    pub async fn project(&self, title: &str) -> Result<(String, i64, String), Failure> {
        let code = self.unique("PRJ");
        let reply = self
            .admin()
            .create(
                "project",
                &json!({
                    "code": code,
                    "title": title,
                    "status": "Active",
                    "owner": self.admin_id,
                    "currency": "USD",
                    "budget": "1250.00",
                }),
            )
            .await?;
        reply.expect(201)?;
        Ok((reply.id()?, reply.version()?, code))
    }

    /// Creates a Task under `project` as the Administrator and returns `(id, version)`.
    pub async fn task(&self, project: &str, subject: &str) -> Result<(String, i64), Failure> {
        let reply = self
            .admin()
            .create(
                "task",
                &json!({
                    "subject": subject,
                    "project": project,
                    "status": "Open",
                }),
            )
            .await?;
        reply.expect(201)?;
        Ok((reply.id()?, reply.version()?))
    }

    /// Creates a draft Time Entry as the Administrator for `hours` and returns `(id, version)`.
    pub async fn time_entry(
        &self,
        task: &str,
        hours: &str,
        date: &str,
    ) -> Result<(String, i64), Failure> {
        let reply = self
            .admin()
            .create(
                "time_entry",
                &json!({
                    "task": task,
                    "person": self.admin_id,
                    "created_by": self.admin_id,
                    "work_date": date,
                    "hours": hours,
                }),
            )
            .await?;
        reply.expect(201)?;
        Ok((reply.id()?, reply.version()?))
    }

    // --------------------------------------------------------------- lifecycle

    /// Stops the host, then drops the database and the root directory (unless `L3_KEEP=1`).
    /// Idempotent: the suite calls it at the end and the drop guard calls it again.
    pub async fn finish(&mut self) {
        if let Some(stop) = self.stop.lock().expect("fixture mutex").take() {
            let _ = stop.send(());
        }
        let running = self.running.lock().expect("fixture mutex").take();
        if let Some(running) = running {
            let _ = tokio::time::timeout(Duration::from_secs(15), running.join()).await;
        }
        self.note("host stopped");
        if keep() {
            let name = self.db.as_ref().map(|db| db.name().to_owned());
            println!(
                "L3_KEEP=1: kept database {} and root {}",
                name.as_deref().unwrap_or("<none>"),
                self.root.display()
            );
            if let Some(db) = self.db.take() {
                std::mem::forget(db);
            }
            return;
        }
        self.preserve_log();
        self.db.take();
        let _ = std::fs::remove_dir_all(&self.root);
    }

    /// Whether the fixture is still serving (the shutdown case stops it).
    pub fn is_running(&self) -> bool {
        self.running.lock().expect("fixture mutex").is_some()
    }

    /// Copies this run's log into the artifacts directory (`L3_ARTIFACTS`, default a directory
    /// under the system temp directory, never the repository) while the root still exists, so a
    /// failing run keeps its server log after teardown. Returns the copy's path when the copy
    /// happened.
    pub fn preserve_log(&self) -> Option<PathBuf> {
        let directory = artifacts_dir().join("server-logs");
        std::fs::create_dir_all(&directory).ok()?;
        let name = self.root.file_name()?.to_string_lossy().into_owned();
        let target = directory.join(format!("{name}.log"));
        std::fs::copy(&self.log, &target).ok().map(|_| target)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Ok(mut stop) = self.stop.lock()
            && let Some(stop) = stop.take()
        {
            let _ = stop.send(());
        }
        if keep() {
            if let Some(db) = self.db.take() {
                std::mem::forget(db);
            }
            return;
        }
        self.preserve_log();
        self.db.take();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn keep() -> bool {
    std::env::var("L3_KEEP").as_deref() == Ok("1")
}

/// Removes the run root when the fixture start fails before a [`Fixture`] owns it.
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

/// Roots older than this are removed by [`sweep_roots`] (the same hour `TestDb::create` gives
/// a leaked database). A concurrent run's root is never this old.
const ROOT_SWEEP_AFTER: Duration = Duration::from_secs(60 * 60);

/// Removes roots an earlier run left behind when it was killed before its drop guard ran
/// (SIGKILL, a CI timeout), so `/tmp/l3_http_<run>` does not accumulate forever.
fn sweep_roots() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with("l3_http_") {
            continue;
        }
        let old = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > ROOT_SWEEP_AFTER);
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Where a failing run's log is copied: `L3_ARTIFACTS` (set by the CI runners, see
/// `tools/ci/e2e/run.sh`) when set, or a directory under the system temp directory otherwise.
/// Never the repository: a plain local run must leave nothing for `git add -A` to pick up.
fn artifacts_dir() -> PathBuf {
    artifacts_dir_for(std::env::var("L3_ARTIFACTS").ok())
}

/// The testable core of [`artifacts_dir`]: takes the `L3_ARTIFACTS` value as a parameter instead
/// of reading the environment, so tests can exercise both branches without racing each other
/// over the shared process environment.
fn artifacts_dir_for(l3_artifacts: Option<String>) -> PathBuf {
    l3_artifacts
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("spot-l3-artifacts"))
}

fn options() -> spot_cli::Options {
    spot_cli::Options {
        login: LoginConfig {
            // The parallel proof holds 200 sessions of one account at once; the shipped bound
            // (8) would evict the oldest and turn the proof into a different test.
            session_policy: SessionPolicy {
                max_per_user: 256,
                ..SessionPolicy::DEFAULT
            },
            // A lockout must be provable in a handful of requests, not in minutes, and the
            // backoff envelope must name a whole second (`retry_after` is rendered in seconds).
            throttle: ThrottleConfig {
                backoff_after: 2,
                lockout_after: 3,
                window: Duration::from_secs(60),
                backoff_base: Duration::from_secs(1),
                backoff_multiplier: 1,
                backoff_cap: Duration::from_secs(1),
                lockout_duration: Duration::from_secs(60),
            },
            // Every request re-reads the credential, so a changed password applies at once.
            cache_ttl: Duration::ZERO,
            ..LoginConfig::for_tests()
        },
        clock: None,
    }
}

/// The repository root: this crate lives at `harness/rust/l3-http`.
pub fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("cannot create a directory in the fixture root");
    for entry in std::fs::read_dir(from).expect("cannot read an app directory") {
        let entry = entry.expect("cannot read a directory entry");
        let target = to.join(entry.file_name());
        if entry
            .file_type()
            .expect("cannot stat an app entry")
            .is_dir()
        {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).expect("cannot copy an app file");
        }
    }
}

/// Fills the two gaps the shipped sample app leaves in its own declared permission model, on the
/// fixture's private copy only (the repository's `apps/sample` is not touched):
///
/// 1. `task.details` is documented as a member of the `confidential` protection group and
///    `permissions/field_grants.toml` grants that group to Project Managers, but `task.toml`
///    declares neither the group nor the membership — so the field stays readable by everyone.
/// 2. `permissions/grants.toml` gives Team Members `if_owner` rights on `time_entry`, and the
///    owner rule reads the document's `created_by` field, which `time_entry.toml` does not
///    declare — so the owner rule can never match.
///
/// Both gaps are reported under `## Open questions` (they belong to `apps/sample`); the fixture
/// fills them so the declared model is exercised over the real stack instead of being reduced to
/// a comment. `spot-cli`'s own end-to-end test fills the same two gaps the same way.
fn patch_sample_gaps(sample: &Path) {
    let task = sample.join("doctypes/task.toml");
    let text = std::fs::read_to_string(&task).expect("task.toml");
    let text = text
        .replace(
            "title_field = \"subject\"",
            "title_field = \"subject\"\n\n[[protection_groups]]\nname = \"confidential\"\nlabel = \"Confidential\"\ntrace = true",
        )
        .replace(
            "name = \"details\"\nlabel = \"Details\"\ntype = \"long_text\"",
            "name = \"details\"\nlabel = \"Details\"\ntype = \"long_text\"\nprotection_group = \"confidential\"",
        );
    std::fs::write(&task, text).expect("cannot write task.toml");

    let entry = sample.join("doctypes/time_entry.toml");
    let text = std::fs::read_to_string(&entry).expect("time_entry.toml")
        + "\n[[fields]]\nname = \"created_by\"\nlabel = \"Created By\"\ntype = \"text\"\nmax_chars = 64\n";
    std::fs::write(&entry, text).expect("cannot write time_entry.toml");
}

/// Creates the five users, before the host starts. Each add-user call prints a one-time secret
/// once; `set-password` replaces it with a password the cases can type.
async fn make_users(root: &Path, site: &str) -> BTreeMap<&'static str, User> {
    let mut users = BTreeMap::new();
    for (role, email, password) in [
        (MANAGER, "manager@l3.test", "quartz-lantern-47-harbor"),
        (MEMBER, "member@l3.test", "saffron-meadow-pilot-3-cedar"),
        (VIEWER, "viewer@l3.test", "cobalt-harbor-mango-8-pebble"),
        (OBSERVER, "observer@l3.test", "amber-thicket-vessel-6-quill"),
        (
            THROTTLED,
            "throttled@l3.test",
            "copper-orchard-lantern-5-ridge",
        ),
    ] {
        users.insert(role, invite(root, site, email, Some(password)).await);
    }
    users.insert(INVITED, invite(root, site, "invited@l3.test", None).await);
    users
}

async fn invite(root: &Path, site: &str, email: &str, password: Option<&str>) -> User {
    let invitation = spot_cli::users::add_user(root, site, email, None, &[])
        .await
        .unwrap_or_else(|e| panic!("add-user {email}: {e}"));
    let id = invitation.user.to_string();
    let secret = invitation.secret.text().to_owned();
    match password {
        Some(password) => {
            spot_cli::users::set_password(root, site, email, &Password::new(password))
                .await
                .unwrap_or_else(|e| panic!("set-password {email}: {e}"));
            User {
                id,
                email: email.to_owned(),
                password: Some(password.to_owned()),
                secret: None,
            }
        }
        None => User {
            id,
            email: email.to_owned(),
            password: None,
            secret: Some(secret),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch directory outside the fixture naming scheme.
    fn scratch(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "l3_http_fixture_test_{label}_{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("inner")).expect("a scratch directory");
        path
    }

    #[test]
    fn a_start_failure_guard_removes_the_root_it_created() {
        let path = scratch("guard");
        {
            let _guard = RootGuard {
                path: path.clone(),
                armed: true,
            };
        }
        assert!(!path.exists(), "the guard left {} behind", path.display());
    }

    #[test]
    fn a_disarmed_guard_leaves_the_root_for_the_fixture() {
        let path = scratch("disarmed");
        {
            let _guard = RootGuard {
                path: path.clone(),
                armed: false,
            };
        }
        assert!(path.exists(), "the guard removed a kept root");
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    fn a_fresh_root_is_not_swept() {
        // A concurrent run's root is fresh: the sweep must never remove it.
        let path = std::env::temp_dir().join(format!("l3_http_sweep_fresh_{}", std::process::id()));
        std::fs::create_dir_all(&path).expect("a fresh root");
        sweep_roots();
        assert!(path.exists(), "the sweep removed a fresh root");
        let _ = std::fs::remove_dir_all(&path);
    }

    #[test]
    #[cfg(unix)]
    fn a_root_an_earlier_run_leaked_is_swept() {
        let path = std::env::temp_dir().join(format!("l3_http_sweep_old_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("inner")).expect("a leaked root");
        // Backdate the directory: `set_modified` needs a file handle, which a directory has
        // only on Unix (the sweep itself runs on every platform).
        std::fs::File::open(&path)
            .expect("the root")
            .set_modified(std::time::SystemTime::now() - ROOT_SWEEP_AFTER - Duration::from_secs(60))
            .expect("backdate the root");
        sweep_roots();
        assert!(!path.exists(), "the sweep kept {}", path.display());
    }

    #[test]
    fn artifacts_dir_defaults_outside_the_repository() {
        // `None` means `L3_ARTIFACTS` is unset; the default must never be inside the
        // repository, or a plain local run leaves untracked files `git add -A` picks up.
        let dir = artifacts_dir_for(None);
        let repo = repo();
        assert!(
            !dir.starts_with(&repo),
            "the default artifacts dir {} is inside the repository {}",
            dir.display(),
            repo.display()
        );
    }

    #[test]
    fn artifacts_dir_honors_the_override() {
        let dir = artifacts_dir_for(Some("/tmp/l3-http-fixture-test-override".to_owned()));
        assert_eq!(dir, PathBuf::from("/tmp/l3-http-fixture-test-override"));
    }
}
