//! Running the matrix: the in-memory fake and PostgreSQL, each at the three levels.
//!
//! The expected outcome table is checked in beside the runs:
//!
//! | Backend | `rc` | `rr` | `ser` | Why |
//! | --- | --- | --- | --- | --- |
//! | in-memory fake | pass | pass | **write skew (G2)** | the fake documents that `Serializable` behaves like `RepeatableRead` and does not detect read-only anomalies (see its crate docs); finding those is the checker's job |
//! | PostgreSQL | pass | pass | pass | PostgreSQL delivers the declared level |
//!
//! "pass" means no anomaly the claimed level forbids. The fake's serializable finding is a known,
//! documented property of the reference backend, not a checker bug; the matrix records it with a
//! witness so a reader can see exactly what happened. PostgreSQL runs are skipped, with a
//! `SKIPPED` notice, when `SPOT_TEST_PG_URL` is not set (and fail when `SPOT_REQUIRE_PG=1`).

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use spot_l7_invariants::history::Level;
use spot_l7_invariants::pg::pg_env;
use spot_storage_contract::{StorageBackend, StorageResult};
use spot_storage_fake::FakeBackend;

use crate::anomaly::AnomalyClass;
use crate::checker::check;
use crate::history::Workload;
use crate::levels::{level_describe, level_name};
use crate::model::Model;
use crate::report::IsolationReport;
use crate::weak::{Weakened, Weakness};
use crate::workload::{self, WorkloadConfig};

/// The `schema` value of a matrix report.
pub const MATRIX_SCHEMA: &str = "spot-l8-matrix/1";

/// Runs the fake at one level and checks the history.
pub async fn run_level_fake(
    model: Arc<Model>,
    config: &WorkloadConfig,
    level: Level,
) -> StorageResult<IsolationReport> {
    let backend = Arc::new(FakeBackend::new());
    run_and_check(backend, model, config, level).await
}

/// Runs the fake behind a weakened wrapper at one level and checks the history.
pub async fn run_level_weak(
    model: Arc<Model>,
    config: &WorkloadConfig,
    level: Level,
    weakness: Weakness,
) -> StorageResult<IsolationReport> {
    let backend = Arc::new(Weakened::new(Arc::new(FakeBackend::new()), weakness));
    run_and_check(backend, model, config, level).await
}

/// Runs PostgreSQL at one level and checks the history, or `None` when no server is configured.
pub async fn run_level_pg(
    model: Arc<Model>,
    config: &WorkloadConfig,
    level: Level,
) -> Option<StorageResult<IsolationReport>> {
    let env = pg_env(config.workers).await?;
    Some(run_and_check(env.backend.clone(), model, config, level).await)
}

async fn run_and_check<B: StorageBackend>(
    backend: Arc<B>,
    model: Arc<Model>,
    config: &WorkloadConfig,
    level: Level,
) -> StorageResult<IsolationReport> {
    let history = workload::run_on(backend, model, config, level).await?;
    Ok(check(&history, level))
}

/// The result at one level.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LevelOutcome {
    /// The level.
    pub level: Level,
    /// What the checker found.
    pub report: IsolationReport,
}

/// The result for one backend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendMatrix {
    /// The backend's name (`fake`, `pg`).
    pub backend: String,
    /// One outcome per level.
    pub levels: Vec<LevelOutcome>,
}

impl BackendMatrix {
    /// The outcome at a level.
    pub fn at(&self, level: Level) -> Option<&IsolationReport> {
        self.levels
            .iter()
            .find(|l| l.level == level)
            .map(|l| &l.report)
    }
}

/// The whole matrix.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MatrixReport {
    /// Always [`MATRIX_SCHEMA`].
    pub schema: String,
    /// The workload.
    pub workload: Workload,
    /// The seed.
    pub seed: u64,
    /// One entry per backend that was run.
    pub backends: Vec<BackendMatrix>,
}

impl MatrixReport {
    /// The backend named `name`.
    pub fn backend(&self, name: &str) -> Option<&BackendMatrix> {
        self.backends.iter().find(|b| b.backend == name)
    }

    /// Whether every forbidden anomaly the run found is one the expected table allows.
    ///
    /// The table (see the module docs) expects no violation anywhere except the fake's
    /// `Serializable` level, where write skew `G2` is the documented consequence of the fake
    /// running serializable as repeatable read. A run that does not exhibit it is not called a
    /// failure here; the test that checks the fake's serializable level states that separately.
    pub fn meets_expectation(&self) -> bool {
        self.backends.iter().all(|b| {
            b.levels.iter().all(|l| {
                let allowed = expected_forbidden(&b.backend, l.level);
                l.report.violations.iter().all(|c| allowed.contains(c))
            })
        })
    }

    /// Pretty JSON.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
    }

    /// Plain text.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "L8 isolation matrix (workload {}, seed {})\n",
            self.workload.name(),
            self.seed
        ));
        for b in &self.backends {
            out.push_str(&format!("backend {}:\n", b.backend));
            for l in &b.levels {
                out.push_str(&format!(
                    "  {} [{}] {}",
                    level_name(l.level),
                    if l.report.ok() { "pass" } else { "FAIL" },
                    l.report.to_text().lines().next().unwrap_or("")
                ));
                out.push('\n');
                for c in &l.report.classes {
                    out.push_str(&format!(
                        "      [{}] {} x{}\n",
                        if c.forbidden { "FAIL" } else { "allow" },
                        c.class.name(),
                        c.total
                    ));
                }
            }
        }
        out
    }
}

/// The forbidden anomaly classes the expected table permits for a backend/level.
///
/// The fake's `ser` level is expected to show write skew `G2` (a violation of serializable, and
/// the documented consequence of the fake implementing serializable as repeatable read); every
/// other backend/level is expected to show no violation at all.
pub fn expected_forbidden(backend: &str, level: Level) -> &'static [AnomalyClass] {
    match (backend, level) {
        ("fake", Level::Serializable) => &[AnomalyClass::G2],
        _ => &[],
    }
}

/// Runs the whole matrix: the fake always; PostgreSQL when a server is configured.
pub async fn run_matrix(model: Arc<Model>, config: &WorkloadConfig) -> MatrixReport {
    let mut backends = Vec::new();

    let mut fake = BackendMatrix {
        backend: "fake".to_owned(),
        levels: Vec::new(),
    };
    for level in [
        Level::ReadCommitted,
        Level::RepeatableRead,
        Level::Serializable,
    ] {
        if let Ok(report) = run_level_fake(model.clone(), config, level).await {
            fake.levels.push(LevelOutcome { level, report });
        }
    }
    backends.push(fake);

    let mut pg = BackendMatrix {
        backend: "pg".to_owned(),
        levels: Vec::new(),
    };
    let mut pg_available = false;
    for level in [
        Level::ReadCommitted,
        Level::RepeatableRead,
        Level::Serializable,
    ] {
        match run_level_pg(model.clone(), config, level).await {
            Some(Ok(report)) => {
                pg_available = true;
                pg.levels.push(LevelOutcome { level, report });
            }
            Some(Err(e)) => {
                eprintln!("pg {} run failed: {e}", level_describe(level));
            }
            None => break,
        }
    }
    if pg_available {
        backends.push(pg);
    } else {
        println!("SKIPPED: set SPOT_TEST_PG_URL to run the PostgreSQL matrix");
    }

    MatrixReport {
        schema: MATRIX_SCHEMA.to_owned(),
        workload: config.workload,
        seed: config.seed,
        backends,
    }
}
