//! Running the engine and naming workloads and checking them with T1's machinery.

use std::sync::Arc;

use spot_l7_invariants::history::History;
use spot_l7_invariants::invariant::{
    BoxFuture, CheckContext, InvariantRegistry, Report, RunMeta, StoreAccess,
};
use spot_l7_invariants::oracle::Oracle;
use spot_l7_invariants::schedule::Profile;
use spot_storage_contract::{
    Access, Catalog, Change, FeedPosition, Isolation, Name, OutboxRecord, Query, QueryExecutor,
    Rows, Sequences, SeriesKey, StorageBackend, StorageError, StorageResult, Transaction,
    TxOptions,
};
use tokio::sync::Mutex;

use crate::driver::{DriverConfig, EngineDriver, Run};
use crate::invariants::{engine_registry, naming_registry};
use crate::naming::{NamingConfig, NamingDriver, NamingRun};
use crate::world::World;

/// Read access to the store **through the engine's backend**, so the checks see exactly what
/// the engine wrote without a second handle on the backend.
pub struct EngineAccess<B: StorageBackend> {
    engine: Arc<spot_document::engine::Engine<B>>,
    catalog: Arc<Catalog>,
    snapshot: Mutex<Option<B::Tx>>,
}

impl<B: StorageBackend> EngineAccess<B> {
    /// Access to `engine`'s store through `catalog`.
    pub fn new(engine: Arc<spot_document::engine::Engine<B>>, catalog: Arc<Catalog>) -> Self {
        EngineAccess {
            engine,
            catalog,
            snapshot: Mutex::new(None),
        }
    }
}

const FEED_PAGE: u32 = 5_000;

impl<B: StorageBackend> StoreAccess for EngineAccess<B> {
    fn query<'a>(&'a self, query: &'a Query) -> BoxFuture<'a, StorageResult<Rows>> {
        Box::pin(async move {
            let mut guard = self.snapshot.lock().await;
            if guard.is_none() {
                let mut options = TxOptions::new(Name::new("l7_check")?, Isolation::RepeatableRead);
                options.access = Access::ReadOnly;
                *guard = Some(
                    self.engine
                        .backend()
                        .begin(self.catalog.clone(), options)
                        .await?,
                );
            }
            match guard.as_mut() {
                Some(tx) => QueryExecutor::query(tx, query).await,
                None => Err(StorageError::new(
                    spot_storage_contract::ErrorKind::Internal,
                    "no snapshot",
                )),
            }
        })
    }

    fn outbox(&self) -> BoxFuture<'_, StorageResult<Vec<OutboxRecord>>> {
        Box::pin(async move {
            let mut all = Vec::new();
            let mut after = FeedPosition::START;
            loop {
                let page = self.engine.backend().read_outbox(after, FEED_PAGE).await?;
                let Some(last) = page.last() else { break };
                after = last.position;
                let full = page.len() >= FEED_PAGE as usize;
                all.extend(page);
                if !full {
                    break;
                }
            }
            Ok(all)
        })
    }

    fn changes(&self) -> BoxFuture<'_, StorageResult<Vec<Change>>> {
        Box::pin(async move {
            let mut all = Vec::new();
            let mut after = FeedPosition::START;
            loop {
                let page = self.engine.backend().read_changes(after, FEED_PAGE).await?;
                let Some(last) = page.last() else { break };
                after = last.position;
                let full = page.len() >= FEED_PAGE as usize;
                all.extend(page);
                if !full {
                    break;
                }
            }
            Ok(all)
        })
    }

    fn next_number<'a>(&'a self, series: &'a str) -> BoxFuture<'a, StorageResult<u64>> {
        Box::pin(async move {
            let key = SeriesKey::new(series)?;
            let options = TxOptions::new(Name::new("l7_probe")?, Isolation::ReadCommitted);
            let mut tx = self
                .engine
                .backend()
                .begin(self.catalog.clone(), options)
                .await?;
            let number = tx.allocate(&key, 1).await;
            let rolled = tx.rollback().await;
            let number = number?;
            rolled?;
            Ok(number)
        })
    }
}

/// Runs the engine workload on `backend` and checks every engine invariant.
pub async fn run_engine<B: StorageBackend>(
    world: Arc<World>,
    config: DriverConfig,
    backend: B,
) -> Result<(Run<B>, Report), spot_app_api::AppError> {
    let engine = Arc::new(world.engine(backend)?);
    let driver = EngineDriver::new(engine, world.clone(), config);
    let run = driver.run().await?;
    let report = check_engine(&world, &run).await;
    Ok((run, report))
}

/// Checks an already-finished engine run.
pub async fn check_engine<B: StorageBackend>(world: &Arc<World>, run: &Run<B>) -> Report {
    check_engine_with(world, run, run.oracle.clone()).await
}

/// Checks an engine run against `oracle` (a possibly-corrupted copy), for the proofs.
pub async fn check_engine_with<B: StorageBackend>(
    world: &Arc<World>,
    run: &Run<B>,
    oracle: Arc<crate::driver::EngineRun>,
) -> Report {
    let registry = engine_registry(&oracle, world);
    let ctx = context_for(world, run);
    registry.check_all(&ctx, run.meta.clone()).await
}

/// Checks an engine run with extra invariants (an app's own) registered next to the framework's.
pub async fn check_engine_extended<B: StorageBackend>(
    world: &Arc<World>,
    run: &Run<B>,
    extend: impl FnOnce(&mut InvariantRegistry),
) -> Report {
    let mut registry = engine_registry(&run.oracle, world);
    extend(&mut registry);
    let ctx = context_for(world, run);
    registry.check_all(&ctx, run.meta.clone()).await
}

fn context_for<B: StorageBackend>(world: &Arc<World>, run: &Run<B>) -> CheckContext {
    let access = Arc::new(EngineAccess::new(run.engine.clone(), world.catalog.clone()));
    CheckContext::new(
        access,
        world.catalog.clone(),
        Arc::new(run.history.clone()),
        Arc::new(Oracle::new()),
    )
}

/// Runs the naming workload on `backend` and checks `naming-gapless`.
pub async fn run_naming<B: StorageBackend>(
    world: Arc<World>,
    config: NamingConfig,
    backend: Arc<B>,
) -> Result<(NamingRun, Report), StorageError> {
    let naming = NamingDriver::new(backend.clone(), world.clone(), config.clone())
        .run()
        .await?;
    let registry = naming_registry(&Arc::new(naming.clone()));
    let ctx = CheckContext::for_backend(
        backend,
        world.catalog.clone(),
        Arc::new(History::new(config.seed, config.workers, &config.profile)),
        Arc::new(Oracle::new()),
    );
    let meta = RunMeta {
        seed: config.seed,
        backend: "naming".to_owned(),
        profile: config.profile.clone(),
        workers: config.workers,
    };
    let report = registry.check_all(&ctx, meta).await;
    Ok((naming, report))
}

/// Checks an already-finished naming run on `backend`.
pub async fn check_naming<B: StorageBackend>(
    world: &Arc<World>,
    naming: &Arc<NamingRun>,
    meta: RunMeta,
    backend: Arc<B>,
) -> Report {
    let registry = naming_registry(naming);
    let ctx = CheckContext::for_backend(
        backend,
        world.catalog.clone(),
        Arc::new(History::new(meta.seed, meta.workers, &meta.profile)),
        Arc::new(Oracle::new()),
    );
    registry.check_all(&ctx, meta).await
}

/// The registry a run checks with, for tests that extend it.
pub fn registry_for(run: &Arc<crate::driver::EngineRun>, world: &Arc<World>) -> InvariantRegistry {
    engine_registry(run, world)
}

/// The profiles the suite runs, from the `SPOT_L7_PROFILE` convention.
pub fn profile_from_env() -> Profile {
    Profile::from_env()
}

/// Writes a report to `path` as pretty JSON.
pub fn write_report(path: &str, report: &Report) -> std::io::Result<()> {
    std::fs::write(path, report.to_json())
}
