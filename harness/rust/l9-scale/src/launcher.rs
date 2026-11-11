//! The process launcher: a real `spot serve` on a real PostgreSQL database.
//!
//! It creates a scratch site with the platform and sample apps (the same `spot` commands an
//! operator runs: `new-site`, `install-app`, `add-user`, `set-password`), starts `spot serve`
//! as a child process on a loopback port, waits for `/api/health/ready`, gives the simulated
//! users a role through the Administrator's session, and tears everything down. The
//! PostgreSQL server is the one `SPOT_TEST_PG_URL` names (the CI service, or a local one).

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_postgres::testing::TestDb;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

use crate::http::{Conn, Target};
use crate::runner::{BoxFuture, ResourceSample, Sampler};
use crate::session::{Secret, SimUser};

/// How to launch.
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// The repository root (holds `apps/platform` and `apps/sample`).
    pub repo: PathBuf,
    /// The `spot` binary; found next to this program, or built with cargo, when `None`.
    pub spot_bin: Option<PathBuf>,
    /// Simulated users to create.
    pub users: u32,
    /// The run id (it names the site and the database).
    pub run_id: String,
    /// Keep the site directory and the database after the run (for a post-mortem).
    pub keep: bool,
}

/// A running scratch deployment.
pub struct Launched {
    /// Where the load goes.
    pub target: Target,
    /// The simulated users, each with a password only this process knows.
    pub users: Vec<SimUser>,
    db: Arc<TestDb>,
    database: String,
    root: PathBuf,
    log: PathBuf,
    child: Child,
    keep: bool,
}

/// The repository root this crate was built in: `harness/rust/l9-scale/../../..`.
pub fn default_repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

/// A random lowercase hex string of `len` characters, from the standard library's per-process
/// random hasher keys (no clock, no extra crate).
pub fn random_hex(len: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    let mut counter = 0u64;
    while out.len() < len {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(counter);
        counter += 1;
        out.push_str(&format!("{:016x}", hasher.finish()));
    }
    out.truncate(len);
    out
}

/// Refuses a run id that is not 1 to 40 lowercase letters or digits. The id becomes part of a
/// database name (inside a SQL identifier), a site name and a directory that is removed at
/// teardown, so it must never carry a quote, a slash or a dot.
pub fn validate_run_id(run_id: &str) -> Result<(), String> {
    let ok = !run_id.is_empty()
        && run_id.len() <= 40
        && run_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if ok {
        Ok(())
    } else {
        Err("--run-id must be 1 to 40 lowercase letters or digits".to_owned())
    }
}

/// A random password that meets the platform's policy (long, mixed) and is never stored.
pub fn random_password() -> String {
    format!("Lx-{}-Qz9", random_hex(32))
}

fn free_port() -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| format!("no free port: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("no local address: {e}"))?
        .port();
    Ok(port)
}

fn find_in_target(exe: &Path) -> Option<PathBuf> {
    let name = format!("spot{}", std::env::consts::EXE_SUFFIX);
    exe.ancestors()
        .skip(1)
        .take(3)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
}

/// Finds (or builds) the `spot` binary.
pub async fn locate_spot(repo: &Path, given: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(path) = given {
        return Ok(path.to_owned());
    }
    if let Ok(path) = std::env::var("SPOT_BIN")
        && !path.is_empty()
    {
        return Ok(PathBuf::from(path));
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this program: {e}"))?;
    if let Some(found) = find_in_target(&exe) {
        return Ok(found);
    }
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let status = Command::new(cargo)
        .args(["build", "-p", "spot-cli", "--bin", "spot"])
        .arg("--manifest-path")
        .arg(repo.join("Cargo.toml"))
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .await
        .map_err(|e| format!("cannot run cargo to build the `spot` binary: {e}"))?;
    if !status.success() {
        return Err("`cargo build -p spot-cli --bin spot` failed".to_owned());
    }
    find_in_target(&exe)
        .ok_or_else(|| "the `spot` binary is not next to this program; set SPOT_BIN".to_owned())
}

/// Runs one `spot` command to completion and returns its standard output. `stdin` is written
/// to the child (a password), and is never part of an error.
async fn spot(
    bin: &Path,
    root: &Path,
    args: &[&str],
    stdin: Option<&str>,
) -> Result<String, String> {
    // Commands that change a site take its lock file and refuse when another holds it. The
    // user setup runs several at once, so a refusal for that reason waits and tries again.
    let mut attempts = 0u32;
    loop {
        match spot_once(bin, root, args, stdin).await {
            Err(error) if attempts < 200 && error.contains("another spot command is changing") => {
                attempts += 1;
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            other => return other,
        }
    }
}

async fn spot_once(
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
        // Standard output of `new-site` and `add-user` carries one-time secrets: only the
        // error stream, which never does, goes into the message.
        return Err(format!(
            "`spot {}` failed: {}",
            args.first().copied().unwrap_or(""),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    std::fs::create_dir_all(to).map_err(|e| format!("cannot create {}: {e}", to.display()))?;
    let entries =
        std::fs::read_dir(from).map_err(|e| format!("cannot read {}: {e}", from.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", from.display()))?;
        let target = to.join(entry.file_name());
        let is_dir = entry.file_type().map_err(|e| e.to_string())?.is_dir();
        if is_dir {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)
                .map_err(|e| format!("cannot copy {}: {e}", entry.path().display()))?;
        }
    }
    Ok(())
}

impl Launched {
    /// Starts the deployment, or returns `None` when no PostgreSQL is configured
    /// (`SPOT_TEST_PG_URL` unset; `TestDb` prints why).
    pub async fn start(options: &LaunchOptions) -> Result<Option<Launched>, String> {
        validate_run_id(&options.run_id)?;
        let Some(db) = TestDb::create().await else {
            return Ok(None);
        };
        let pg_url = std::env::var("SPOT_TEST_PG_URL")
            .map_err(|_| "SPOT_TEST_PG_URL vanished".to_owned())?;
        let bin = locate_spot(&options.repo, options.spot_bin.as_deref()).await?;
        let site = format!("l9-scale-{}", options.run_id);
        let database = site.replace('-', "_");
        let root = std::env::temp_dir().join(format!("l9_scale_{}", options.run_id));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)
            .map_err(|e| format!("cannot create {}: {e}", root.display()))?;
        let db = Arc::new(db);
        let result = Self::bring_up(options, &db, &bin, &site, &database, &root, &pg_url).await;
        match result {
            Ok(launched) => Ok(Some(launched)),
            Err(error) => {
                drop_database(&db, &database).await;
                if !options.keep {
                    let _ = std::fs::remove_dir_all(&root);
                }
                Err(error)
            }
        }
    }

    async fn bring_up(
        options: &LaunchOptions,
        db: &Arc<TestDb>,
        bin: &Path,
        site: &str,
        database: &str,
        root: &Path,
        pg_url: &str,
    ) -> Result<Launched, String> {
        copy_dir(&options.repo.join("apps/sample"), &root.join("sample"))?;
        let port = free_port()?;
        let platform = options.repo.join("apps/platform");
        std::fs::write(
            root.join("common.toml"),
            format!(
                "apps = [{:?}, \"sample\"]\n[server]\nbind = [\"127.0.0.1:{port}\"]\nshutdown_grace = 5\n\
                 [postgres]\nadmin_url = {pg_url:?}\n",
                platform.display().to_string()
            ),
        )
        .map_err(|e| format!("cannot write common.toml: {e}"))?;

        spot(bin, root, &["new-site", site], None).await?;
        let admin_password = random_password();
        spot(
            bin,
            root,
            &["set-password", site, "administrator", "--password-stdin"],
            Some(&admin_password),
        )
        .await?;
        spot(
            bin,
            root,
            &["install-app", "sample", "--site", site, "--apply", "--seed"],
            None,
        )
        .await?;

        // The simulated users, a few at a time: every command is a process with a database
        // connection of its own.
        let mut users: Vec<(String, String, String)> = Vec::new();
        let wanted: Vec<u32> = (0..options.users).collect();
        for batch in wanted.chunks(8) {
            let mut jobs = Vec::new();
            for index in batch {
                let (bin, root, site) = (bin.to_owned(), root.to_owned(), site.to_owned());
                let email = format!("l9-user-{index:05}@l9.test");
                let password = random_password();
                jobs.push(tokio::spawn(async move {
                    let said = spot(&bin, &root, &["add-user", &site, &email], None).await?;
                    let id = said
                        .split_whitespace()
                        .nth(1)
                        .ok_or_else(|| "add-user printed no user id".to_owned())?
                        .to_owned();
                    spot(
                        &bin,
                        &root,
                        &["set-password", &site, &email, "--password-stdin"],
                        Some(&password),
                    )
                    .await?;
                    Ok::<_, String>((email, password, id))
                }));
            }
            for job in jobs {
                users.push(
                    job.await
                        .map_err(|_| "a user setup task panicked".to_owned())??,
                );
            }
        }

        let log = root.join("spot-serve.log");
        let log_file = std::fs::File::create(&log)
            .map_err(|e| format!("cannot create the server log: {e}"))?;
        let log_err = log_file.try_clone().map_err(|e| e.to_string())?;
        let child = Command::new(bin)
            .arg("--root")
            .arg(root)
            .arg("serve")
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_err))
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("cannot start `spot serve`: {e}"))?;
        let target = Target {
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            host: site.to_owned(),
        };
        let mut launched = Launched {
            target: target.clone(),
            users: Vec::new(),
            db: db.clone(),
            database: database.to_owned(),
            root: root.to_owned(),
            log,
            child,
            keep: options.keep,
        };
        if let Err(error) = launched.wait_ready().await {
            launched.stop().await;
            return Err(error);
        }
        let ids: BTreeMap<String, String> = users
            .iter()
            .map(|(e, _, id)| (e.clone(), id.clone()))
            .collect();
        if let Err(error) = grant_roles(&target, &admin_password, &ids).await {
            launched.stop().await;
            return Err(error);
        }
        launched.users = users
            .into_iter()
            .map(|(email, password, _)| SimUser {
                email,
                password: Secret::new(password),
            })
            .collect();
        Ok(launched)
    }

    async fn wait_ready(&mut self) -> Result<(), String> {
        let mut conn = Conn::new();
        for _ in 0..600 {
            if let Ok(Some(status)) = self.child.try_wait() {
                return Err(format!(
                    "`spot serve` exited ({status}) before it was ready; log: {}",
                    self.log.display()
                ));
            }
            if let Ok(reply) = conn
                .request(
                    &self.target,
                    "GET",
                    "/api/health/ready",
                    &[],
                    None,
                    Duration::from_secs(2),
                )
                .await
                && reply.status == 200
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Err(format!(
            "the host was not ready within 60 s; log: {}",
            self.log.display()
        ))
    }

    /// A sampler of this deployment's server process and database.
    pub fn sampler(&self) -> Arc<dyn Sampler> {
        Arc::new(LaunchSampler {
            pid: self.child.id(),
            db: self.db.clone(),
            database: self.database.clone(),
        })
    }

    async fn stop(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }

    /// Stops the server, drops the run's database and removes the site directory (unless the
    /// run was started with `keep`).
    pub async fn shutdown(mut self) {
        self.stop().await;
        if self.keep {
            eprintln!(
                "l9-scale: kept {} and database {} (server log {})",
                self.root.display(),
                self.database,
                self.log.display()
            );
            return;
        }
        drop_database(&self.db, &self.database).await;
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

async fn drop_database(db: &TestDb, database: &str) {
    let client = db.admin_client().await;
    // `database` is `l9_scale_` and a run id that `validate_run_id` limited to [a-z0-9].
    let _ = client
        .batch_execute(&format!(
            "DROP DATABASE IF EXISTS \"{database}\" WITH (FORCE)"
        ))
        .await;
}

/// Gives every user the sample's `Project Manager` role (every right on the sample DocTypes)
/// and the platform's `User Manager` (the only role that reads users, which links to a user
/// need), through the Administrator's session.
async fn grant_roles(
    target: &Target,
    admin_password: &str,
    users: &BTreeMap<String, String>,
) -> Result<(), String> {
    let mut conn = Conn::new();
    let timeout = Duration::from_secs(30);
    let body = json!({ "user": "administrator", "password": admin_password }).to_string();
    let login = conn
        .request(
            target,
            "POST",
            "/api/v1/auth/login",
            &[],
            Some(body.as_bytes()),
            timeout,
        )
        .await
        .map_err(|e| format!("the Administrator's sign-in failed: {e:?}"))?;
    if login.status != 200 {
        return Err(format!(
            "the Administrator's sign-in answered HTTP {}",
            login.status
        ));
    }
    let token = login
        .json()
        .pointer("/data/token")
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "the Administrator's sign-in returned no token".to_owned())?;
    let bearer = format!("Bearer {token}");
    let manager = role_id(target, &bearer, "Project Manager", true).await?;
    let user_manager = role_id(target, &bearer, "User Manager", false).await?;
    for id in users.values() {
        for role in [&manager, &user_manager] {
            let reply = api(
                target,
                &bearer,
                "/api/v1/resource/role_assignment",
                &json!({
                    "user": id, "role": role, "source": "manual", "state": "active",
                    "reason": "Assigned by the l9-scale launcher."
                }),
            )
            .await?;
            if reply.status != 201 {
                return Err(format!("a role assignment answered HTTP {}", reply.status));
            }
        }
    }
    Ok(())
}

/// One authenticated POST as the Administrator.
async fn api(
    target: &Target,
    bearer: &str,
    path: &str,
    body: &Json,
) -> Result<crate::http::Response, String> {
    let bytes = body.to_string().into_bytes();
    Conn::new()
        .request(
            target,
            "POST",
            path,
            &[("Authorization", bearer)],
            Some(&bytes),
            Duration::from_secs(30),
        )
        .await
        .map_err(|e| format!("POST {path}: {e:?}"))
}

/// The id of the role named `name`, created when `create` and it does not exist yet.
async fn role_id(
    target: &Target,
    bearer: &str,
    name: &str,
    create: bool,
) -> Result<String, String> {
    let found = api(
        target,
        bearer,
        "/api/v1/resource/role/query",
        &json!({ "filter": { "field": "role_name", "op": "eq", "value": name }, "limit": 1 }),
    )
    .await?;
    if let Some(id) = found.json().pointer("/data/0/id").and_then(Json::as_str) {
        return Ok(id.to_owned());
    }
    if !create {
        return Err(format!("the site has no `{name}` role"));
    }
    let made = api(
        target,
        bearer,
        "/api/v1/resource/role",
        &json!({ "role_name": name, "origin": "reference" }),
    )
    .await?;
    made.json()
        .pointer("/data/id")
        .and_then(Json::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("cannot create the `{name}` role (HTTP {})", made.status))
}

struct LaunchSampler {
    pid: Option<u32>,
    db: Arc<TestDb>,
    database: String,
}

impl Sampler for LaunchSampler {
    fn sample(&self) -> BoxFuture<'_, ResourceSample> {
        Box::pin(async move {
            let mut sample = ResourceSample::default();
            if let Some(pid) = self.pid {
                sample.rss_kb = read_rss_kb(pid);
                sample.open_fds = count_fds(pid);
            }
            let client = self.db.admin_client().await;
            let connections = client
                .query_one(
                    "SELECT count(*) FROM pg_stat_activity WHERE datname = $1",
                    &[&self.database],
                )
                .await;
            sample.pg_connections = connections
                .ok()
                .and_then(|row| row.try_get::<_, i64>(0).ok())
                .and_then(|n| u64::try_from(n).ok());
            let waiting = client
                .query_one(
                    "SELECT count(*) FROM pg_locks l JOIN pg_stat_activity a ON a.pid = l.pid \
                     WHERE a.datname = $1 AND NOT l.granted",
                    &[&self.database],
                )
                .await;
            sample.pg_waiting_locks = waiting
                .ok()
                .and_then(|row| row.try_get::<_, i64>(0).ok())
                .and_then(|n| u64::try_from(n).ok());
            sample
        })
    }
}

fn read_rss_kb(pid: u32) -> Option<u64> {
    let text = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|kb| kb.parse().ok())
}

fn count_fds(pid: u32) -> Option<u64> {
    let entries = std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    Some(entries.count() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_id_cannot_carry_sql_or_a_path() {
        assert!(validate_run_id(&random_hex(12)).is_ok());
        assert!(validate_run_id("abc123").is_ok());
        for bad in [
            "",
            "A",
            "a\"; drop database x; --",
            "../x",
            "a/b",
            "a.b",
            "a-b",
            "a b",
            &"a".repeat(41),
        ] {
            assert!(validate_run_id(bad).is_err(), "{bad}");
        }
    }
}
