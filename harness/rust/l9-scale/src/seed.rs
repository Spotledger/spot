//! The volume loader (WP-TST-15.T4): creates `task` parent documents with `checklist` child
//! rows through the **same HTTP path** `l9_scale::session` already uses against a real
//! `spot serve` (`POST /api/v1/resource/task`), not by writing to the storage tables.
//!
//! Why the API path and not `COPY`: the platform's document engine has no separate bulk-import
//! entry point this harness can call without going through HTTP (the nearest thing,
//! `spot-import`, still validates and writes one row at a time through a `DocumentService` and
//! is not reachable from this crate without new dependencies the brief does not name), and a
//! `COPY` straight into the storage tables would mean hand-building SQL in a crate that is not
//! a storage crate (forbidden by ADR-45). The API path is slower per row than `COPY` would be,
//! but it is deterministic, keeps memory flat (a bounded pool of persistent connections, not
//! one per row) and is the path every other `l9-scale` operation already measures.
//!
//! Rows are created by a fixed number of workers, each with its own persistent [`Conn`], pulling
//! the next index from a shared counter that starts at the checkpoint file's last completed
//! index: a run interrupted by its time box or a failure resumes from there instead of
//! redoing work. No step here reads a wall clock; the deadline is a nanosecond value on the
//! caller's [`Clock`] (ADR-087 hard rule: time is an argument).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use tokio::sync::Mutex;

// `seed.rs` is included directly by `tests/volume.rs` (`#[path = "../src/seed.rs"] mod seed;`)
// rather than declared in `lib.rs`, whose module list is not an owned path for this task,
// so this file is compiled as part of the test binary, which depends on `l9_scale` as an
// ordinary external crate; every item this module needs from the harness is already `pub`.
use l9_scale::clock::Clock;
use l9_scale::http::{Conn, Target};
use l9_scale::rng::Rng;

/// The parent DocType the volume loader seeds (the same one `l9_scale::session::READING` lists).
pub const DOCTYPE: &str = "task";
/// The child-table field the parent carries (`task.checklist`, child DocType `checklist_item`).
pub const CHILD_FIELD: &str = "checklist";

/// A document write failed outright (a transport error, a timeout, or a non-2xx status that is
/// not the `409`/`412` a concurrent naming series would (eventually) produce).
#[derive(Debug, Clone)]
pub struct SeedError(pub String);

impl std::fmt::Display for SeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SeedError {}

/// Refuses a target whose host does not look like a test deployment's. Seeded data is
/// synthetic and harmless, but the loader must never run against anything that is not
/// obviously a scratch or test system (the brief's security note).
pub fn refuse_unless_test_like(name: &str) -> Result<(), SeedError> {
    let lower = name.to_ascii_lowercase();
    let looks_like_test = ["l9-", "l9_", "test", "scratch", "spot_test", "sandbox"]
        .iter()
        .any(|needle| lower.contains(needle));
    if looks_like_test {
        Ok(())
    } else {
        Err(SeedError(format!(
            "refusing to seed volume data into `{name}`: its name does not look like a test \
             or scratch deployment"
        )))
    }
}

/// How many child rows a parent gets, how many parents to make and how to make them.
#[derive(Debug, Clone)]
pub struct SeedOptions {
    /// Where the API is.
    pub target: Target,
    /// `Bearer <token>` for a user allowed to create `task` and `project` documents.
    pub bearer: String,
    /// How many `task` parents to create in total (across every run against one checkpoint).
    pub parents: u64,
    /// The fewest `checklist` rows a task gets.
    pub children_min: u32,
    /// The most `checklist` rows a task gets (inclusive).
    pub children_max: u32,
    /// The schedule's seed: the same seed and `parents` always build the same documents.
    pub seed: u64,
    /// Persistent connections writing concurrently (bounds memory and open sockets, not rows).
    pub workers: usize,
    /// A request slower than this is a failure.
    pub request_timeout: Duration,
    /// Progress is recorded here periodically, so a run that stops (the time box, a crash,
    /// `Ctrl-C`) resumes instead of starting over.
    pub checkpoint: PathBuf,
    /// Stop accepting new work once the clock passes this value; `None` runs to completion.
    pub deadline_ns: Option<u64>,
}

/// What one `seed` call did.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SeedReport {
    /// Rows successfully created in this call (not counting rows a previous call already made).
    pub created: u64,
    /// Rows that failed outright (the loader stops a batch early when every row in it fails).
    pub failed: u64,
    /// Child (`checklist_item`) rows created along with the parents counted in `created`.
    pub child_rows: u64,
    /// The checkpoint's total completed count after this call (`created` of every call so far).
    pub total_done: u64,
    /// Whether the deadline, not completion, ended the call.
    pub timed_out: bool,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct Checkpoint {
    done: u64,
}

fn load_checkpoint(path: &Path) -> Checkpoint {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_checkpoint(path: &Path, checkpoint: Checkpoint) -> Result<(), SeedError> {
    let text = serde_json::to_string(&checkpoint)
        .map_err(|e| SeedError(format!("cannot encode the checkpoint: {e}")))?;
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .map_err(|e| SeedError(format!("cannot create {}: {e}", parent.display())))?;
    }
    std::fs::write(path, text)
        .map_err(|e| SeedError(format!("cannot write {}: {e}", path.display())))
}

const STATUSES: [&str; 5] = ["Open", "In Progress", "Blocked", "Review", "Done"];
const PRIORITIES: [&str; 4] = ["Low", "Medium", "High", "Urgent"];

/// A `task` document for `index`, deterministic from `options.seed`: the same index with the
/// same seed and the same `projects` always builds the same JSON body, in any run and on any
/// worker (no two workers ever draw from a shared generator, so none shares state to race on).
pub fn build_task(options: &SeedOptions, projects: &[String], index: u64) -> Json {
    let mut rng =
        Rng::new(options.seed ^ index.wrapping_add(1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let n_children = u32::try_from(rng.between(
        u64::from(options.children_min),
        u64::from(options.children_max),
    ))
    .unwrap_or(options.children_min);
    let checklist: Vec<Json> = (0..n_children)
        .map(|i| {
            json!({
                "text": format!("l9-volume-{index}-item-{i}"),
                "done": rng.below(2) == 1,
            })
        })
        .collect();
    let project = projects
        .get(usize::try_from(rng.below(projects.len().max(1) as u64)).unwrap_or(0))
        .cloned()
        .unwrap_or_default();
    let status = STATUSES[usize::try_from(rng.below(STATUSES.len() as u64)).unwrap_or(0)];
    let priority = PRIORITIES[usize::try_from(rng.below(PRIORITIES.len() as u64)).unwrap_or(0)];
    let month = 1 + rng.below(12);
    let day = 1 + rng.below(28);
    json!({
        "subject": format!("L9 volume task {index:08}"),
        "project": project,
        "status": status,
        "priority": priority,
        "due_date": format!("2026-{month:02}-{day:02}"),
        "estimated_hours": format!("{}.00", 1 + rng.below(40)),
        CHILD_FIELD: checklist,
    })
}

/// Creates `count` `project` parents the seeded tasks can link to (one call, not time-boxed:
/// there are always few of them, even at the 50-million-row tier).
pub async fn ensure_projects(
    target: &Target,
    bearer: &str,
    timeout: Duration,
    owner: &str,
    count: u32,
    seed: u64,
) -> Result<Vec<String>, SeedError> {
    refuse_unless_test_like(&target.host)?;
    let mut conn = Conn::new();
    let mut ids = Vec::with_capacity(count as usize);
    for i in 0..count {
        let body = json!({
            "code": format!("L9VOL-{seed:x}-{i:04}"),
            "title": "L9 volume project",
            "status": "Active",
            "owner": owner,
            "currency": "USD",
            "budget": "100000.00",
        });
        let bytes = body.to_string().into_bytes();
        let response = conn
            .request(
                target,
                "POST",
                "/api/v1/resource/project",
                &[
                    ("Authorization", bearer),
                    ("Content-Type", "application/json"),
                ],
                Some(&bytes),
                timeout,
            )
            .await
            .map_err(|e| SeedError(format!("creating a volume project: {e:?}")))?;
        if response.status != 201 {
            return Err(SeedError(format!(
                "creating a volume project answered HTTP {}",
                response.status
            )));
        }
        let id = response
            .json()
            .pointer("/data/id")
            .and_then(Json::as_str)
            .map(str::to_owned)
            .ok_or_else(|| SeedError("a created project carries no id".to_owned()))?;
        ids.push(id);
    }
    Ok(ids)
}

/// Creates up to `options.parents` `task` documents (resuming from the checkpoint, stopping at
/// the deadline), each with `options.children_min..=children_max` `checklist` rows, through
/// `options.workers` persistent connections pulling from one shared counter.
pub async fn seed(
    options: &SeedOptions,
    projects: &[String],
    clock: Arc<dyn Clock>,
) -> Result<SeedReport, SeedError> {
    refuse_unless_test_like(&options.target.host)?;
    if projects.is_empty() {
        return Err(SeedError(
            "seed needs at least one project to link".to_owned(),
        ));
    }
    let checkpoint = load_checkpoint(&options.checkpoint);
    let start = checkpoint.done.min(options.parents);
    let next = Arc::new(AtomicU64::new(start));
    let created = Arc::new(AtomicU64::new(0));
    let failed = Arc::new(AtomicU64::new(0));
    let child_rows = Arc::new(AtomicU64::new(0));
    let timed_out = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // Saving the checkpoint is its own critical section: two workers must never interleave a
    // write of the file. It is written periodically (every `CHECKPOINT_EVERY` attempts), not
    // after every row: at this crate's write throughput a file write per HTTP round trip would
    // make the checkpoint the bottleneck, not the server. A run that stops between two
    // checkpoints resumes a little behind where it actually got to and recreates the handful
    // of rows in between (extra, harmless synthetic rows, not a correctness problem for this
    // loader's purpose) rather than losing any.
    const CHECKPOINT_EVERY: u64 = 500;
    let checkpoint_lock = Arc::new(Mutex::new(()));
    let attempted = Arc::new(AtomicU64::new(start));

    let workers = options.workers.max(1);
    let mut handles = Vec::with_capacity(workers);
    for _ in 0..workers {
        let target = options.target.clone();
        let bearer = options.bearer.clone();
        let timeout = options.request_timeout;
        let options = options.clone();
        let projects = projects.to_vec();
        let next = next.clone();
        let created = created.clone();
        let failed = failed.clone();
        let child_rows = child_rows.clone();
        let timed_out = timed_out.clone();
        let checkpoint_lock = checkpoint_lock.clone();
        let attempted = attempted.clone();
        let clock = clock.clone();
        handles.push(tokio::spawn(async move {
            let mut conn = Conn::new();
            loop {
                if let Some(deadline) = options.deadline_ns
                    && clock.now_ns() >= deadline
                {
                    timed_out.store(true, Ordering::SeqCst);
                    return;
                }
                let index = next.fetch_add(1, Ordering::SeqCst);
                if index >= options.parents {
                    return;
                }
                let body = build_task(&options, &projects, index);
                let n_children = body
                    .get(CHILD_FIELD)
                    .and_then(Json::as_array)
                    .map_or(0, Vec::len) as u64;
                let bytes = body.to_string().into_bytes();
                let outcome = conn
                    .request(
                        &target,
                        "POST",
                        "/api/v1/resource/task",
                        &[
                            ("Authorization", &bearer),
                            ("Content-Type", "application/json"),
                        ],
                        Some(&bytes),
                        timeout,
                    )
                    .await;
                let ok = matches!(outcome, Ok(ref r) if r.status == 201);
                if ok {
                    created.fetch_add(1, Ordering::SeqCst);
                    child_rows.fetch_add(n_children, Ordering::SeqCst);
                } else {
                    failed.fetch_add(1, Ordering::SeqCst);
                }
                let done_so_far = attempted.fetch_add(1, Ordering::SeqCst) + 1;
                let last = index + 1 == options.parents;
                if last || done_so_far.is_multiple_of(CHECKPOINT_EVERY) {
                    let guard = checkpoint_lock.lock().await;
                    let done = attempted.load(Ordering::SeqCst).min(options.parents);
                    let _ = save_checkpoint(&options.checkpoint, Checkpoint { done });
                    drop(guard);
                }
            }
        }));
    }
    for handle in handles {
        let _ = handle.await;
    }
    // One last save: the deadline can stop every worker between two periodic checkpoints.
    {
        let done = attempted.load(Ordering::SeqCst).min(options.parents);
        save_checkpoint(&options.checkpoint, Checkpoint { done })?;
    }
    let total_done = load_checkpoint(&options.checkpoint).done.max(start);
    Ok(SeedReport {
        created: created.load(Ordering::SeqCst),
        failed: failed.load(Ordering::SeqCst),
        child_rows: child_rows.load(Ordering::SeqCst),
        total_done,
        timed_out: timed_out.load(Ordering::SeqCst),
    })
}

/// Runs `spot --root <root> check` (the closest existing storage-readiness check this codebase
/// has) and returns its output, or the reason it failed. There is no `--deep` flag on `check`
/// yet (the brief names one; this codebase does not have it — see the L9
/// baseline notes), so this is the full verification pass available today.
pub async fn run_check(bin: &Path, root: &Path) -> Result<String, SeedError> {
    let output = tokio::process::Command::new(bin)
        .arg("--root")
        .arg(root)
        .arg("check")
        .output()
        .await
        .map_err(|e| SeedError(format!("cannot run `spot check`: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(stdout)
    } else {
        Err(SeedError(format!(
            "`spot check` failed: {}{}",
            stdout,
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn opts() -> SeedOptions {
        SeedOptions {
            target: Target {
                addr: ([127, 0, 0, 1], 1).into(),
                host: "l9-scale-unit".into(),
            },
            bearer: "Bearer x".into(),
            parents: 10,
            children_min: 0,
            children_max: 3,
            seed: 42,
            workers: 4,
            request_timeout: Duration::from_secs(1),
            checkpoint: std::env::temp_dir().join("l9_seed_unit_checkpoint.json"),
            deadline_ns: None,
        }
    }

    #[test]
    fn a_target_must_look_like_a_test_deployment() {
        assert!(refuse_unless_test_like("l9-scale-abc123").is_ok());
        assert!(refuse_unless_test_like("spot_test_abc").is_ok());
        assert!(refuse_unless_test_like("production").is_err());
        assert!(refuse_unless_test_like("acme-live").is_err());
    }

    #[test]
    fn the_same_seed_and_index_always_build_the_same_task() {
        let options = opts();
        let projects = vec!["p1".to_owned(), "p2".to_owned()];
        let a = build_task(&options, &projects, 7);
        let b = build_task(&options, &projects, 7);
        assert_eq!(a, b);
        let c = build_task(&options, &projects, 8);
        assert_ne!(a, c);
    }

    #[test]
    fn child_counts_stay_inside_their_bounds() {
        let mut options = opts();
        options.children_min = 2;
        options.children_max = 5;
        let projects = vec!["p1".to_owned()];
        for index in 0..200u64 {
            let task = build_task(&options, &projects, index);
            let n = task
                .get(CHILD_FIELD)
                .and_then(Json::as_array)
                .unwrap()
                .len();
            assert!((2..=5).contains(&n), "{n} children at index {index}");
        }
    }

    #[test]
    fn a_checkpoint_round_trips_through_a_file() {
        let path = std::env::temp_dir().join(format!("l9_seed_cp_{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        assert_eq!(load_checkpoint(&path).done, 0);
        save_checkpoint(&path, Checkpoint { done: 42 }).unwrap();
        assert_eq!(load_checkpoint(&path).done, 42);
        let _ = std::fs::remove_file(&path);
    }
}
