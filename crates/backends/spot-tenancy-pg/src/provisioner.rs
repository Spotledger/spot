//! The real provisioning executor: a tenant's schema in its placement database, its baseline
//! migration and its seed.
//!
//! [`PgProvisioner`] implements the five provisioning ports of `spot-tenancy` for one control
//! plane. The two that touch the database are real:
//!
//! * [`SchemaPort::create`](spot_tenancy::ports::SchemaPort::create) ensures the placement
//!   database exists (through `spot-postgres`'s admin helpers) and creates the tenant's schema,
//!   deriving its name from the tenant id and never from the tenant-chosen name. It is
//!   idempotent: an existing schema is accepted only when it is empty or carries this tenant's
//!   ownership comment, so a resume after a crash does not fail and a foreign schema is never
//!   claimed. `drop_schema` is `DROP SCHEMA IF EXISTS ... CASCADE`.
//! * [`SiteSeeder::seed`](spot_tenancy::ports::SiteSeeder::seed) sets `search_path` to the tenant
//!   schema, runs the baseline catalogue through `spot-pg-migrate`'s planner and executor
//!   ([`baseline`](crate::baseline)), and inserts the tenant's seed row. Every statement is
//!   unqualified and therefore lands in the tenant's own schema; nothing outside it is touched.
//!
//! The IAM, licence and telemetry ports are inert here: their work packages (WP-CTL-04, WP-IAM)
//! own the real adapters. They are implemented so the control plane's journaled workflow can run
//! end to end on a real database, and they participate in the kill seam like the real steps.

use std::collections::BTreeMap;
use std::sync::Arc;

use deadpool_postgres::Pool;
use spot_app_api::AppResult;
use spot_postgres::PgConfig;
use spot_storage_contract::{Catalog, EntityDef, EntityKind, FieldDef, FieldType, Name};
use spot_tenancy::ports::{
    IamPort, LicencePort, PortResult, SchemaPort, SiteSeeder, TelemetryPort,
};
use spot_tenancy::{ClusterId, DatabaseId, Step, Tenant};

use crate::baseline;
use crate::blocking::block_on;
use crate::error::{invalid_baseline, is_undefined_table, step};
use crate::fault::Faults;

/// The seed table every tenant gets, and the row the seeder writes.
pub const SEED_TABLE: &str = "tenant_seed";

/// One database that can hold tenant schemas: the coordinates the placement map names, the
/// config an admin connection uses, and the pool every statement checks out from.
pub struct PlacementDb {
    /// The cluster the database lives on.
    pub cluster: ClusterId,
    /// The database inside that cluster.
    pub database: DatabaseId,
    /// Connection settings for the database; its `dbname` is the database to create if absent.
    pub config: PgConfig,
    /// The pool provisioning checks out from.
    pub pool: Pool,
}

impl std::fmt::Debug for PlacementDb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PlacementDb")
            .field("cluster", &self.cluster)
            .field("database", &self.database)
            .finish_non_exhaustive()
    }
}

/// The provisioning executor over a set of placement databases.
pub struct PgProvisioner {
    admin: PgConfig,
    placements: BTreeMap<(ClusterId, DatabaseId), PlacementDb>,
    catalog: Catalog,
    runtime: Arc<tokio::runtime::Runtime>,
    faults: Arc<Faults>,
}

impl std::fmt::Debug for PgProvisioner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgProvisioner")
            .field("placements", &self.placements.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl PgProvisioner {
    /// A provisioner over `placements`, whose tenants get `app_catalog` plus the built-in
    /// `tenant_seed` entity.
    ///
    /// `admin` is used only to create a placement database that does not exist yet, and
    /// `runtime` is what the synchronous ports block on. `faults` is the kill seam used by the
    /// resume tests (an unarmed [`Faults`] is inert).
    ///
    /// # Errors
    /// `tenancy_pg.invalid_baseline` when the application catalogue already defines
    /// `tenant_seed` or cannot be rebuilt with it.
    pub fn new(
        admin: PgConfig,
        placements: Vec<PlacementDb>,
        app_catalog: Catalog,
        runtime: Arc<tokio::runtime::Runtime>,
        faults: Arc<Faults>,
    ) -> AppResult<Self> {
        let catalog = merged_catalog(app_catalog)?;
        let placements = placements
            .into_iter()
            .map(|db| ((db.cluster.clone(), db.database.clone()), db))
            .collect();
        Ok(Self {
            admin,
            placements,
            catalog,
            runtime,
            faults,
        })
    }

    /// The baseline catalogue (the application's entities and the seed entity).
    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    fn block<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        block_on(self.runtime.as_ref(), future)
    }

    fn placement(&self, tenant: &Tenant) -> PortResult<&PlacementDb> {
        self.placements
            .get(&(
                tenant.placement.cluster.clone(),
                tenant.placement.database.clone(),
            ))
            .ok_or_else(|| step("the placement database is not configured", false))
    }

    /// Drops the connection once when the fault armed for `step` fires, then reports a retryable
    /// failure so the control plane journals the step as unfinished and a rerun resumes it.
    fn after(&self, tenant: &Tenant, step_name: Step) -> PortResult {
        if !self.faults.take_step(step_name.name()) {
            return Ok(());
        }
        if let Ok(db) = self.placement(tenant) {
            let pool = db.pool.clone();
            self.block(async move {
                if let Ok(client) = pool.get().await {
                    let _ = client
                        .batch_execute("SELECT pg_terminate_backend(pg_backend_pid())")
                        .await;
                }
            });
        }
        Err(step("the connection was dropped", true))
    }
}

impl SchemaPort for PgProvisioner {
    fn create(&self, tenant: &Tenant) -> PortResult {
        let db = self.placement(tenant)?;
        let schema = schema_ident(&tenant.placement.schema)?;
        let raw_schema = tenant.placement.schema.clone();
        let id = tenant.id;
        let expected = format!("spot tenant {id}");
        let admin = self.admin.clone();
        let placement = db.config.clone();
        let collation = self.catalog.collation.as_str().to_owned();
        let pool = db.pool.clone();
        let result = self.block(async move {
            ensure_database(&admin, &placement, &collation).await?;
            let mut client = pool
                .get()
                .await
                .map_err(|_| step("no placement connection is available", true))?;
            let tx = client
                .transaction()
                .await
                .map_err(|_| step("the tenant schema transaction could not begin", true))?;
            let exists: bool = tx
                .query_one("SELECT to_regnamespace($1) IS NOT NULL", &[&raw_schema])
                .await
                .map_err(|_| step("the tenant schema could not be read", true))?
                .get(0);
            if exists {
                let comment: Option<String> = tx
                    .query_one(
                        "SELECT obj_description(to_regnamespace($1), 'pg_namespace')",
                        &[&raw_schema],
                    )
                    .await
                    .map_err(|_| step("the tenant schema could not be read", true))?
                    .get(0);
                if comment.as_deref() != Some(expected.as_str()) {
                    let empty: bool = tx
                        .query_one(
                            "SELECT NOT EXISTS (SELECT 1 FROM pg_class \
                             WHERE relnamespace = to_regnamespace($1)::oid)",
                            &[&raw_schema],
                        )
                        .await
                        .map_err(|_| step("the tenant schema could not be read", true))?
                        .get(0);
                    if !empty {
                        return Err(step(
                            "a schema with this name already exists and is not empty",
                            false,
                        ));
                    }
                }
            } else {
                tx.batch_execute(&format!("CREATE SCHEMA {schema}"))
                    .await
                    .map_err(|_| step("the tenant schema could not be created", true))?;
            }
            tx.batch_execute(&format!("COMMENT ON SCHEMA {schema} IS 'spot tenant {id}'"))
                .await
                .map_err(|_| step("the tenant schema could not be claimed", true))?;
            tx.commit()
                .await
                .map_err(|_| step("the tenant schema transaction could not commit", true))
        });
        result?;
        self.after(tenant, Step::CreateSchema)
    }

    fn drop_schema(&self, tenant: &Tenant) -> PortResult {
        let db = self.placement(tenant)?;
        let schema = schema_ident(&tenant.placement.schema)?;
        let pool = db.pool.clone();
        self.block(async move {
            let client = pool
                .get()
                .await
                .map_err(|_| step("no placement connection is available", true))?;
            client
                .batch_execute(&format!("DROP SCHEMA IF EXISTS {schema} CASCADE"))
                .await
                .map_err(|_| step("the tenant schema could not be dropped", true))
        })
    }
}

impl SiteSeeder for PgProvisioner {
    fn seed(&self, tenant: &Tenant) -> PortResult {
        let db = self.placement(tenant)?;
        let schema = schema_ident(&tenant.placement.schema)?;
        let catalog = self.catalog.clone();
        let id = tenant.id.as_uuid();
        let label = tenant.name.to_string();
        let pool = db.pool.clone();
        self.block(async move {
            let client = pool
                .get()
                .await
                .map_err(|_| step("no placement connection is available", true))?;
            client
                .batch_execute(&format!("SET search_path TO {schema}"))
                .await
                .map_err(|_| step("the tenant schema could not be selected", true))?;
            let outcome = async {
                baseline::migrate(&client, &catalog)
                    .await
                    .map_err(|reason| step(reason, true))?;
                client
                    .execute(
                        &format!(
                            "INSERT INTO {SEED_TABLE} (id, version, label) VALUES ($1, 1, $2) \
                             ON CONFLICT (id) DO NOTHING"
                        ),
                        &[&id, &label],
                    )
                    .await
                    .map_err(|_| step("the tenant seed could not be written", true))?;
                Ok::<(), spot_tenancy::ports::StepFailure>(())
            }
            .await;
            let _ = client.batch_execute("RESET search_path").await;
            outcome
        })?;
        self.after(tenant, Step::Seed)
    }

    fn unseed(&self, tenant: &Tenant) -> PortResult {
        let db = self.placement(tenant)?;
        let schema = schema_ident(&tenant.placement.schema)?;
        let id = tenant.id.as_uuid();
        let pool = db.pool.clone();
        self.block(async move {
            let client = pool
                .get()
                .await
                .map_err(|_| step("no placement connection is available", true))?;
            if client
                .batch_execute(&format!("SET search_path TO {schema}"))
                .await
                .is_err()
            {
                // The schema is already gone: there is nothing left to unseed.
                return Ok(());
            }
            let outcome = match client
                .execute(&format!("DELETE FROM {SEED_TABLE} WHERE id = $1"), &[&id])
                .await
            {
                Ok(_) => Ok(()),
                Err(error) if is_undefined_table(&error) => Ok(()),
                Err(_) => Err(step("the tenant seed could not be removed", true)),
            };
            let _ = client.batch_execute("RESET search_path").await;
            outcome
        })
    }
}

impl PgProvisioner {
    /// Applies the baseline catalogue only: the split the adapters' schema port drives (the
    /// seed row is the seeder's step). `seed` runs the same migration before its row, so a
    /// second call plans nothing and both splits converge on one schema.
    fn migrate_baseline(&self, tenant: &Tenant) -> PortResult {
        let db = self.placement(tenant)?;
        let schema = schema_ident(&tenant.placement.schema)?;
        let catalog = self.catalog.clone();
        let pool = db.pool.clone();
        self.block(async move {
            let client = pool
                .get()
                .await
                .map_err(|_| step("no placement connection is available", true))?;
            client
                .batch_execute(&format!("SET search_path TO {schema}"))
                .await
                .map_err(|_| step("the tenant schema could not be selected", true))?;
            let outcome = baseline::migrate(&client, &catalog)
                .await
                .map_err(|reason| step(reason, true));
            let _ = client.batch_execute("RESET search_path").await;
            outcome
        })
    }
}

/// The real [`MigrationRunner`](spot_tenancy_adapters::MigrationRunner): the framework crate's
/// schema and seed adapters drive this executor, so the control plane runs the ports through
/// `spot-pg-migrate` without a SQL statement leaving the backend crate (ADR-45).
impl spot_tenancy_adapters::MigrationRunner for PgProvisioner {
    fn ensure_schema(&self, tenant: &Tenant) -> Result<(), spot_tenancy_adapters::AdapterError> {
        SchemaPort::create(self, tenant).map_err(|_| spot_tenancy_adapters::AdapterError::Schema)
    }

    fn drop_schema(&self, tenant: &Tenant) -> Result<(), spot_tenancy_adapters::AdapterError> {
        SchemaPort::drop_schema(self, tenant)
            .map_err(|_| spot_tenancy_adapters::AdapterError::Schema)
    }

    fn apply_baseline(&self, tenant: &Tenant) -> Result<(), spot_tenancy_adapters::AdapterError> {
        self.migrate_baseline(tenant)
            .map_err(|_| spot_tenancy_adapters::AdapterError::Seed)
    }

    fn seed(&self, tenant: &Tenant) -> Result<(), spot_tenancy_adapters::AdapterError> {
        SiteSeeder::seed(self, tenant).map_err(|_| spot_tenancy_adapters::AdapterError::Seed)
    }

    fn unseed(&self, tenant: &Tenant) -> Result<(), spot_tenancy_adapters::AdapterError> {
        SiteSeeder::unseed(self, tenant).map_err(|_| spot_tenancy_adapters::AdapterError::Seed)
    }
}

impl IamPort for PgProvisioner {
    fn create_users(&self, tenant: &Tenant) -> PortResult {
        self.after(tenant, Step::CreateUsers)
    }

    fn remove_users(&self, _tenant: &Tenant) -> PortResult {
        Ok(())
    }
}

impl LicencePort for PgProvisioner {
    fn issue(&self, tenant: &Tenant) -> PortResult {
        self.after(tenant, Step::IssueLicence)
    }

    fn revoke(&self, _tenant: &Tenant) -> PortResult {
        Ok(())
    }
}

impl TelemetryPort for PgProvisioner {
    fn register(&self, tenant: &Tenant) -> PortResult {
        self.after(tenant, Step::RegisterTelemetry)
    }

    fn deregister(&self, _tenant: &Tenant) -> PortResult {
        Ok(())
    }
}

async fn ensure_database(admin: &PgConfig, placement: &PgConfig, collation: &str) -> PortResult {
    match spot_postgres::database_exists(admin, &placement.dbname).await {
        Ok(true) => Ok(()),
        Ok(false) => spot_postgres::create_database(admin, &placement.dbname, collation)
            .await
            .map_err(|_| step("the placement database could not be created", true)),
        Err(_) => Err(step("the placement database could not be checked", true)),
    }
}

/// The built-in baseline entity every tenant gets.
fn seed_entity() -> AppResult<EntityDef> {
    let name = Name::new(SEED_TABLE).map_err(|_| invalid_baseline())?;
    let label = Name::new("label").map_err(|_| invalid_baseline())?;
    Ok(EntityDef {
        name,
        kind: EntityKind::Document,
        fields: vec![FieldDef {
            name: label,
            ty: FieldType::Text {
                max_chars: Some(140),
            },
            nullable: false,
        }],
        children: vec![],
        indexes: vec![],
        exclusions: vec![],
        search: None,
    })
}

/// Adds the seed entity to the application catalogue, keeping its version and collation.
fn merged_catalog(app: Catalog) -> AppResult<Catalog> {
    let mut entities: Vec<EntityDef> = app.entities().cloned().collect();
    entities.push(seed_entity()?);
    Catalog::new(app.version, app.collation.clone(), entities).map_err(|_| invalid_baseline())
}

/// Quotes and validates a tenant schema name (always from `TenantId::schema_name`).
fn schema_ident(schema: &str) -> PortResult<String> {
    if schema.is_empty()
        || schema.len() > 63
        || !schema
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(step(
            "the tenant schema name is not a safe identifier",
            false,
        ));
    }
    Ok(format!("\"{schema}\""))
}
