//! The configuration and the production wiring of the `tenants` module (`WP-CTL-01.T7`): the real control store, provisioning executor, licence issuer, final-backup archiver and approval verifier. Split from `tenants.rs` for the 1,000-line gate.

use super::tenants_kv::Kv;
use super::tenants_kv::PgKv;
use super::tenants_ports::AuditBridge;
use super::tenants_ports::InviteQueue;
use super::tenants_ports::KvLicenceStore;
use super::tenants_ports::KvTelemetry;
use super::tenants_ports::ProvisionerInputs;
use super::tenants_ports::RUNNER_FACTORY;
use super::tenants_ports::SharedRunner;
use super::tenants_ports::SignedApprovals;
use super::tenants_ports::leaked_runtime;
use super::tenants_state::Deps;
use super::tenants_state::PortSet;
use super::tenants_state::fill_random;
use crate::CpCode;
use crate::CpError;
use deadpool_postgres::Pool;
use serde::Deserialize;
use spot_core::Clock;
use spot_mgmt_api::ports::AuditSink as MgmtAudit;
use spot_tenancy::Cluster;
use spot_tenancy::ClusterId;
use spot_tenancy::DatabaseId;
use spot_tenancy::Region;
use spot_tenancy_adapters::IamAdapter;
use spot_tenancy_adapters::LicenceIssuer;
use spot_tenancy_adapters::LicenceWindow;
use spot_tenancy_adapters::SchemaAdapter;
use spot_tenancy_adapters::SeedAdapter;
use spot_tenancy_adapters::SnapshotArchiver;
use spot_tenancy_adapters::TelemetryAdapter;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// One database that can hold tenants.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClusterConfig {
    /// The cluster id (lower-case identifier).
    pub cluster: String,
    /// The database id inside the cluster.
    pub database: String,
    /// The residency region of the hardware.
    pub region: String,
    /// The most live tenants the database may hold.
    pub capacity: u32,
    /// Whether it takes new tenants.
    #[serde(default = "default_true")]
    pub accepting: bool,
    /// The environment variable holding the connection URL of the placement database.
    pub url_env: String,
    /// The environment variable holding the connection string the final backup dumps with.
    pub snapshot_url_env: Option<String>,
}

/// The configuration of the module (`SPOT_CP_TENANTS_CONFIG` names the TOML file).
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantsConfig {
    /// The environment variable holding the URL of the provisioning role (CREATE DATABASE only).
    pub provisioner_url_env: String,
    /// The placement databases.
    pub clusters: Vec<ClusterConfig>,
    /// Tenants per edition; an edition without an entry is unlimited.
    #[serde(default)]
    pub editions: BTreeMap<String, u32>,
    /// The delay before a deprovisioning may be confirmed.
    #[serde(default = "default_delay")]
    pub confirm_delay_secs: u64,
    /// The retention before data may be deleted, in days (not below the plane's minimum).
    #[serde(default = "default_retention")]
    pub retention_days: u64,
    /// The directory final backups are written under.
    pub archive_root: PathBuf,
    /// The directory of the PostgreSQL client binaries (`pg_dump`).
    pub pg_bin: PathBuf,
    /// The environment variable holding the licence signing seed (64 hex characters).
    pub licence_key_env: Option<String>,
    /// The key id the licences are signed under.
    #[serde(default = "default_key_id")]
    pub licence_key_id: String,
    /// The seconds between runner passes.
    #[serde(default = "default_poll")]
    pub poll_secs: u64,
    /// The attempts a failing workflow gets per process before the runner leaves it `failed`.
    #[serde(default = "default_attempts")]
    pub max_attempts: u32,
}

fn default_true() -> bool {
    true
}
fn default_delay() -> u64 {
    86_400
}
fn default_retention() -> u64 {
    30
}
fn default_key_id() -> String {
    "licence-1".to_owned()
}
fn default_poll() -> u64 {
    5
}
fn default_attempts() -> u32 {
    5
}

impl TenantsConfig {
    /// Parses and validates the module configuration.
    ///
    /// # Errors
    /// `cp.invalid` when the text is not a valid configuration.
    pub fn from_toml(text: &str) -> Result<Self, CpError> {
        let config: Self =
            toml::from_str(text).map_err(|_| CpError::invalid(&["tenants_config"]))?;
        if config.clusters.is_empty() || config.poll_secs == 0 || config.max_attempts == 0 {
            return Err(CpError::invalid(&["tenants_config"]));
        }
        Ok(config)
    }
}

pub(crate) fn env(name: &str) -> Result<String, CpError> {
    std::env::var(name).map_err(|_| CpError::with_reason(CpCode::Internal, "tenants.env_missing"))
}

pub(crate) fn internal(reason: &'static str) -> CpError {
    CpError::with_reason(CpCode::Internal, reason)
}

pub(crate) async fn production(
    config: TenantsConfig,
    pool: Pool,
    clock: Arc<dyn Clock>,
    audit: Arc<dyn MgmtAudit>,
    cp_config: &crate::config::Config,
) -> Result<Deps, CpError> {
    let runtime = leaked_runtime();
    let store = spot_tenancy_pg::PgControlStore::new(
        pool.clone(),
        Arc::clone(&runtime),
        Arc::new(spot_tenancy_pg::Faults::new()),
    );
    store
        .prepare()
        .await
        .map_err(|_| internal("tenants.control_schema"))?;
    let mut clusters = Vec::new();
    let mut placements = Vec::new();
    let mut connections = Vec::new();
    for entry in &config.clusters {
        let url = env(&entry.url_env)?;
        let pg =
            spot_postgres::PgConfig::from_url(&url).map_err(|_| internal("tenants.cluster_url"))?;
        let placement_pool = spot_postgres::PgBackend::connect(pg.clone())
            .await
            .map_err(|_| internal("tenants.cluster_unreachable"))?
            .pool();
        let (cluster_id, database_id) = (
            ClusterId::new(entry.cluster.clone()).map_err(|_| internal("tenants.cluster_id"))?,
            DatabaseId::new(entry.database.clone()).map_err(|_| internal("tenants.cluster_id"))?,
        );
        clusters.push(Cluster {
            cluster: cluster_id.clone(),
            database: database_id.clone(),
            region: Region::new(entry.region.clone())
                .map_err(|_| internal("tenants.cluster_region"))?,
            capacity: entry.capacity,
            accepting: entry.accepting,
        });
        if let Some(name) = &entry.snapshot_url_env {
            connections.push(((cluster_id.clone(), database_id.clone()), env(name)?));
        }
        placements.push(spot_tenancy_pg::PlacementDb {
            cluster: cluster_id,
            database: database_id,
            config: pg,
            pool: placement_pool,
        });
    }
    let admin = spot_postgres::PgConfig::from_url(&env(&config.provisioner_url_env)?)
        .map_err(|_| internal("tenants.provisioner_url"))?;
    let factory = RUNNER_FACTORY
        .get()
        .ok_or_else(|| internal("tenants.no_runner_factory"))?;
    let runner = factory(ProvisionerInputs {
        admin,
        placements,
        runtime: Arc::clone(&runtime),
    })?;
    let kv: Arc<dyn Kv> = Arc::new(PgKv::new(pool.clone()));
    let licence = {
        let seed = config
            .licence_key_env
            .as_deref()
            .map(env)
            .transpose()?
            .and_then(|t| crate::config::decode_key(&t));
        let seed = seed.ok_or_else(|| internal("tenants.licence_key"))?;
        let issuer = spot_license::issuing::Issuer::new(
            ed25519_dalek::SigningKey::from_bytes(&seed),
            config.licence_key_id.clone(),
        );
        LicenceIssuer::new(
            issuer,
            config.licence_key_id.clone(),
            KvLicenceStore::new(Arc::clone(&kv), Arc::clone(&runtime)),
            Arc::clone(&clock),
            LicenceWindow::default(),
            spot_license::Entitlements::none(),
        )
    };
    let provider = spot_snapshot::PgDumpProvider::new(config.pg_bin.clone(), Arc::clone(&clock));
    let archiver = SnapshotArchiver::new(
        provider,
        connections,
        config.archive_root.clone(),
        Arc::clone(&runtime),
    );
    let keys: Vec<[u8; 32]> = cp_config
        .administrator_keys
        .iter()
        .filter_map(|k| crate::config::decode_key(k))
        .collect();
    let nonces =
        spot_signed_command_pg::PgNonceStore::with_skew(pool, spot_mgmt_api::MAX_CLOCK_SKEW_SECS);
    nonces
        .prepare()
        .await
        .map_err(|_| internal("tenants.nonce_table"))?;
    let plane = spot_tenancy::Config::default();
    let retention_ms = config
        .retention_days
        .saturating_mul(86_400_000)
        .max(plane.min_retention_ms);
    Ok(Deps {
        store: Arc::new(store),
        ports: PortSet {
            schema: Arc::new(SchemaAdapter::new(SharedRunner(Arc::clone(&runner)))),
            seeder: Arc::new(SeedAdapter::new(SharedRunner(runner))),
            iam: Arc::new(IamAdapter::new(InviteQueue {
                kv: Arc::clone(&kv),
                runtime: Arc::clone(&runtime),
            })),
            licence: Arc::new(licence),
            telemetry: Arc::new(TelemetryAdapter::new(KvTelemetry {
                kv: Arc::clone(&kv),
                runtime: Arc::clone(&runtime),
            })),
            archiver: Arc::new(archiver),
        },
        audit: Arc::new(AuditBridge { audit, runtime }),
        kv,
        approvals: Arc::new(SignedApprovals::new(
            &keys,
            Box::new(nonces),
            &cp_config.audience,
        )),
        clusters,
        plane,
        editions: config.editions.clone(),
        confirm_delay_ms: config.confirm_delay_secs.saturating_mul(1000),
        retention_ms,
        entropy: Arc::new(|| {
            let mut bytes = [0u8; 10];
            fill_random(&mut bytes);
            bytes
        }),
        poll_ms: config.poll_secs.saturating_mul(1000),
        max_attempts: config.max_attempts,
    })
}
