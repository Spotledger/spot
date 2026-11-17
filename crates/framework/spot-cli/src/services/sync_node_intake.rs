//! The edge's apply path: the production `IntakeFactory` the `sync_node` service builds its intake
//! through (WP-RTB-02.T12; ADR-062 sections 3 and 4, ADR-064 section 7, ADR-065 section 1).
//!
//! This module provides one port, `dyn IntakeFactory`, on a site with `role = "edge"` (on any
//! other role it does nothing). The factory builds, on the sync worker's plain thread:
//!
//! * the real apply path (`spot_sync_node_pg::PgBundleApply`) with the site's adapters:
//!   [`SiteClock`] (the site clock, never the wall clock), the ownership check
//!   (`spot_replica_apply::PartitionOwners` over the site's partition registry), the quarantine
//!   port ([`StoreQuarantine`] over the site's quarantine store) and the audit marker sink;
//! * the PostgreSQL parking store and the node catalog;
//! * a `DurableIntake`, **restored** from the database before the first call: the applied journal
//!   prefix, the skew holds, the parked bundles' first positions, the repair list and the stored
//!   serve cursor ([`PgState`]). Until that restore has completed nothing is acknowledged: a
//!   restore that fails makes the worker answer every call with the failure.
//!
//! # Not wired means not running
//!
//! The adapters come from other modules' ports (`dyn PartitionRegistry`, `dyn QuarantineStore`,
//! `dyn ReplicaAudit`, `PgNodeCatalog` and [`NodeReportSource`]). When one is missing the factory
//! is **not** provided and the `sync_node` service says its apply path is not wired: nothing is
//! applied on a guess, and a warning names the missing parts (never a value).
//!
//! # Settings
//!
//! ```toml
//! [services.sync_node_intake]
//! max_skew = 0                 # metadata versions of skew accepted from the sender (ADR-066)
//! parking_max_bundles = 1000   # per tenant
//! parking_max_bytes = 67108864 # per tenant
//! parking_max_age_secs = 86400 # a parked bundle older than this goes to the repair list
//! apply_deadline_secs = 60     # one bundle's transaction
//! max_writes = 10000           # writes of one bundle's transaction
//! ```
//!
//! # The bundle image convention
//!
//! A bundle record's image is the `spot_replica_apply::RecordPayload` JSON whose document entity
//! and id equal the bundle key. The apply path refuses anything else (the record is quarantined
//! with its reason code and the bundle is refused whole), so a sender cannot smuggle a record
//! under another key.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use spot_core::Clock;
use spot_events_contract::SourceId;
use spot_pg_bridge::Bridge;
use spot_postgres::PgBackend;
use spot_quarantine::{NewItem, QuarantineStore, RecordRef};
use spot_replica_apply::{
    OriginFields, OwnerCheck, PartitionOwners, QuarantinePort, Reason, ReceivedRecord,
    ReplicaAudit, ScopeFields,
};
use spot_sync_node::{HoldSet, NodeConfig, NodeIntake, ParkingLimits, RepairItem, RestoreState};
use spot_sync_node_host::intake::{DurableIntake, StateStore, parked_first_positions};
use spot_sync_node_host::pg::PgServeCursor;
use spot_sync_node_host::{Error as HostError, Intake, IntakeFactory, IntakeRequest};
use spot_sync_node_pg::{
    ApplyClock, ApplyEnv, ApplyLimits, CatalogSource, Db, MAX_LIST, PgBundleApply, PgNodeCatalog,
    PgNodeError, PgNodeState, PgParkingStore, PgRepairList, StaticCatalog,
};
use spot_sync_protocol::NodeId;
use spot_sync_skew::NodeReport;

use crate::services::SiteCtx;
use crate::services::node_role;
use crate::{Error, Result};

/// The module's name and its `[services.<name>]` table.
pub const NAME: &str = "sync_node_intake";
/// After `node_role` (1005), whose role this module reads, and before `sync_node` (1200), which
/// reads the port this module provides.
pub const ORDER: u32 = 1150;

/// Stable code: a restore could not read the saved state whole (more repair items than one read
/// returns).
pub const RESTORE_INCOMPLETE: &str = "sync_node_intake.restore_incomplete";

/// `[services.sync_node_intake]`.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Metadata versions of skew accepted from the sender.
    pub max_skew: Option<u64>,
    /// Most parked bundles per tenant.
    pub parking_max_bundles: Option<usize>,
    /// Most parked image bytes per tenant.
    pub parking_max_bytes: Option<usize>,
    /// A parked bundle older than this many seconds goes to the repair list.
    pub parking_max_age_secs: Option<u64>,
    /// Deadline of one bundle's transaction, seconds.
    pub apply_deadline_secs: Option<u64>,
    /// Most writes of one bundle's transaction.
    pub max_writes: Option<u32>,
}

/// The bounds a [`Settings`] resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bounds {
    /// The metadata skew accepted.
    pub max_skew: u64,
    /// The parking limits.
    pub parking: ParkingLimits,
    /// The limits of one bundle's apply.
    pub apply: ApplyLimits,
}

impl Settings {
    /// The resolved bounds, with the defaults of the library crates.
    ///
    /// # Errors
    ///
    /// [`Error`] naming the setting that is zero or out of range.
    pub fn bounds(&self) -> Result<Bounds> {
        let defaults = ParkingLimits::default();
        let apply_defaults = ApplyLimits::default();
        let parking = ParkingLimits {
            max_bundles: self.parking_max_bundles.unwrap_or(defaults.max_bundles),
            max_bytes: self.parking_max_bytes.unwrap_or(defaults.max_bytes),
            max_age_ms: self
                .parking_max_age_secs
                .map_or(Some(defaults.max_age_ms), |secs| secs.checked_mul(1000))
                .ok_or_else(|| {
                    Error("sync_node_intake: parking_max_age_secs is out of range".to_owned())
                })?,
        };
        parking.validate().map_err(|_| {
            Error("sync_node_intake: every parking limit must be above zero".into())
        })?;
        let deadline = self
            .apply_deadline_secs
            .map_or(apply_defaults.deadline, Duration::from_secs);
        let max_writes = self.max_writes.unwrap_or(apply_defaults.max_writes);
        if deadline.is_zero() || max_writes == 0 {
            return Err(Error(
                "sync_node_intake: apply_deadline_secs and max_writes must be above zero".into(),
            ));
        }
        Ok(Bounds {
            max_skew: self.max_skew.unwrap_or(0),
            parking,
            apply: ApplyLimits {
                deadline,
                max_writes,
            },
        })
    }
}

/// The node report (software version, metadata version, migration epoch, decodable majors) the
/// intake triages version skew against. A module that knows them provides it as a port.
pub trait NodeReportSource: Send + Sync + 'static {
    /// The report now.
    fn report(&self) -> NodeReport;
}

/// The site clock as the apply path's instant: Unix microseconds, read from the injected clock.
pub struct SiteClock(pub Arc<dyn Clock>);

impl ApplyClock for SiteClock {
    fn now_micros(&self) -> i64 {
        i64::try_from(self.0.now_utc().unix_timestamp_nanos() / 1_000).unwrap_or(i64::MAX)
    }
}

/// The quarantine port over the site's quarantine store: a record the ownership check refused is
/// held there, unchanged, so the stream moves on and nothing is dropped silently (ADR-062 G2, G6).
/// The detail stored is the reason's code, never a value.
pub struct StoreQuarantine(pub Arc<dyn QuarantineStore>);

impl QuarantinePort for StoreQuarantine {
    fn hold<'a>(
        &'a self,
        record: &'a ReceivedRecord,
        reason: &'a Reason,
        at: i64,
    ) -> Pin<Box<dyn Future<Output = std::result::Result<(), Reason>> + Send + 'a>> {
        Box::pin(async move {
            let item = NewItem::held(
                record.source.clone(),
                record.seq,
                RecordRef::of(record),
                reason.code(),
                reason.code(),
                at,
            );
            self.0
                .insert(item, record.clone())
                .await
                .map(|_| ())
                .map_err(|error| Reason::QuarantineFailed {
                    why: error.code().to_owned(),
                })
        })
    }
}

/// The restore state of one tenant, read from the node's PostgreSQL tables and written back after
/// each receive.
pub struct PgState {
    bridge: Arc<Bridge>,
    state: PgNodeState,
    repair: PgRepairList,
    source: String,
}

impl PgState {
    /// The state store of the tenant whose bundles come from `source`.
    #[must_use]
    pub fn new(bridge: Arc<Bridge>, db: &Db, source: &SourceId) -> Self {
        Self {
            bridge,
            state: PgNodeState::new(db.clone()),
            repair: PgRepairList::new(db.clone()),
            source: source.as_str().to_owned(),
        }
    }
}

/// A store failure as the host's error: the code only, never a value.
fn store_error(error: &PgNodeError) -> HostError {
    HostError::store_failed(format!(
        "the node's state could not be read or written ({})",
        error.code()
    ))
}

impl StateStore for PgState {
    fn load(&mut self, tenant: &str) -> spot_sync_node_host::Result<RestoreState> {
        let applied_prefix = self
            .state
            .applied_prefix(tenant, &self.source)
            .map_err(|e| store_error(&e))?;
        let holds = self.state.load_holds(tenant).map_err(|e| store_error(&e))?;
        let waiting = self.repair.len(tenant).map_err(|e| store_error(&e))?;
        if waiting > u64::try_from(MAX_LIST).unwrap_or(u64::MAX) {
            // One read returns at most `MAX_LIST` items; a restore from part of the list would
            // let the node run ahead of a bundle it cannot see. Refuse, and say so.
            return Err(HostError::new(
                RESTORE_INCOMPLETE,
                format!(
                    "the repair list holds {waiting} items, more than one restore reads ({MAX_LIST}); clear it before the node starts"
                ),
            ));
        }
        let repair: Vec<RepairItem> = self
            .repair
            .list(tenant, MAX_LIST)
            .map_err(|e| store_error(&e))?
            .into_iter()
            .map(|stored| stored.item)
            .collect();
        let parked_first_positions = parked_first_positions(&self.bridge, tenant)?;
        Ok(RestoreState {
            applied_prefix,
            served_prefix: None,
            holds,
            parked_first_positions,
            repair,
        })
    }

    fn save_holds(&mut self, tenant: &str, holds: &HoldSet) -> spot_sync_node_host::Result<()> {
        self.state
            .save_holds(tenant, holds)
            .map_err(|e| store_error(&e))
    }

    fn push_repair(&mut self, items: &[RepairItem]) -> spot_sync_node_host::Result<()> {
        self.repair.push(items).map_err(|e| store_error(&e))
    }
}

type Durable = DurableIntake<PgBundleApply, PgParkingStore, PgNodeCatalog, PgServeCursor, PgState>;

/// The durable intake plus a look at the failures the store traits could not return
/// (`has`, `has_tombstone`, parking calls answer a plain value): any such failure of a call is
/// reported as that call's error, so the loop backs off and says so, instead of carrying on.
struct Watched {
    inner: Durable,
    apply: PgBundleApply,
    parking: PgParkingStore,
}

impl Intake for Watched {
    fn cursor(&mut self) -> spot_sync_node_host::Result<u64> {
        self.inner.cursor()
    }

    fn receive(
        &mut self,
        items: Vec<spot_sync_node_host::envelope::Served>,
        quarantined: usize,
        now_ms: u64,
    ) -> spot_sync_node_host::Result<spot_sync_node_host::Received> {
        let result = self.inner.receive(items, quarantined, now_ms);
        let failure = self
            .apply
            .take_failure()
            .or_else(|| self.parking.take_failure());
        match (result, failure) {
            (Err(error), _) => Err(error),
            (Ok(_), Some(failure)) => Err(store_error(&failure)),
            (Ok(received), None) => Ok(received),
        }
    }
}

/// What the factory builds the apply path from.
pub struct SiteParts {
    /// The site's database (the apply transaction's backend).
    pub backend: Arc<PgBackend>,
    /// The ownership decision (G1).
    pub owner: Arc<dyn OwnerCheck>,
    /// Where the `replicated` marker is written.
    pub audit: Arc<dyn ReplicaAudit>,
    /// Where a refused record is held.
    pub quarantine: Arc<dyn QuarantinePort>,
    /// The instant of an apply.
    pub clock: Arc<dyn ApplyClock>,
    /// The storage catalog the apply path validates records against.
    pub catalog: Arc<dyn CatalogSource>,
    /// The node catalog (links, topics and majors of the running metadata generation).
    pub node_catalog: PgNodeCatalog,
    /// The report the intake triages version skew against.
    pub report: Arc<dyn NodeReportSource>,
    /// The bounds.
    pub bounds: Bounds,
}

/// The production `IntakeFactory`.
pub struct SiteIntakeFactory {
    parts: SiteParts,
}

impl SiteIntakeFactory {
    /// A factory over `parts`.
    #[must_use]
    pub fn new(parts: SiteParts) -> Self {
        Self { parts }
    }
}

impl IntakeFactory for SiteIntakeFactory {
    fn metadata_epoch(&self) -> u64 {
        self.parts.report.report().migration_epoch
    }

    fn build(&self, request: IntakeRequest) -> spot_sync_node_host::Result<Box<dyn Intake>> {
        let parts = &self.parts;
        let db =
            Db::new(Arc::clone(&request.bridge), request.pool_size).map_err(|e| store_error(&e))?;
        // The tables are created by `spot migrate`, never here: a missing one is a refusal.
        db.require_tables().map_err(|e| store_error(&e))?;
        let env = ApplyEnv {
            backend: Arc::clone(&parts.backend),
            catalog: Arc::clone(&parts.catalog),
            owner: Arc::clone(&parts.owner),
            audit: Arc::clone(&parts.audit),
            quarantine: Arc::clone(&parts.quarantine),
            clock: Arc::clone(&parts.clock),
            origin: OriginFields::default(),
            max_skew: parts.bounds.max_skew,
            limits: parts.bounds.apply,
            failpoints: None,
        };
        let apply = PgBundleApply::new(
            db.clone(),
            request.tenant.clone(),
            request.hub_source.clone(),
            env,
        )
        .map_err(|e| store_error(&e))?;
        let parking = PgParkingStore::new(db.clone());
        let node = NodeIntake::new(
            NodeConfig {
                hub_source: request.hub_source.clone(),
                limits: parts.bounds.parking,
                // One node serves one tenant: its own database.
                max_tenants: 1,
            },
            apply.clone(),
            parking.clone(),
            parts.node_catalog.clone(),
            parts.report.report(),
        )
        .map_err(|e| HostError::store_failed(format!("the intake did not build ({})", e.code())))?;
        let state = PgState::new(Arc::clone(&request.bridge), &db, &request.hub_source);
        let inner = DurableIntake::restore(
            node,
            request.cursors.serve(),
            state,
            request.tenant,
            request.hub_source,
        )?;
        Ok(Box::new(Watched {
            inner,
            apply,
            parking,
        }))
    }
}

/// Registers the factory when the site is an edge and every part of the apply path is provided.
///
/// # Errors
///
/// [`Error`] for settings that do not validate or a partition node name that is not a node id.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default(NAME)?;
    let bounds = settings.bounds()?;
    if !ctx.assembling() {
        return Ok(());
    }
    let is_edge = ctx
        .port::<node_role::ActiveRole>()
        .is_some_and(|active| active.role().name() == "edge");
    if !is_edge {
        return Ok(());
    }
    let clock = Arc::clone(ctx.clock());
    let registry = ctx.port::<dyn spot_partition::PartitionRegistry>();
    let quarantine = ctx.port::<dyn QuarantineStore>();
    let audit = ctx.port::<dyn ReplicaAudit>();
    let node_catalog = ctx.port::<PgNodeCatalog>();
    let report = ctx.port::<dyn NodeReportSource>();
    let mut missing: Vec<&str> = Vec::new();
    if registry.is_none() {
        missing.push("partition registry");
    }
    if quarantine.is_none() {
        missing.push("quarantine store");
    }
    if audit.is_none() {
        missing.push("replica audit");
    }
    if node_catalog.is_none() {
        missing.push("node catalog");
    }
    if report.is_none() {
        missing.push("node report");
    }
    let (Some(registry), Some(quarantine), Some(audit), Some(node_catalog), Some(report)) =
        (registry, quarantine, audit, node_catalog, report)
    else {
        tracing::warn!(
            missing = ?missing,
            "sync_node_intake: the apply path is not wired; the edge applies nothing"
        );
        return Ok(());
    };
    let env = *ctx.env()?;
    let Some(profile) = env.site.partition.as_ref() else {
        return Err(Error(
            "sync_node_intake: an edge needs a `[partition]` section naming its node".to_owned(),
        ));
    };
    let local = NodeId::new(&profile.node)
        .map_err(|e| Error(format!("sync_node_intake: partition node: {e}")))?;
    let mut fields = ScopeFields::new();
    for entry in &profile.scope {
        fields = fields.with(&entry.key, &entry.field);
    }
    let owner: Arc<dyn OwnerCheck> =
        Arc::new(PartitionOwners::new(registry, Arc::new(fields), local));
    let parts = SiteParts {
        backend: Arc::new(env.backend.clone()),
        owner,
        audit,
        quarantine: Arc::new(StoreQuarantine(quarantine)),
        clock: Arc::new(SiteClock(clock)),
        catalog: Arc::new(StaticCatalog(Arc::clone(&env.permission.catalog))),
        node_catalog: (*node_catalog).clone(),
        report,
        bounds,
    };
    ctx.provide::<dyn IntakeFactory>(Arc::new(SiteIntakeFactory::new(parts)))?;
    Ok(())
}
