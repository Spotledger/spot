//! The audit inputs of one site: the live side over the site pool, a scratch side in a database
//! of its own, and the journal data the rebuild reads.
//!
//! What the host supplies is [`JournalSource`]: the journal store, the verifying and snapshot
//! keys, the upcasters, the catalogue, the audit options (segments from the verified manifests,
//! forks, derived entities, the tier's RTO) and the financial fingerprint declarations the apps
//! registered. Everything database-shaped is built here.

use std::rc::Rc;
use std::sync::Arc;

use spot_image_upcast::Registry as UpcastRegistry;
use spot_journal_audit::DerivedEntities;
use spot_journal_audit_job::{AuditInputs, AuditJobError, TenantInputs};
use spot_journal_audit_pg::{FingerprintQuery, PgLiveSide, PgScratchSide};
use spot_journal_audit_run::AuditOptions;
use spot_journal_manifest::VerifyingKeys;
use spot_journal_snapshot::SnapshotKeys;
use spot_journal_store::{JournalStore, TenantId};
use spot_pg_bridge::Bridge;
use spot_storage_contract::Catalog;

use crate::error::HostError;
use crate::scratch::{ScratchFactory, ScratchWithDatabase};

/// Everything about one tenant's journal that one audit run needs, opened for that run only.
pub struct JournalData {
    /// The audit options (tenant, source, segments, forks, RTO).
    pub options: AuditOptions<'static>,
    /// The journal store the rebuild reads.
    pub store: Arc<dyn JournalStore>,
    /// The tenant's public manifest keys.
    pub manifest_keys: Arc<VerifyingKeys>,
    /// The tenant's snapshot keys.
    pub snapshot_keys: Arc<dyn SnapshotKeys>,
    /// The upcasters.
    pub registry: Rc<UpcastRegistry>,
    /// The catalogue of the site (its tables are what both sides read).
    pub catalog: Arc<Catalog>,
    /// Entities derived from others, left out of the compare.
    pub derived: DerivedEntities,
    /// The financial fingerprints the apps registered (none: the compare covers records only).
    pub fingerprints: Vec<FingerprintQuery>,
}

/// Opens the journal data of a tenant of this host.
pub trait JournalSource: Send + Sync {
    /// The data for `tenant`.
    ///
    /// # Errors
    /// [`HostError::Source`] (or `TenantMismatch`) when it cannot be opened.
    fn open(&self, tenant: &TenantId) -> Result<JournalData, HostError>;
}

/// Where run ids and the time come from (the library reads neither a clock nor randomness).
pub trait RunClock: Send + Sync {
    /// A fresh run id: lowercase letters and digits, starting with a letter, unique per run.
    fn run_id(&self) -> String;
    /// Now, in Unix milliseconds.
    fn now_millis(&self) -> i64;
}

/// The site's [`AuditInputs`].
pub struct SiteAuditInputs {
    /// This site's tenant (one tenant, one database: the inputs refuse any other).
    tenant: TenantId,
    live: Arc<Bridge>,
    source: Arc<dyn JournalSource>,
    scratch: Arc<ScratchFactory>,
    clock: Arc<dyn RunClock>,
}

impl SiteAuditInputs {
    /// Inputs for `tenant`, whose live database `live` reaches.
    #[must_use]
    pub fn new(
        tenant: TenantId,
        live: Arc<Bridge>,
        source: Arc<dyn JournalSource>,
        scratch: Arc<ScratchFactory>,
        clock: Arc<dyn RunClock>,
    ) -> Self {
        Self {
            tenant,
            live,
            source,
            scratch,
            clock,
        }
    }

    fn build(&self, tenant: &TenantId) -> Result<TenantInputs, HostError> {
        // One tenant per run: the live side is this site's database, so another site's tenant is
        // refused before anything is opened.
        if tenant != &self.tenant {
            return Err(HostError::TenantMismatch);
        }
        let data = self.source.open(tenant)?;
        if data.options.tenant != *tenant {
            return Err(HostError::TenantMismatch);
        }
        let run = self.clock.run_id();
        let now = self.clock.now_millis();
        let live = PgLiveSide::open(
            Arc::clone(&self.live),
            tenant.as_str(),
            Arc::clone(&data.catalog),
        )
        .and_then(|side| side.with_derived(data.derived.clone()))
        .and_then(|side| side.with_fingerprints(data.fingerprints.clone()))
        .map_err(|_| HostError::Source)?;
        // The database first, then the side inside it; if the side cannot be made the database
        // is dropped by its guard on the way out.
        let database = self.scratch.create(tenant.as_str(), &run, now)?;
        let bridge = self.scratch.open_bridge(database.name())?;
        let side = PgScratchSide::create(
            bridge,
            tenant.as_str(),
            Arc::clone(&data.catalog),
            &run,
            now,
        )
        .and_then(|side| side.with_derived(data.derived.clone()))
        .and_then(|side| side.with_fingerprints(data.fingerprints.clone()))
        .map_err(|_| HostError::Scratch("prepared"))?;
        Ok(TenantInputs {
            options: data.options,
            live: Box::new(live),
            scratch: Box::new(ScratchWithDatabase::new(side, database)),
            store: data.store,
            manifest_keys: data.manifest_keys,
            snapshot_keys: data.snapshot_keys,
            registry: data.registry,
            classifier: None,
            strict_topics: true,
        })
    }
}

impl AuditInputs for SiteAuditInputs {
    fn open(&self, tenant: &TenantId) -> Result<TenantInputs, AuditJobError> {
        self.build(tenant).map_err(|error| match error {
            HostError::TenantMismatch => AuditJobError::TenantMismatch { side: "inputs" },
            _ => AuditJobError::ProviderFailed { provider: "inputs" },
        })
    }
}
