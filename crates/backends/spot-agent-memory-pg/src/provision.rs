//! Provisioning: a tenant's memory gets a database and a role of its own, on the control server,
//! recorded in the control store.
//!
//! This is the half of WP-AI-09 that makes the isolation structural. A tenant's memory is **not**
//! a schema in the tenant's business database, and not a table prefix: it is a *database*
//! ([`MemoryPlacement::database`]) owned by a *role* ([`MemoryPlacement::role`]) that is granted
//! nothing on the business database. PostgreSQL has no cross-database query — a connection sees
//! one database — so memory code holds no path to business tables from the memory connection.
//!
//! What this crate does **not** guarantee: it revokes `CONNECT` from `PUBLIC` on the *memory*
//! database only. Unless the control plane also revokes `PUBLIC CONNECT` on the business
//! databases, the memory role *can* connect to a business database (the isolation test asserts
//! so); what stops it reading there is the absence of any table privilege (`42501`), not the
//! absence of a connection.
//!
//! | Step | What it does |
//! | --- | --- |
//! | `database` | creates `spot_memory_db_<derived>` from `template0`, UTF-8, the control plane's ICU collation, when it is missing |
//! | `role` | creates `spot_memory_role_<derived>` — `LOGIN`, `NOSUPERUSER`, `NOCREATEDB`, `NOCREATEROLE`, `NOREPLICATION`, `NOBYPASSRLS` — when it is missing |
//! | `grants` | revokes `CONNECT` from `PUBLIC` on the memory database, grants it to the role and makes the role the database's owner; in the memory database it revokes the `public` schema from `PUBLIC` and grants `USAGE, CREATE` to the role |
//! | `extension` | creates `vector` **in the memory database only** |
//! | `registry` | records `(tenant, database, role, created_ms, updated_ms)` in `spot_memory_database` in the control store |
//!
//! The names are derived from the tenant id — never from a tenant-chosen name — with a hash of the
//! tenant id appended, so two tenants can never collide on one database: `a-b` and `a_b` would
//! otherwise both map to `a_b`, and a collision here would be two customers sharing a memory.
//!
//! # The password
//!
//! The role's password is **supplied by the caller** and never generated, stored, logged or
//! returned here: a placement carries the database and the role, and the caller combines them
//! with its own secret to build a [`PgConfig`]. The password is validated against a conservative
//! alphabet (it is set with a quoted literal, because PostgreSQL accepts no parameter for
//! `CREATE ROLE`) and then never touched again. Where the secret lives — a vault, a sealed
//! config — is the control plane's business, not this crate's.
//!
//! # Deprovisioning
//!
//! [`deprovision`](PgMemoryProvisioner::deprovision) drops the database (`WITH (FORCE)`, so live
//! sessions cannot keep a deleted tenant's memory alive), drops the role and removes the record.
//! Export first ([`PgMemoryStore::export`](crate::PgMemoryStore::export)): a dropped database is
//! gone.

use std::sync::Arc;

use deadpool_postgres::Client;
use spot_agent_memory::TenantId;
use spot_app_api::{AppError, AppResult};
use spot_postgres::PgConfig;

use crate::blocking::block_on;
use crate::error::provision;
use crate::schema;

/// The control-store table that records one memory database per tenant.
pub const REGISTRY_TABLE: &str = "spot_memory_database";

/// The longest PostgreSQL identifier, in bytes.
const MAX_IDENT: usize = 63;

/// Where one tenant's memory lives: the database and the role, and nothing secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryPlacement {
    /// The tenant whose memory it is.
    pub tenant: TenantId,
    /// The memory database's name, derived from the tenant id.
    pub database: String,
    /// The role that owns the memory database and is granted no privilege on any other database.
    pub role: String,
}

impl MemoryPlacement {
    /// A connection configuration for this placement, on the control server's host and port.
    ///
    /// The caller supplies the password it provisioned with; this crate neither stores nor
    /// returns it.
    #[must_use]
    pub fn config(&self, admin: &PgConfig, password: &str) -> PgConfig {
        let mut config = admin.clone();
        config.dbname.clone_from(&self.database);
        config.user.clone_from(&self.role);
        password.clone_into(&mut config.password);
        config.application_name = "spot-agent-memory".to_owned();
        config
    }
}

/// The provisioning executor: one control-server connection configuration and the runtime the
/// synchronous calls block on.
pub struct PgMemoryProvisioner {
    admin: PgConfig,
    collation: String,
    runtime: Arc<tokio::runtime::Runtime>,
}

impl std::fmt::Debug for PgMemoryProvisioner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgMemoryProvisioner")
            .field("host", &self.admin.host)
            .field("port", &self.admin.port)
            .field("collation", &self.collation)
            .finish_non_exhaustive()
    }
}

impl PgMemoryProvisioner {
    /// A provisioner over the control server `admin` connects to, creating memory databases with
    /// the ICU collation `collation` (the same one the tenant's own database is created with).
    #[must_use]
    pub fn new(admin: PgConfig, collation: &str, runtime: Arc<tokio::runtime::Runtime>) -> Self {
        Self {
            admin,
            collation: collation.to_owned(),
            runtime,
        }
    }

    /// Creates the control store's record table if it is missing.
    ///
    /// # Errors
    /// `agent_memory_pg.migration_failed` when the control store refuses.
    pub fn prepare_control(&self) -> AppResult<()> {
        let admin = self.admin.clone();
        self.block(async move {
            let client = admin_client(&admin).await?;
            client
                .batch_execute(&format!(
                    "CREATE TABLE IF NOT EXISTS {REGISTRY_TABLE} (\n\
                         tenant     text PRIMARY KEY,\n\
                         database   text NOT NULL,\n\
                         role       text NOT NULL,\n\
                         created_ms bigint NOT NULL,\n\
                         updated_ms bigint NOT NULL,\n\
                         CONSTRAINT {REGISTRY_TABLE}_created CHECK (created_ms >= 0),\n\
                         CONSTRAINT {REGISTRY_TABLE}_updated CHECK (updated_ms >= 0),\n\
                         CONSTRAINT {REGISTRY_TABLE}_database UNIQUE (database),\n\
                         CONSTRAINT {REGISTRY_TABLE}_role UNIQUE (role)\n\
                     )"
                ))
                .await
                .map_err(crate::error::migration)
        })
    }

    /// Provisions `tenant`'s memory database and role, and records the placement.
    ///
    /// Idempotent: an existing database, role or record is kept, and the grants are re-applied, so
    /// a resume after a crash converges instead of failing.
    ///
    /// # Errors
    /// `agent_memory_pg.provision_failed` naming the step that failed, and
    /// `agent_memory_pg.extension_missing` when the server has no `vector` extension.
    pub fn provision(
        &self,
        tenant: &TenantId,
        password: &str,
        now_ms: i64,
    ) -> AppResult<MemoryPlacement> {
        let placement = placement_for(tenant);
        let admin = self.admin.clone();
        let collation = self.collation.clone();
        let secret = password.to_owned();
        self.block(async move {
            validate_secret(&secret)?;
            let client = admin_client(&admin).await?;
            ensure_database(&admin, &placement, &collation).await?;
            ensure_role(&client, &placement, &secret).await?;
            grant_database(&client, &placement).await?;
            let memory = admin.clone().with_dbname(&placement.database);
            let memory_client = admin_client(&memory).await?;
            schema::ensure_extension(&memory_client).await?;
            grant_schema(&memory_client, &placement).await?;
            record(&client, &placement, now_ms).await?;
            Ok::<MemoryPlacement, AppError>(placement)
        })
    }

    /// Removes `tenant`'s memory database, its role and its record; `Ok(false)` when nothing was
    /// recorded.
    ///
    /// # Errors
    /// `agent_memory_pg.provision_failed` naming the step that failed.
    pub fn deprovision(&self, tenant: &TenantId) -> AppResult<bool> {
        let admin = self.admin.clone();
        self.block(async move {
            let client = admin_client(&admin).await?;
            let Some(placement) = read(&client, tenant).await? else {
                return Ok(false);
            };
            let database = identifier(&placement.database).map_err(|_| provision("database"))?;
            client
                .batch_execute(&format!("DROP DATABASE IF EXISTS {database} WITH (FORCE)"))
                .await
                .map_err(|_| provision("database"))?;
            let role = identifier(&placement.role).map_err(|_| provision("role"))?;
            client
                .batch_execute(&format!("DROP ROLE IF EXISTS {role}"))
                .await
                .map_err(|_| provision("role"))?;
            client
                .execute(
                    &format!("DELETE FROM {REGISTRY_TABLE} WHERE tenant = $1"),
                    &[&tenant.as_str()],
                )
                .await
                .map_err(|_| provision("registry"))?;
            Ok(true)
        })
    }

    /// The placement recorded for `tenant`, if one is.
    ///
    /// # Errors
    /// `agent_memory_pg.provision_failed` when the control store refuses.
    pub fn placement(&self, tenant: &TenantId) -> AppResult<Option<MemoryPlacement>> {
        let admin = self.admin.clone();
        self.block(async move {
            let client = admin_client(&admin).await?;
            read(&client, tenant).await
        })
    }

    fn block<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        block_on(self.runtime.as_ref(), future)
    }
}

/// The placement a tenant's memory gets: the derived database and role names.
#[must_use]
pub fn placement_for(tenant: &TenantId) -> MemoryPlacement {
    MemoryPlacement {
        tenant: tenant.clone(),
        database: derived_ident("db", tenant),
        role: derived_ident("role", tenant),
    }
}

/// A derived, collision-resistant, PostgreSQL-safe identifier for `tenant`.
///
/// The tenant id may hold `-`, which a database name cannot; the id is mapped to `_` and a hash of
/// the *original* id is appended, so `a-b` and `a_b` still get different names. The base is
/// clipped so the whole name fits PostgreSQL's 63-byte limit.
#[must_use]
pub fn derived_ident(kind: &str, tenant: &TenantId) -> String {
    let prefix = format!("spot_memory_{kind}_");
    let hash = format!("{:012x}", fnv1a(tenant.as_str()));
    let budget = MAX_IDENT.saturating_sub(prefix.len() + 1 + hash.len());
    let base: String = tenant
        .as_str()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(budget)
        .collect();
    format!("{prefix}{base}_{hash}")
}

/// FNV-1a over `text`: small, stable and dependency-free.
fn fnv1a(text: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Quotes and validates an identifier this crate derived or read from the control store: it must
/// be 1 to 63 lower-case letters, digits or underscores. Nothing else can be quoted.
fn identifier(name: &str) -> Result<String, ()> {
    if name.is_empty()
        || name.len() > MAX_IDENT
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(());
    }
    Ok(format!("\"{name}\""))
}

/// Checks a role password: 16 to 128 characters of an alphabet that needs no escaping, so the one
/// statement that must carry it as a literal (PostgreSQL takes no parameter for `CREATE ROLE`)
/// cannot be shaped by it. It is never stored or logged.
fn validate_secret(password: &str) -> AppResult<()> {
    let printable = password.len() >= 16
        && password.len() <= 128
        && password.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'.' | b'_'
                        | b'~'
                        | b'!'
                        | b'@'
                        | b'%'
                        | b'^'
                        | b'*'
                        | b'('
                        | b')'
                        | b'-'
                        | b'+'
                        | b'='
                )
        });
    if printable {
        Ok(())
    } else {
        Err(provision("role"))
    }
}

/// One connection on `config`, outside any pool: a provisioning step opens what it needs and
/// closes it again.
async fn admin_client(config: &PgConfig) -> AppResult<Client> {
    let pool = crate::pool_for(config).map_err(|_| provision("database"))?;
    pool.get().await.map_err(|_| provision("database"))
}

/// Creates the memory database when it is missing.
async fn ensure_database(
    admin: &PgConfig,
    placement: &MemoryPlacement,
    collation: &str,
) -> AppResult<()> {
    match spot_postgres::database_exists(admin, &placement.database).await {
        Ok(true) => Ok(()),
        Ok(false) => spot_postgres::create_database(admin, &placement.database, collation)
            .await
            .map_err(|_| provision("database")),
        Err(_) => Err(provision("database")),
    }
}

/// Creates the memory role when it is missing, with the password the caller supplied.
///
/// PostgreSQL accepts no bind parameter for `CREATE ROLE`/`ALTER ROLE`, so the password reaches
/// the statement as the server's own `%L`-quoted literal: the value is bound as a parameter, the
/// *server* renders the literal, and no secret is ever concatenated into a string this crate
/// builds. The role's attributes are re-applied on every run, so a role that survived a crash
/// ends up with exactly the grants and secret the caller holds now.
async fn ensure_role(
    client: &Client,
    placement: &MemoryPlacement,
    password: &str,
) -> AppResult<()> {
    let role = identifier(&placement.role).map_err(|_| provision("role"))?;
    let exists: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)",
            &[&placement.role],
        )
        .await
        .map_err(|_| provision("role"))?
        .try_get(0)
        .map_err(|_| provision("role"))?;
    if !exists {
        client
            .batch_execute(&format!(
                "CREATE ROLE {role} WITH LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION \
                 NOBYPASSRLS"
            ))
            .await
            .map_err(|_| provision("role"))?;
    }
    let statement: String = client
        .query_one(
            "SELECT format('ALTER ROLE %I WITH LOGIN PASSWORD %L', $1::text, $2::text)",
            &[&placement.role, &password],
        )
        .await
        .map_err(|_| provision("role"))?
        .try_get(0)
        .map_err(|_| provision("role"))?;
    client
        .batch_execute(&statement)
        .await
        .map_err(|_| provision("role"))
}

/// Makes the role the database's owner and closes it to everyone else.
async fn grant_database(client: &Client, placement: &MemoryPlacement) -> AppResult<()> {
    let database = identifier(&placement.database).map_err(|_| provision("grants"))?;
    let role = identifier(&placement.role).map_err(|_| provision("grants"))?;
    client
        .batch_execute(&format!(
            "REVOKE ALL ON DATABASE {database} FROM PUBLIC;\n\
             GRANT CONNECT ON DATABASE {database} TO {role};\n\
             ALTER DATABASE {database} OWNER TO {role}"
        ))
        .await
        .map_err(|_| provision("grants"))
}

/// Gives the role the `public` schema of its own memory database, and nobody else.
async fn grant_schema(client: &Client, placement: &MemoryPlacement) -> AppResult<()> {
    let role = identifier(&placement.role).map_err(|_| provision("grants"))?;
    client
        .batch_execute(&format!(
            "REVOKE ALL ON SCHEMA public FROM PUBLIC;\n\
             GRANT USAGE, CREATE ON SCHEMA public TO {role}"
        ))
        .await
        .map_err(|_| provision("grants"))
}

/// Records the placement, keeping the original creation instant on a re-run.
async fn record(client: &Client, placement: &MemoryPlacement, now_ms: i64) -> AppResult<()> {
    client
        .execute(
            &format!(
                "INSERT INTO {REGISTRY_TABLE} (tenant, database, role, created_ms, updated_ms) \
                 VALUES ($1, $2, $3, $4, $4) \
                 ON CONFLICT (tenant) DO UPDATE SET database = EXCLUDED.database, \
                 role = EXCLUDED.role, updated_ms = EXCLUDED.updated_ms"
            ),
            &[
                &placement.tenant.as_str(),
                &placement.database,
                &placement.role,
                &now_ms,
            ],
        )
        .await
        .map_err(|_| provision("registry"))?;
    Ok(())
}

/// The placement recorded for `tenant`, if one is.
async fn read(client: &Client, tenant: &TenantId) -> AppResult<Option<MemoryPlacement>> {
    let rows = client
        .query(
            &format!("SELECT database, role FROM {REGISTRY_TABLE} WHERE tenant = $1"),
            &[&tenant.as_str()],
        )
        .await
        .map_err(|_| provision("registry"))?;
    match rows.first() {
        Some(row) => Ok(Some(MemoryPlacement {
            tenant: tenant.clone(),
            database: row.try_get(0).map_err(|_| provision("registry"))?,
            role: row.try_get(1).map_err(|_| provision("registry"))?,
        })),
        None => Ok(None),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn tenant(name: &str) -> TenantId {
        TenantId::parse(name).unwrap_or_else(|error| panic!("{error}"))
    }

    #[test]
    fn a_derived_name_is_a_safe_identifier_that_fits_the_column() {
        for name in ["tenant_a", "tenant-a", &"a".repeat(64)] {
            let id = tenant(name);
            for kind in ["db", "role"] {
                let ident = derived_ident(kind, &id);
                assert!(ident.len() <= MAX_IDENT, "{ident} fits PostgreSQL");
                assert!(identifier(&ident).is_ok(), "{ident} is quotable");
                assert!(ident.starts_with(&format!("spot_memory_{kind}_")));
            }
        }
    }

    #[test]
    fn two_tenants_that_differ_only_in_a_separator_get_different_names() {
        assert_ne!(
            derived_ident("db", &tenant("tenant-a")),
            derived_ident("db", &tenant("tenant_a")),
            "a collision here would be two customers sharing one memory"
        );
        assert_eq!(
            derived_ident("db", &tenant("tenant_a")),
            derived_ident("db", &tenant("tenant_a")),
            "the derivation is deterministic"
        );
    }

    #[test]
    fn a_placement_config_names_the_memory_database_and_role() {
        let admin = PgConfig::new("db.example", 5432, "postgres", "spot", "admin-secret");
        let placement = placement_for(&tenant("tenant_a"));
        let config = placement.config(&admin, "memory-secret-value");
        assert_eq!(config.dbname, placement.database);
        assert_eq!(config.user, placement.role);
        assert_eq!(config.host, "db.example");
        assert!(!format!("{placement:?}").contains("memory-secret-value"));
    }

    #[test]
    fn a_password_outside_the_safe_alphabet_is_refused() {
        assert!(validate_secret("a-safe-password-0000").is_ok());
        assert!(validate_secret("short").is_err());
        assert!(validate_secret("has'quote-in-it-0000").is_err());
        assert!(validate_secret("has backslash \\ in it 0000").is_err());
        assert!(validate_secret(&"x".repeat(129)).is_err());
    }
}
