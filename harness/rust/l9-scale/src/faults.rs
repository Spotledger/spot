//! Fault injection while the platform is under load (WP-TST-15.T7): injectors that work on a CI
//! runner without privilege where they can and say why they were skipped where they cannot, a
//! ledger of acknowledged and unacknowledged writes, a readiness probe and the checks that
//! follow a fault.
//!
//! Included by `tests/faults.rs` with `#[path]` (not declared in `lib.rs`), so it names the
//! harness as `l9_scale::`. Nothing here reads a wall clock: callers pass a [`Clock`] and waits
//! are `tokio::time::sleep`. No float, no `unsafe`.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use l9_scale::clock::Clock;
use l9_scale::http::{Conn, HttpError, Target};
use serde_json::{Value as Json, json};
use spot_postgres::testing::TestDb;
use tokio::process::Command;

/// The request timeout of the load and the ledger writers.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The faults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// `pg_terminate_backend` on every connection of the site's database.
    PgConnectionKill,
    /// `pg_ctl restart -m fast` on the harness-owned PostgreSQL.
    PgRestart,
    /// The data directory's filesystem filled to 95 percent.
    DiskFull,
    /// The data device throttled through cgroup `io.max`.
    SlowDisk,
    /// The host's clock one minute ahead.
    ClockSkew,
}

impl Fault {
    /// The fault's name in output.
    pub fn name(self) -> &'static str {
        match self {
            Fault::PgConnectionKill => "pg_connection_kill",
            Fault::PgRestart => "pg_restart",
            Fault::DiskFull => "disk_full",
            Fault::SlowDisk => "slow_disk",
            Fault::ClockSkew => "clock_skew",
        }
    }

    /// Whether readiness must leave 200 while the fault lasts. A killed connection is absorbed
    /// by the pool; a restarted PostgreSQL is not.
    pub fn expects_not_ready(self) -> bool {
        matches!(self, Fault::PgRestart)
    }
}

/// What an injection did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Injected {
    /// The fault is in effect; heal it with [`heal`].
    Active,
    /// It cannot be injected here, and why.
    Skipped(String),
}

/// What [`heal`] needs to undo a fault.
#[derive(Debug, Clone, Default)]
pub struct Undo {
    filler: Option<PathBuf>,
    io_max: Option<(PathBuf, String)>,
}

/// The environment variables that switch on the injectors that need a prepared runner.
pub mod env {
    /// The data directory of the harness-owned PostgreSQL (for `pg_ctl restart`).
    pub const PG_DATA: &str = "L9_PG_CTL_DATA";
    /// The `pg_ctl` binary (default `pg_ctl`).
    pub const PG_CTL: &str = "L9_PG_CTL";
    /// A directory on the small filesystem the PostgreSQL data directory lives on.
    pub const FULL_DIR: &str = "L9_FAULT_FULL_DIR";
    /// The `io.max` file of the cgroup the PostgreSQL runs in.
    pub const IO_MAX_FILE: &str = "L9_FAULT_IO_MAX_FILE";
    /// The line to write to throttle, e.g. `8:0 rbps=1048576 wbps=1048576`.
    pub const IO_MAX_SLOW: &str = "L9_FAULT_IO_MAX_SLOW";
    /// The line to write to lift the throttle, e.g. `8:0 rbps=max wbps=max`.
    pub const IO_MAX_FAST: &str = "L9_FAULT_IO_MAX_FAST";
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// The database name of a launched site (the launcher names it from the site).
pub fn database_of(site: &str) -> String {
    site.replace('-', "_")
}

/// Injects `fault`. `db` is the scratch-database helper (its admin connection reaches the same
/// PostgreSQL), `database` the site's database.
pub async fn inject(fault: Fault, db: &TestDb, database: &str, undo: &mut Undo) -> Injected {
    match fault {
        Fault::PgConnectionKill => {
            let client = db.admin_client().await;
            let killed = client
                .query(
                    "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
                     WHERE datname = $1 AND pid <> pg_backend_pid()",
                    &[&database],
                )
                .await;
            match killed {
                Ok(rows) => {
                    println!("l9 faults: terminated {} backends", rows.len());
                    Injected::Active
                }
                Err(e) => Injected::Skipped(format!("pg_terminate_backend failed: {e}")),
            }
        }
        Fault::PgRestart => {
            let Some(data) = var(env::PG_DATA) else {
                return Injected::Skipped(format!(
                    "{} is not set: no harness-owned PostgreSQL to restart",
                    env::PG_DATA
                ));
            };
            let ctl = var(env::PG_CTL).unwrap_or_else(|| "pg_ctl".to_owned());
            // `-w` waits until the server accepts connections again, so the fault lasts for the
            // restart itself; readiness is watched by the probe meanwhile.
            match Command::new(ctl)
                .args(["restart", "-m", "fast", "-w", "-D", &data])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .await
            {
                Ok(status) if status.success() => Injected::Active,
                Ok(status) => Injected::Skipped(format!("pg_ctl restart exited {status}")),
                Err(e) => Injected::Skipped(format!("cannot run pg_ctl: {e}")),
            }
        }
        Fault::DiskFull => {
            let Some(dir) = var(env::FULL_DIR) else {
                return Injected::Skipped(format!(
                    "{} is not set: filling a disk needs a small tmpfs or loop mount (privilege)",
                    env::FULL_DIR
                ));
            };
            let dir = PathBuf::from(dir);
            let (total, used) = match disk_usage(&dir).await {
                Some(v) => v,
                None => {
                    return Injected::Skipped(
                        "cannot read the filesystem usage with df".to_owned(),
                    );
                }
            };
            let goal = fill_target_bytes(total, used, 95);
            let filler = dir.join("l9-filler.bin");
            match write_filler(&filler, goal) {
                Ok(()) => {
                    undo.filler = Some(filler);
                    Injected::Active
                }
                Err(e) => Injected::Skipped(format!("cannot fill the disk: {e}")),
            }
        }
        Fault::SlowDisk => {
            let (Some(file), Some(slow), Some(_)) = (
                var(env::IO_MAX_FILE),
                var(env::IO_MAX_SLOW),
                var(env::IO_MAX_FAST),
            ) else {
                return Injected::Skipped(format!(
                    "{}, {} and {} are not all set: throttling a disk needs a cgroup io.max (privilege)",
                    env::IO_MAX_FILE,
                    env::IO_MAX_SLOW,
                    env::IO_MAX_FAST
                ));
            };
            let fast = var(env::IO_MAX_FAST).unwrap_or_default();
            match std::fs::write(&file, format!("{slow}\n")) {
                Ok(()) => {
                    undo.io_max = Some((PathBuf::from(file), fast));
                    Injected::Active
                }
                Err(e) => Injected::Skipped(format!("cannot write io.max: {e}")),
            }
        }
        Fault::ClockSkew => Injected::Skipped(
            "the host has no --clock-offset hook: the clock is read through the Clock port, which \
             this harness cannot wrap from outside the process"
                .to_owned(),
        ),
    }
}

/// Undoes what [`inject`] left in effect (a no-op for faults that end by themselves).
pub fn heal(undo: &mut Undo) {
    if let Some(path) = undo.filler.take() {
        let _ = std::fs::remove_file(path);
    }
    if let Some((file, fast)) = undo.io_max.take() {
        let _ = std::fs::write(file, format!("{fast}\n"));
    }
}

/// The bytes to write so that `used` becomes `percent` percent of `total` (0 when it already is).
pub fn fill_target_bytes(total: u64, used: u64, percent: u64) -> u64 {
    let goal = u128::from(total) * u128::from(percent) / 100;
    u64::try_from(goal.saturating_sub(u128::from(used))).unwrap_or(u64::MAX)
}

/// `(total, used)` bytes of the filesystem of `dir`, from `df -P -B1`.
async fn disk_usage(dir: &Path) -> Option<(u64, u64)> {
    let out = Command::new("df")
        .args(["-P", "-B1"])
        .arg(dir)
        .output()
        .await
        .ok()?;
    parse_df(&String::from_utf8_lossy(&out.stdout))
}

/// Parses the second line of `df -P -B1` output.
pub fn parse_df(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().nth(1)?;
    let fields: Vec<&str> = line.split_whitespace().collect();
    Some((fields.get(1)?.parse().ok()?, fields.get(2)?.parse().ok()?))
}

fn write_filler(path: &Path, bytes: u64) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(path)?;
    let chunk = vec![0u8; 1024 * 1024];
    let mut left = bytes;
    while left > 0 {
        let n = usize::try_from(left.min(chunk.len() as u64)).unwrap_or(chunk.len());
        file.write_all(&chunk[..n])?;
        left -= n as u64;
    }
    file.sync_all()
}

// --- answers during a fault ---------------------------------------------------------------------------

/// How an attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attempt {
    /// 2xx: the write is acknowledged.
    Acked,
    /// 409: the key or the row already exists (a retry after an unseen success).
    Exists,
    /// 429, 502, 503 or 504: the contract's retryable refusals.
    Retryable,
    /// No answer within the request timeout.
    TimedOut,
    /// The connection failed or dropped: retry on a new one.
    Dropped,
    /// Anything else: a 500 or an unexpected 4xx. Never allowed during a fault.
    Unexpected(u16),
}

/// Classifies a status.
pub fn classify(status: u16) -> Attempt {
    match status {
        200..=299 => Attempt::Acked,
        409 => Attempt::Exists,
        429 | 502 | 503 | 504 => Attempt::Retryable,
        other => Attempt::Unexpected(other),
    }
}

/// What a writer did.
#[derive(Debug, Default, Clone)]
pub struct Ledger {
    /// Names whose write was acknowledged.
    pub acked: BTreeSet<String>,
    /// Names no attempt acknowledged.
    pub unacked: BTreeSet<String>,
    /// Attempts by outcome, in order of occurrence: (milliseconds on the run clock, outcome).
    pub attempts: Vec<(u64, Attempt)>,
    /// The longest an attempt took, milliseconds.
    pub slowest_ms: u64,
}

impl Ledger {
    /// Statuses that were neither a success nor a contract refusal.
    pub fn unexpected(&self) -> Vec<u16> {
        self.attempts
            .iter()
            .filter_map(|(_, a)| match a {
                Attempt::Unexpected(s) => Some(*s),
                _ => None,
            })
            .collect()
    }

    /// The first moment, at or after `from_ms`, from which `streak` attempts in a row were
    /// acknowledged; `None` if that never happened.
    pub fn recovered_at(&self, from_ms: u64, streak: usize) -> Option<u64> {
        let tail: Vec<&(u64, Attempt)> = self
            .attempts
            .iter()
            .filter(|(t, _)| *t >= from_ms)
            .collect();
        let mut run = 0;
        for (t, a) in &tail {
            if matches!(a, Attempt::Acked | Attempt::Exists) {
                run += 1;
                if run >= streak {
                    return Some(*t);
                }
            } else {
                run = 0;
            }
        }
        None
    }
}

/// One session of a ledger writer.
pub struct WriterSession {
    /// The target.
    pub target: Target,
    /// The sign-in name.
    pub email: String,
    /// The password.
    pub password: String,
}

async fn sign_in(conn: &mut Conn, s: &WriterSession) -> Option<String> {
    let body = json!({ "user": s.email, "password": s.password }).to_string();
    let r = conn
        .request(
            &s.target,
            "POST",
            "/api/v1/auth/login",
            &[("Content-Type", "application/json")],
            Some(body.as_bytes()),
            REQUEST_TIMEOUT,
        )
        .await
        .ok()?;
    if r.status != 200 {
        return None;
    }
    r.json()
        .pointer("/data/token")
        .and_then(Json::as_str)
        .map(|t| format!("Bearer {t}"))
}

/// The user id the session answer names (the owner a project needs).
async fn own_id(conn: &mut Conn, s: &WriterSession, bearer: &str) -> Option<String> {
    let r = conn
        .request(
            &s.target,
            "GET",
            "/api/v1/auth/session",
            &[("Authorization", bearer)],
            None,
            REQUEST_TIMEOUT,
        )
        .await
        .ok()?;
    r.json()
        .pointer("/data/user/id")
        .and_then(Json::as_str)
        .map(str::to_owned)
}

/// Writes projects named `l9f<run>x<n>` at about `per_second` until `stop`, each with an
/// `Idempotency-Key` equal to its name, retrying an unacknowledged write up to `attempts` times
/// with the same key. The returned ledger is what was acknowledged and what was not.
pub async fn ledger_writer(
    session: WriterSession,
    run: String,
    first: u64,
    per_second: u64,
    attempts: u32,
    stop: Arc<AtomicBool>,
    clock: Arc<dyn Clock>,
) -> Ledger {
    let mut ledger = Ledger::default();
    let mut conn = Conn::new();
    let mut bearer = sign_in(&mut conn, &session).await.unwrap_or_default();
    let mut owner = own_id(&mut conn, &session, &bearer)
        .await
        .unwrap_or_default();
    let pause = Duration::from_millis((1000 / per_second.max(1)).max(1));
    let mut n = first;
    while !stop.load(Ordering::SeqCst) {
        n += 1;
        let name = format!("L9F{run}X{n}");
        let body = json!({
            "code": name, "title": "L9 fault ledger", "status": "Active",
            "owner": owner, "currency": "USD", "budget": "1000.00"
        })
        .to_string();
        let mut acked = false;
        for _ in 0..attempts {
            let started = clock.now_ns();
            let result = conn
                .request(
                    &session.target,
                    "POST",
                    "/api/v1/resource/project",
                    &[
                        ("Authorization", &bearer),
                        ("Content-Type", "application/json"),
                        ("Idempotency-Key", &name),
                    ],
                    Some(body.as_bytes()),
                    REQUEST_TIMEOUT,
                )
                .await;
            let now = clock.now_ns();
            ledger.slowest_ms = ledger.slowest_ms.max((now - started) / 1_000_000);
            let outcome = match result {
                Ok(r) if r.status == 401 => {
                    // The session did not survive a restart: sign in again and retry.
                    if let Some(fresh) = sign_in(&mut conn, &session).await {
                        bearer = fresh;
                        owner = own_id(&mut conn, &session, &bearer).await.unwrap_or(owner);
                    }
                    Attempt::Dropped
                }
                Ok(r) => classify(r.status),
                Err(HttpError::Timeout) => Attempt::TimedOut,
                Err(HttpError::Transport) => {
                    conn = Conn::new();
                    Attempt::Dropped
                }
            };
            ledger.attempts.push((now / 1_000_000, outcome));
            if matches!(outcome, Attempt::Acked | Attempt::Exists) {
                acked = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        if acked {
            ledger.acked.insert(name);
        } else {
            ledger.unacked.insert(name);
        }
        tokio::time::sleep(pause).await;
    }
    ledger
}

/// Merges ledgers of several writers.
pub fn merge(ledgers: Vec<Ledger>) -> Ledger {
    let mut out = Ledger::default();
    for l in ledgers {
        out.acked.extend(l.acked);
        out.unacked.extend(l.unacked);
        out.attempts.extend(l.attempts);
        out.slowest_ms = out.slowest_ms.max(l.slowest_ms);
    }
    out.attempts.sort_by_key(|(t, _)| *t);
    out
}

/// Probes `/api/health/ready` every 250 ms until `stop`; (milliseconds on the run clock, status
/// or 0 when no answer).
pub async fn readiness_probe(
    target: Target,
    stop: Arc<AtomicBool>,
    clock: Arc<dyn Clock>,
    out: Arc<Mutex<Vec<(u64, u16)>>>,
) {
    while !stop.load(Ordering::SeqCst) {
        let mut conn = Conn::new();
        let status = conn
            .request(
                &target,
                "GET",
                "/api/health/ready",
                &[],
                None,
                Duration::from_secs(2),
            )
            .await
            .map_or(0, |r| r.status);
        if let Ok(mut v) = out.lock() {
            v.push((clock.now_ns() / 1_000_000, status));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// The readiness transitions of a probe series: the first moment at or after `from_ms` that was
/// not ready, and the first moment after that which was ready again.
pub fn transitions(series: &[(u64, u16)], from_ms: u64) -> (Option<u64>, Option<u64>) {
    let down = series
        .iter()
        .find(|(t, s)| *t >= from_ms && *s != 200)
        .map(|(t, _)| *t);
    let up = down.and_then(|d| {
        series
            .iter()
            .find(|(t, s)| *t > d && *s == 200)
            .map(|(t, _)| *t)
    });
    (down, up)
}

/// Every project code in the site, paged: what a client can read back.
pub async fn all_codes(target: &Target, bearer: &str) -> Result<Vec<String>, String> {
    let mut conn = Conn::new();
    let mut codes = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut body = json!({ "limit": 500 });
        if let Some(c) = &cursor {
            body["cursor"] = Json::String(c.clone());
        }
        let bytes = body.to_string().into_bytes();
        let r = conn
            .request(
                target,
                "POST",
                "/api/v1/resource/project/query",
                &[
                    ("Authorization", bearer),
                    ("Content-Type", "application/json"),
                ],
                Some(&bytes),
                REQUEST_TIMEOUT,
            )
            .await
            .map_err(|e| format!("the project list failed: {e:?}"))?;
        if r.status != 200 {
            return Err(format!("the project list answered {}", r.status));
        }
        let page = r.json();
        for row in page
            .get("data")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default()
        {
            if let Some(c) = row.get("code").and_then(Json::as_str) {
                codes.push(c.to_owned());
            }
        }
        cursor = page
            .get("next_cursor")
            .and_then(Json::as_str)
            .map(str::to_owned);
        if cursor.is_none() {
            return Ok(codes);
        }
    }
}

/// The checks that follow a fault. Panics with the fault, the ids and the timeline, never
/// retrying a lost acknowledged write away.
pub fn check_ledger(fault: Fault, ledger: &Ledger, codes: &[String]) {
    let present: BTreeSet<&String> = codes.iter().collect();
    let lost: Vec<&String> = ledger
        .acked
        .iter()
        .filter(|n| !present.contains(*n))
        .collect();
    assert!(
        lost.is_empty(),
        "RELEASE BLOCKER: {} acknowledged writes are missing after {}: {lost:?}",
        lost.len(),
        fault.name()
    );
    // Exactly once: no code appears twice (an unacknowledged write is absent or present once).
    let mut seen = BTreeSet::new();
    for code in codes {
        assert!(
            seen.insert(code),
            "code {code} is present twice after {}",
            fault.name()
        );
    }
    assert_eq!(
        ledger.unexpected(),
        Vec::<u16>::new(),
        "{} produced answers that are neither a success nor a retryable refusal",
        fault.name()
    );
}
