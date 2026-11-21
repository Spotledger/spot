//! The naming workload: `spot-naming` gapless allocation under parallel create and rollback
//! (WP-TST-07.T2).
//!
//! The document engine has no naming integration yet (WP-MET-03 wires the naming plan into the
//! lifecycle); WP-DOC-05.T1's promise is at the transaction level: a number is allocated inside
//! the caller's business transaction, so a rolled-back transaction returns it and committed
//! numbers are exactly `1..=n`. This workload drives that promise with the two series the
//! sample app's DocTypes name (`TE-.YYYY.-.#####` for Time Entry, `PRJ-.YYYY.-.####` for
//! Project): each operation allocates a name under [`GapPolicy::GaplessStrict`], writes a
//! `label` marker row with the rendered name in the same transaction, and commits — or rolls
//! back, emulating a `before_save` step that fails after the number was taken.
//!
//! The recorded committed allocations are the oracle the `naming-gapless` invariant checks
//! against the store's series and the marker rows.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use spot_app_api::Document;
use spot_app_api::prelude::{Name, RecordId};
use spot_core::SiteId;
use spot_l7_invariants::schedule::{Profile, Rng};
use spot_naming::{GapPolicy, NamingContext};
use spot_storage_contract::{
    Catalog, DocumentData, DocumentStore, Fields, Isolation, StorageBackend, StorageResult,
    Transaction, TxOptions, Value,
};
use time::UtcOffset;

use crate::world::{LABEL, PROJECT, TIME_ENTRY, World};

/// How a naming run is configured.
#[derive(Clone, Debug)]
pub struct NamingConfig {
    /// The seed.
    pub seed: u64,
    /// Concurrent workers.
    pub workers: u32,
    /// Operations over all workers.
    pub operations: u64,
    /// One in this many operations rolls back after allocating.
    pub rollback_one_in: u64,
    /// The profile name.
    pub profile: String,
}

impl NamingConfig {
    /// The configuration of a named profile, smaller than the engine workload because each
    /// operation is a transaction of its own.
    pub fn for_profile(profile: Profile, seed: u64) -> NamingConfig {
        NamingConfig {
            seed,
            workers: profile.workers(),
            operations: profile.operations().min(20_000),
            rollback_one_in: 4,
            profile: profile.name().to_owned(),
        }
    }
}

/// One committed allocation.
#[derive(Clone, Debug)]
pub struct Allocation {
    /// The series key.
    pub series: String,
    /// The number handed out.
    pub number: u64,
    /// The rendered name.
    pub name: String,
}

/// The exact expected state of a naming run.
#[derive(Clone, Debug, Default)]
pub struct NamingRun {
    /// Committed allocations, in commit order.
    pub allocations: Vec<Allocation>,
    /// Committed numbers per series.
    pub series: BTreeMap<String, BTreeSet<u64>>,
    /// Transactions that allocated and then rolled back.
    pub rollbacks: u64,
    /// Operations that could not allocate at all (a clean storage refusal).
    pub refused: u64,
}

impl NamingRun {
    /// The series keys the run touched.
    pub fn series_keys(&self) -> Vec<String> {
        self.series.keys().cloned().collect()
    }
}

struct Shared {
    run: Mutex<NamingRun>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The naming workload driver.
pub struct NamingDriver<B: StorageBackend> {
    backend: Arc<B>,
    catalog: Arc<Catalog>,
    world: Arc<World>,
    config: NamingConfig,
    shared: Arc<Shared>,
}

impl<B: StorageBackend> NamingDriver<B> {
    /// A driver over `backend` and the world's catalog.
    pub fn new(backend: Arc<B>, world: Arc<World>, config: NamingConfig) -> Self {
        NamingDriver {
            backend,
            catalog: world.catalog.clone(),
            world,
            config,
            shared: Arc::new(Shared {
                run: Mutex::new(NamingRun::default()),
            }),
        }
    }

    /// Runs the naming workload and returns its oracle.
    pub async fn run(self) -> StorageResult<NamingRun> {
        let workers = self.config.workers.max(1);
        let mut tasks = Vec::new();
        for w in 0..workers {
            let backend = self.backend.clone();
            let catalog = self.catalog.clone();
            let world = self.world.clone();
            let shared = self.shared.clone();
            let config = self.config.clone();
            tasks.push(tokio::spawn(async move {
                let mut rng =
                    Rng::new(config.seed ^ (u64::from(w) + 1).wrapping_mul(0x2545_F491_4F6C_DD1D));
                let share = config.operations / u64::from(workers)
                    + u64::from(u64::from(w) < config.operations % u64::from(workers));
                for _ in 0..share {
                    let rollback = rng.chance(1, config.rollback_one_in.max(1));
                    let series_doctype = if rng.chance(1, 2) {
                        TIME_ENTRY
                    } else {
                        PROJECT
                    };
                    let _ = one(
                        &backend,
                        &catalog,
                        &world,
                        &shared,
                        series_doctype,
                        rollback,
                    )
                    .await;
                }
            }));
        }
        for task in tasks {
            task.await.map_err(|e| {
                spot_storage_contract::StorageError::new(
                    spot_storage_contract::ErrorKind::Internal,
                    format!("a naming worker panicked: {e}"),
                )
            })?;
        }
        let run = lock(&self.shared.run).clone();
        Ok(run)
    }
}

async fn one<B: StorageBackend>(
    backend: &Arc<B>,
    catalog: &Arc<Catalog>,
    world: &Arc<World>,
    shared: &Arc<Shared>,
    series_doctype: &str,
    rollback: bool,
) -> StorageResult<()> {
    let options = TxOptions::new(Name::new("l7_naming")?, Isolation::ReadCommitted);
    let mut tx = backend.begin(catalog.clone(), options).await?;
    let doctype = Name::new(series_doctype)?;
    let doc = Document::new(doctype.clone(), RecordId::new_v7());
    let ctx = NamingContext::new(
        SiteId::new("l7-engine").map_err(|e| {
            spot_storage_contract::StorageError::new(
                spot_storage_contract::ErrorKind::InvalidValue,
                e.to_string(),
            )
        })?,
        world.now,
        UtcOffset::UTC,
    );
    let ticket = match world
        .namer
        .ticket(&doctype, &doc, &ctx, GapPolicy::GaplessStrict, &mut tx)
        .await
    {
        Ok(t) => t,
        Err(_) => {
            let _ = tx.rollback().await;
            lock(&shared.run).refused += 1;
            return Ok(());
        }
    };
    let number = ticket.number();
    let series = ticket.series().map(|s| s.as_str().to_owned());
    let name = ticket.commit_name();
    let mut fields = Fields::new();
    fields.insert(Name::new("name")?, Value::Text(name.as_str().to_owned()));
    fields.insert(Name::new("colour")?, Value::Text("#445566".to_owned()));
    let data = DocumentData {
        entity: Name::new(LABEL)?,
        id: RecordId::new_v7(),
        fields,
        children: BTreeMap::new(),
    };
    tx.insert(&data).await?;
    if rollback {
        tx.rollback().await?;
        lock(&shared.run).rollbacks += 1;
        return Ok(());
    }
    tx.commit().await?;
    if let (Some(number), Some(series)) = (number, series) {
        let mut run = lock(&shared.run);
        run.series.entry(series.clone()).or_default().insert(number);
        run.allocations.push(Allocation {
            series,
            number,
            name: name.as_str().to_owned(),
        });
    }
    Ok(())
}
