//! Running a workload and checking it.

use std::sync::Arc;

use spot_storage_contract::{StorageBackend, StorageResult};

use crate::builtin::framework_registry;
use crate::driver::{DriverConfig, LoadDriver};
use crate::history::History;
use crate::invariant::{CheckContext, InvariantRegistry, Report, RunMeta};
use crate::model::Model;
use crate::oracle::Oracle;

/// A finished run: what the checks compare the store with.
pub struct Completed {
    /// The history.
    pub history: Arc<History>,
    /// The oracle.
    pub oracle: Arc<Oracle>,
    /// Describes the run.
    pub meta: RunMeta,
}

/// Runs the workload of `config` on `backend`.
pub async fn run_workload<B: StorageBackend>(
    backend: Arc<B>,
    model: Arc<Model>,
    config: DriverConfig,
    backend_name: &str,
) -> StorageResult<Completed> {
    let driver = LoadDriver::new(backend, model, config);
    let run = driver.run().await?;
    Ok(Completed {
        meta: RunMeta {
            seed: run.seed,
            backend: backend_name.to_owned(),
            profile: run.profile.clone(),
            workers: run.workers,
        },
        history: Arc::new(run.history),
        oracle: Arc::new(run.oracle),
    })
}

/// Checks every invariant of `registry` against the store.
pub async fn check<B: StorageBackend>(
    backend: Arc<B>,
    model: &Model,
    done: &Completed,
    registry: &InvariantRegistry,
) -> Report {
    let ctx = CheckContext::for_backend(
        backend,
        model.catalog.clone(),
        done.history.clone(),
        done.oracle.clone(),
    );
    registry.check_all(&ctx, done.meta.clone()).await
}

/// Runs the workload and checks the framework invariants plus whatever `extend` registers.
pub async fn run_and_check<B: StorageBackend>(
    backend: Arc<B>,
    model: Arc<Model>,
    config: DriverConfig,
    backend_name: &str,
    extend: impl FnOnce(&mut InvariantRegistry),
) -> StorageResult<(Completed, Report)> {
    let done = run_workload(backend.clone(), model.clone(), config, backend_name).await?;
    let mut registry = framework_registry(&model);
    extend(&mut registry);
    let report = check(backend, &model, &done, &registry).await;
    Ok((done, report))
}
