//! The PostgreSQL [`ControlStore`]: the tenant registry, its placement and its workflow journal.
//!
//! The store mirrors the reference `MemoryStore` exactly, but its invariants are also enforced
//! by the database so two control nodes cannot both win:
//!
//! * `insert_within` names no live tenant twice and no live placement twice (partial unique indexes);
//!   a deleted tenant's unexpired name reservation blocks a new tenant with a typed error; and
//!   the placement's live-tenant count and the insert are one transaction holding an advisory
//!   lock on the database, so two control nodes cannot both take the last slot of a bound;
//! * `update` takes the row `FOR UPDATE`, checks the optimistic version, the immutable fields,
//!   the transition table and the "placement changes only while moving" rule, then writes the
//!   row and appends one history row when the state changed;
//! * `journal_append` and `journal` are per tenant and ordered by the database's sequence.
//!
//! Every method opens its own connection and commits its own transaction; nothing joins a
//! caller's transaction. Failures carry a fixed `&'static str` reason, never a raw SQL string.

use std::collections::BTreeMap;
use std::sync::Arc;

use deadpool_postgres::{Object, Pool, PoolError};
use spot_tenancy::{
    ClusterId, ControlStore, DatabaseId, Edition, Error, JournalEntry, JournalKind, Region, Result,
    Retention, Step, Tenant, TenantFlags, TenantId, TenantName, TenantState,
};
use tokio_postgres::Row;

use crate::blocking::block_on;
use crate::error::{store_failed, unique_violation};
use crate::fault::Faults;
use crate::schema;

/// The control store over a control database.
pub struct PgControlStore {
    pool: Pool,
    runtime: Arc<tokio::runtime::Runtime>,
    faults: Arc<Faults>,
}

impl std::fmt::Debug for PgControlStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgControlStore").finish_non_exhaustive()
    }
}

impl PgControlStore {
    /// A store on `pool`. It does not touch the database until [`prepare`](Self::prepare) or a
    /// trait method is called.
    ///
    /// `runtime` is what the synchronous trait methods block on; `faults` is the kill seam used
    /// by the resume tests (an unarmed [`Faults`] is inert).
    #[must_use]
    pub fn new(pool: Pool, runtime: Arc<tokio::runtime::Runtime>, faults: Arc<Faults>) -> Self {
        Self {
            pool,
            runtime,
            faults,
        }
    }

    /// Creates the control schema and its tables if they are missing. Idempotent and safe from
    /// several control nodes at once.
    ///
    /// # Errors
    /// `tenancy_pg.migration_failed` when the database cannot apply the DDL.
    pub async fn prepare(&self) -> spot_app_api::AppResult<()> {
        schema::prepare(&self.pool).await
    }

    async fn conn(&self) -> Result<Object> {
        self.pool
            .get()
            .await
            .map_err(|_: PoolError| store_failed("no control store connection is available"))
    }

    async fn insert_inner(&self, mut tenant: Tenant, now: u64, capacity: u32) -> Result<Tenant> {
        tenant.placement.validate_for(&tenant.id)?;
        if tenant.state != TenantState::Provisioning {
            return Err(Error::WrongState {
                tenant: tenant.id.to_string(),
                state: tenant.state,
                operation: "register a tenant",
            });
        }
        tenant.version = 1;
        tenant.retention = None;
        tenant.name_reserved_until = None;

        let mut client = self.conn().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|_| store_failed("the control store transaction could not begin"))?;

        let name = tenant.name.as_str().to_owned();
        // A deleted tenant's reservation blocks the name. `FOR UPDATE` makes a concurrent delete
        // and this insert serialise so the reservation is never missed.
        let reserved = tx
            .query_opt(
                "SELECT name_reserved_until FROM spot_control.tenant \
                 WHERE name = $1 AND state = 'deleted' AND name_reserved_until IS NOT NULL \
                 ORDER BY name_reserved_until DESC LIMIT 1 FOR UPDATE",
                &[&name],
            )
            .await
            .map_err(|_| store_failed("the control store query failed"))?;
        if let Some(row) = reserved
            && let Some(until) = row.get::<_, Option<i64>>(0)
        {
            let until = u64::try_from(until).unwrap_or(u64::MAX);
            if until > now {
                return Err(Error::NameReserved { name, until });
            }
        }

        let live = tx
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM spot_control.tenant \
                 WHERE name = $1 AND state <> 'deleted')",
                &[&name],
            )
            .await
            .map_err(|_| store_failed("the control store query failed"))?
            .get::<_, bool>(0);
        if live {
            return Err(Error::NameTaken { name });
        }

        let cluster = tenant.placement.cluster.as_str().to_owned();
        let database = tenant.placement.database.as_str().to_owned();
        let schema = tenant.placement.schema.clone();
        let occupied = tx
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM spot_control.tenant \
                 WHERE cluster = $1 AND database = $2 AND schema = $3 AND state <> 'deleted')",
                &[&cluster, &database, &schema],
            )
            .await
            .map_err(|_| store_failed("the control store query failed"))?
            .get::<_, bool>(0);
        if occupied {
            return Err(Error::InvalidPlacement {
                reason: "another tenant already holds this schema",
            });
        }

        // The capacity check and the insert are one step: the transaction-scoped advisory lock
        // keyed on the placement serialises every insert into this database, so two control
        // nodes cannot each count the same free slot and both take it.
        let lock_key = format!("{cluster}/{database}");
        tx.execute(
            "SELECT pg_advisory_xact_lock(hashtextextended($1, 0))",
            &[&lock_key],
        )
        .await
        .map_err(|_| store_failed("the control store lock failed"))?;
        let used = tx
            .query_one(
                "SELECT count(*) FROM spot_control.tenant \
                 WHERE cluster = $1 AND database = $2 AND state <> 'deleted'",
                &[&cluster, &database],
            )
            .await
            .map_err(|_| store_failed("the control store query failed"))?
            .get::<_, i64>(0);
        let used = u64::try_from(used).unwrap_or(u64::MAX);
        if used >= u64::from(capacity) {
            return Err(Error::CapacityExceeded {
                cluster: cluster.clone(),
                database: database.clone(),
            });
        }

        let inserted = tx
            .execute(
                "INSERT INTO spot_control.tenant \
                 (id, name, region, edition, state, cluster, database, schema, created, flags, \
                  version, backup_ref, backup_verified, delete_after, name_reserved_until) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 1, NULL, NULL, NULL, NULL)",
                &[
                    &tenant.id.as_uuid(),
                    &name,
                    &tenant.region.as_str(),
                    &tenant.edition.as_str(),
                    &state_name(tenant.state),
                    &cluster,
                    &database,
                    &schema,
                    &i64::try_from(tenant.created).unwrap_or(i64::MAX),
                    &i32::try_from(flags_bits(tenant.flags)).unwrap_or(i32::MAX),
                ],
            )
            .await;
        if let Err(error) = inserted {
            return Err(insert_error(&error, &name));
        }
        tx.commit()
            .await
            .map_err(|_| store_failed("the control store transaction could not commit"))?;
        Ok(tenant)
    }

    async fn get_inner(&self, id: &TenantId) -> Result<Tenant> {
        let client = self.conn().await?;
        let row = client
            .query_opt(SELECT_TENANT, &[&id.as_uuid()])
            .await
            .map_err(|_| store_failed("the control store query failed"))?
            .ok_or_else(|| not_found(id))?;
        tenant_from_row(&row)
    }

    async fn find_by_name_inner(&self, name: &TenantName) -> Result<Option<Tenant>> {
        let client = self.conn().await?;
        let row = client
            .query_opt(
                &format!("{SELECT_TENANT_PREFIX} WHERE name = $1 AND state <> 'deleted'"),
                &[&name.as_str()],
            )
            .await
            .map_err(|_| store_failed("the control store query failed"))?;
        row.as_ref().map(tenant_from_row).transpose()
    }

    async fn update_inner(&self, tenant: Tenant, expected_version: u64) -> Result<Tenant> {
        let mut client = self.conn().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|_| store_failed("the control store transaction could not begin"))?;
        let row = tx
            .query_opt(
                &format!("{SELECT_TENANT_PREFIX} WHERE id = $1 FOR UPDATE"),
                &[&tenant.id.as_uuid()],
            )
            .await
            .map_err(|_| store_failed("the control store query failed"))?
            .ok_or_else(|| not_found(&tenant.id))?;
        let stored = tenant_from_row(&row)?;

        if stored.version != expected_version {
            return Err(Error::VersionConflict {
                tenant: tenant.id.to_string(),
                expected: expected_version,
                found: stored.version,
            });
        }
        if tenant.name != stored.name
            || tenant.region != stored.region
            || tenant.created != stored.created
        {
            return Err(Error::InvalidRequest {
                reason: "a tenant's name, region and creation time never change",
            });
        }
        if stored.state != tenant.state && !stored.state.can_become(tenant.state) {
            return Err(Error::InvalidTransition {
                tenant: tenant.id.to_string(),
                from: stored.state,
                to: tenant.state,
            });
        }
        if stored.state == TenantState::Deleted {
            return Err(Error::WrongState {
                tenant: tenant.id.to_string(),
                state: TenantState::Deleted,
                operation: "update a deleted tenant",
            });
        }
        if tenant.placement != stored.placement {
            if stored.state != TenantState::Moving {
                return Err(Error::InvalidPlacement {
                    reason: "the placement changes only while the tenant is moving",
                });
            }
            tenant.placement.validate_for(&tenant.id)?;
            let cluster = tenant.placement.cluster.as_str();
            let database = tenant.placement.database.as_str();
            let schema = tenant.placement.schema.as_str();
            let occupied = tx
                .query_one(
                    "SELECT EXISTS (SELECT 1 FROM spot_control.tenant \
                     WHERE id <> $1 AND state <> 'deleted' \
                       AND cluster = $2 AND database = $3 AND schema = $4)",
                    &[&tenant.id.as_uuid(), &cluster, &database, &schema],
                )
                .await
                .map_err(|_| store_failed("the control store query failed"))?
                .get::<_, bool>(0);
            if occupied {
                return Err(Error::InvalidPlacement {
                    reason: "another tenant already holds this schema",
                });
            }
        }

        let next_version = expected_version
            .checked_add(1)
            .ok_or_else(|| store_failed("the tenant version overflowed"))?;
        let updated = tx
            .execute(
                "UPDATE spot_control.tenant SET \
                 name = $2, region = $3, edition = $4, state = $5, cluster = $6, \
                 database = $7, schema = $8, flags = $9, version = $10, backup_ref = $11, \
                 backup_verified = $12, delete_after = $13, name_reserved_until = $14 \
                 WHERE id = $1",
                &[
                    &tenant.id.as_uuid(),
                    &tenant.name.as_str(),
                    &tenant.region.as_str(),
                    &tenant.edition.as_str(),
                    &state_name(tenant.state),
                    &tenant.placement.cluster.as_str(),
                    &tenant.placement.database.as_str(),
                    &tenant.placement.schema,
                    &i32::try_from(flags_bits(tenant.flags)).unwrap_or(i32::MAX),
                    &i64::try_from(next_version).unwrap_or(i64::MAX),
                    &tenant
                        .retention
                        .as_ref()
                        .map(|retention| retention.backup_ref.as_str()),
                    &tenant
                        .retention
                        .as_ref()
                        .map(|retention| retention.verified),
                    &tenant
                        .retention
                        .as_ref()
                        .and_then(|retention| i64::try_from(retention.delete_after).ok()),
                    &tenant
                        .name_reserved_until
                        .and_then(|until| i64::try_from(until).ok()),
                ],
            )
            .await;
        if let Err(error) = updated {
            if let Some(constraint) = unique_violation(&error) {
                if constraint == "tenant_live_placement" {
                    return Err(Error::InvalidPlacement {
                        reason: "another tenant already holds this schema",
                    });
                }
                if constraint == "tenant_live_name" {
                    return Err(Error::NameTaken {
                        name: tenant.name.to_string(),
                    });
                }
            }
            return Err(store_failed("the control store query failed"));
        }

        if stored.state != tenant.state {
            tx.execute(
                "INSERT INTO spot_control.tenant_history \
                 (tenant, version, from_state, to_state) VALUES ($1, $2, $3, $4)",
                &[
                    &tenant.id.as_uuid(),
                    &i64::try_from(next_version).unwrap_or(i64::MAX),
                    &state_name(stored.state),
                    &state_name(tenant.state),
                ],
            )
            .await
            .map_err(|_| store_failed("the control store history could not be written"))?;
        }

        // The kill-and-resume test drops this connection after the effect but before the commit:
        // the update rolls back and the caller resumes from the journal.
        if self.faults.take_update() {
            let _ = tx
                .execute("SELECT pg_terminate_backend(pg_backend_pid())", &[])
                .await;
            return Err(store_failed("the control store connection was dropped"));
        }

        tx.commit()
            .await
            .map_err(|_| store_failed("the control store transaction could not commit"))?;

        let mut stored = tenant;
        stored.version = next_version;
        Ok(stored)
    }

    async fn list_inner(&self) -> Result<Vec<Tenant>> {
        let client = self.conn().await?;
        let rows = client
            .query(&format!("{SELECT_TENANT_PREFIX} ORDER BY id"), &[])
            .await
            .map_err(|_| store_failed("the control store query failed"))?;
        rows.iter().map(tenant_from_row).collect()
    }

    async fn load_inner(&self) -> Result<BTreeMap<(ClusterId, DatabaseId), u32>> {
        let client = self.conn().await?;
        let rows = client
            .query(
                "SELECT cluster, database, count(*) FROM spot_control.tenant \
                 WHERE state <> 'deleted' GROUP BY cluster, database",
                &[],
            )
            .await
            .map_err(|_| store_failed("the control store query failed"))?;
        let mut out = BTreeMap::new();
        for row in &rows {
            let cluster = ClusterId::new(row.get::<_, String>(0))
                .map_err(|_| corrupt("a stored cluster id is not valid"))?;
            let database = DatabaseId::new(row.get::<_, String>(1))
                .map_err(|_| corrupt("a stored database id is not valid"))?;
            let count = u32::try_from(row.get::<_, i64>(2))
                .map_err(|_| corrupt("a stored tenant count is not valid"))?;
            out.insert((cluster, database), count);
        }
        Ok(out)
    }

    async fn journal_append_inner(&self, id: &TenantId, entry: JournalEntry) -> Result<()> {
        let mut client = self.conn().await?;
        let tx = client
            .transaction()
            .await
            .map_err(|_| store_failed("the control store transaction could not begin"))?;
        let exists = tx
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM spot_control.tenant WHERE id = $1)",
                &[&id.as_uuid()],
            )
            .await
            .map_err(|_| store_failed("the control store query failed"))?
            .get::<_, bool>(0);
        if !exists {
            return Err(not_found(id));
        }
        tx.execute(
            "INSERT INTO spot_control.tenant_journal (tenant, step, kind, at) \
             VALUES ($1, $2, $3, $4)",
            &[
                &id.as_uuid(),
                &entry.step.name(),
                &kind_name(entry.kind),
                &i64::try_from(entry.at).unwrap_or(i64::MAX),
            ],
        )
        .await
        .map_err(|_| store_failed("the control store journal could not be written"))?;
        tx.commit()
            .await
            .map_err(|_| store_failed("the control store transaction could not commit"))
    }

    async fn journal_inner(&self, id: &TenantId) -> Result<Vec<JournalEntry>> {
        let client = self.conn().await?;
        let exists = client
            .query_one(
                "SELECT EXISTS (SELECT 1 FROM spot_control.tenant WHERE id = $1)",
                &[&id.as_uuid()],
            )
            .await
            .map_err(|_| store_failed("the control store query failed"))?
            .get::<_, bool>(0);
        if !exists {
            return Err(not_found(id));
        }
        let rows = client
            .query(
                "SELECT step, kind, at FROM spot_control.tenant_journal \
                 WHERE tenant = $1 ORDER BY seq",
                &[&id.as_uuid()],
            )
            .await
            .map_err(|_| store_failed("the control store query failed"))?;
        rows.iter()
            .map(|row| {
                let step = parse_step(row.get::<_, String>(0).as_str())
                    .ok_or_else(|| corrupt("a stored journal step is not valid"))?;
                let kind = parse_kind(row.get::<_, String>(1).as_str())
                    .ok_or_else(|| corrupt("a stored journal kind is not valid"))?;
                let at = u64::try_from(row.get::<_, i64>(2))
                    .map_err(|_| corrupt("a stored journal instant is not valid"))?;
                Ok(JournalEntry { step, kind, at })
            })
            .collect()
    }
}

impl ControlStore for PgControlStore {
    fn insert_within(&self, tenant: Tenant, now: u64, capacity: u32) -> Result<Tenant> {
        block_on(
            self.runtime.as_ref(),
            self.insert_inner(tenant, now, capacity),
        )
    }

    fn get(&self, id: &TenantId) -> Result<Tenant> {
        block_on(self.runtime.as_ref(), self.get_inner(id))
    }

    fn find_by_name(&self, name: &TenantName) -> Result<Option<Tenant>> {
        block_on(self.runtime.as_ref(), self.find_by_name_inner(name))
    }

    fn update(&self, tenant: Tenant, expected_version: u64) -> Result<Tenant> {
        block_on(
            self.runtime.as_ref(),
            self.update_inner(tenant, expected_version),
        )
    }

    fn list(&self) -> Result<Vec<Tenant>> {
        block_on(self.runtime.as_ref(), self.list_inner())
    }

    fn load(&self) -> Result<BTreeMap<(ClusterId, DatabaseId), u32>> {
        block_on(self.runtime.as_ref(), self.load_inner())
    }

    fn journal_append(&self, id: &TenantId, entry: JournalEntry) -> Result<()> {
        block_on(self.runtime.as_ref(), self.journal_append_inner(id, entry))
    }

    fn journal(&self, id: &TenantId) -> Result<Vec<JournalEntry>> {
        block_on(self.runtime.as_ref(), self.journal_inner(id))
    }
}

const SELECT_TENANT_PREFIX: &str = "SELECT id, name, region, edition, state, cluster, database, \
     schema, created, flags, version, backup_ref, backup_verified, delete_after, \
     name_reserved_until FROM spot_control.tenant";

const SELECT_TENANT: &str = "SELECT id, name, region, edition, state, cluster, database, \
     schema, created, flags, version, backup_ref, backup_verified, delete_after, \
     name_reserved_until FROM spot_control.tenant WHERE id = $1";

fn not_found(id: &TenantId) -> Error {
    Error::NotFound {
        tenant: id.to_string(),
    }
}

fn corrupt(reason: &'static str) -> Error {
    store_failed(reason)
}

fn pg() -> Error {
    store_failed("the control store query failed")
}

fn insert_error(error: &tokio_postgres::Error, name: &str) -> Error {
    match unique_violation(error) {
        Some("tenant_pkey") => Error::InvalidRequest {
            reason: "the tenant id is already registered",
        },
        Some("tenant_live_name") => Error::NameTaken {
            name: name.to_owned(),
        },
        Some("tenant_live_placement") => Error::InvalidPlacement {
            reason: "another tenant already holds this schema",
        },
        _ => pg(),
    }
}

fn tenant_from_row(row: &Row) -> Result<Tenant> {
    let id = TenantId::from_uuid(row.get("id"))
        .map_err(|_| corrupt("a stored tenant id is not a UUIDv7"))?;
    let name = TenantName::new(row.get::<_, String>("name"))
        .map_err(|_| corrupt("a stored tenant name is not valid"))?;
    let region = Region::new(row.get::<_, String>("region"))
        .map_err(|_| corrupt("a stored region is not valid"))?;
    let edition = Edition::new(row.get::<_, String>("edition"))
        .map_err(|_| corrupt("a stored edition is not valid"))?;
    let state = parse_state(row.get::<_, String>("state").as_str())
        .ok_or_else(|| corrupt("a stored tenant state is not valid"))?;
    let cluster = ClusterId::new(row.get::<_, String>("cluster"))
        .map_err(|_| corrupt("a stored cluster id is not valid"))?;
    let database = DatabaseId::new(row.get::<_, String>("database"))
        .map_err(|_| corrupt("a stored database id is not valid"))?;
    let schema: String = row.get("schema");
    let created = u64::try_from(row.get::<_, i64>("created"))
        .map_err(|_| corrupt("a stored creation instant is not valid"))?;
    let flags = u32::try_from(row.get::<_, i32>("flags"))
        .map_err(|_| corrupt("stored tenant flags are not valid"))?;
    let flags =
        TenantFlags::from_bits(flags).map_err(|_| corrupt("stored tenant flags are not valid"))?;
    let version = u64::try_from(row.get::<_, i64>("version"))
        .map_err(|_| corrupt("a stored tenant version is not valid"))?;
    let retention = match row.get::<_, Option<String>>("backup_ref") {
        None => None,
        Some(backup_ref) => {
            let delete_after = row
                .get::<_, Option<i64>>("delete_after")
                .ok_or_else(|| corrupt("a stored retention has no deletion instant"))?;
            Some(Retention {
                backup_ref,
                verified: row
                    .get::<_, Option<bool>>("backup_verified")
                    .unwrap_or(false),
                delete_after: u64::try_from(delete_after)
                    .map_err(|_| corrupt("a stored deletion instant is not valid"))?,
            })
        }
    };
    let name_reserved_until = row
        .get::<_, Option<i64>>("name_reserved_until")
        .map(|until| u64::try_from(until).unwrap_or(u64::MAX));
    Ok(Tenant {
        id,
        name,
        region,
        edition,
        state,
        placement: spot_tenancy::Placement {
            cluster,
            database,
            schema,
        },
        created,
        flags,
        version,
        retention,
        name_reserved_until,
    })
}

fn state_name(state: TenantState) -> &'static str {
    match state {
        TenantState::Provisioning => "provisioning",
        TenantState::Active => "active",
        TenantState::Suspended => "suspended",
        TenantState::Moving => "moving",
        TenantState::Deprovisioning => "deprovisioning",
        TenantState::Deleted => "deleted",
    }
}

fn parse_state(state: &str) -> Option<TenantState> {
    match state {
        "provisioning" => Some(TenantState::Provisioning),
        "active" => Some(TenantState::Active),
        "suspended" => Some(TenantState::Suspended),
        "moving" => Some(TenantState::Moving),
        "deprovisioning" => Some(TenantState::Deprovisioning),
        "deleted" => Some(TenantState::Deleted),
        _ => None,
    }
}

fn flags_bits(flags: TenantFlags) -> u32 {
    let mut bits = 0;
    if flags.contains(TenantFlags::LEGAL_HOLD) {
        bits |= 1;
    }
    if flags.contains(TenantFlags::READ_ONLY) {
        bits |= 2;
    }
    bits
}

fn kind_name(kind: JournalKind) -> &'static str {
    match kind {
        JournalKind::Started => "started",
        JournalKind::Done => "done",
        JournalKind::Compensated => "compensated",
    }
}

fn parse_kind(kind: &str) -> Option<JournalKind> {
    match kind {
        "started" => Some(JournalKind::Started),
        "done" => Some(JournalKind::Done),
        "compensated" => Some(JournalKind::Compensated),
        _ => None,
    }
}

fn parse_step(step: &str) -> Option<Step> {
    Step::ALL
        .into_iter()
        .find(|candidate| candidate.name() == step)
}
