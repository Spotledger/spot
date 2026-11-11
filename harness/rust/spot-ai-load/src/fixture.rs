//! The scratch database: schema, seeded table, the `ai` role and its grants, and the clean-up.
//!
//! One database per run (`spot_postgres::testing::TestDb`). The `ai` role is created with the
//! WP-AI-05.T1 helpers (`role_creation_sql`, `grants_for_schema`) and then given a login and a
//! test-only password, because the T1 role is `NOLOGIN` and a deployment connects with a
//! per-tenant secret. Roles are cluster-wide, so the role's name is unique per run and the
//! clean-up drops it also when the run failed: [`Fixture::teardown`] on the normal path and the
//! `Drop` implementation as the net under it.

use spot_ai_isolation::{AiPoolSettings, grants_for_schema, role_creation_sql};
use spot_postgres::PgConfig;
use spot_postgres::testing::TestDb;
use tokio_postgres::{Client, NoTls};

use crate::error::{HarnessError, Result};

/// The seeded table's name (the `spot_` prefix is the platform's rule for every table).
pub const TABLE: &str = "spot_load_docs";

/// A provisioned scratch database.
pub struct Fixture {
    db: TestDb,
    role: String,
    password: String,
    schema: String,
    ai_write_allowed: bool,
    torn_down: bool,
}

impl Fixture {
    /// Creates the database, the schema, `rows` seeded rows and the `ai` role.
    ///
    /// Returns `None` (after the helper printed `SKIPPED: set SPOT_TEST_PG_URL`) when no server
    /// is configured. On an error the fixture is dropped, which cleans up.
    ///
    /// # Errors
    ///
    /// [`HarnessError::Database`] when a provisioning statement fails.
    pub async fn create(
        rows: u64,
        pool: &AiPoolSettings,
        role_settings: &[(String, String)],
    ) -> Result<Option<Self>> {
        let Some(db) = TestDb::create().await else {
            return Ok(None);
        };
        let suffix: String = {
            let name = db.name();
            name[name.len().saturating_sub(16)..].to_owned()
        };
        let mut fixture = Self {
            db,
            role: format!("spot_ail_{suffix}"),
            password: format!("pw_{suffix}"),
            schema: format!("spot_ail_{suffix}"),
            ai_write_allowed: false,
            torn_down: false,
        };
        fixture.provision(rows, pool, role_settings).await?;
        Ok(Some(fixture))
    }

    async fn provision(
        &mut self,
        rows: u64,
        pool: &AiPoolSettings,
        role_settings: &[(String, String)],
    ) -> Result<()> {
        let admin = self.db.raw_client().await;
        let table = self.table_path();
        let rows = i64::try_from(rows).unwrap_or(i64::MAX);
        let schema = &self.schema;
        run(
            &admin,
            "create the schema",
            &format!("CREATE SCHEMA {schema}"),
        )
        .await?;
        run(
            &admin,
            "create the table",
            &format!(
                "CREATE TABLE {table} (id bigint PRIMARY KEY, tenant text NOT NULL, \
                 status text NOT NULL, amount_cents bigint NOT NULL, created_at bigint NOT NULL, \
                 payload text NOT NULL)"
            ),
        )
        .await?;
        admin
            .execute(
                &format!(
                    "INSERT INTO {table} SELECT g, 'tenant-' || (g % 4), \
                     (ARRAY['draft','open','paid','void','late'])[g % 5 + 1], \
                     (g * 37) % 100000, g, md5(g::text) FROM generate_series(1, $1::bigint) g"
                ),
                &[&rows],
            )
            .await
            .map_err(|e| HarnessError::db("seed the table", &e))?;
        run(
            &admin,
            "index the table",
            &format!("CREATE INDEX ON {table} (status, created_at)"),
        )
        .await?;
        run(&admin, "analyse the table", &format!("ANALYZE {table}")).await?;

        let role = &self.role;
        for statement in role_creation_sql(role, pool)? {
            run(&admin, "create the ai role", &statement).await?;
        }
        for (name, value) in role_settings {
            run(
                &admin,
                "apply a role setting",
                &format!("ALTER ROLE {role} SET {name} = {value}"),
            )
            .await?;
        }
        run(
            &admin,
            "give the ai role a login",
            &format!("ALTER ROLE {role} LOGIN PASSWORD '{}'", self.password),
        )
        .await?;
        for statement in grants_for_schema(role, self.db.name(), schema)? {
            run(&admin, "grant to the ai role", &statement).await?;
        }
        self.ai_write_allowed = self.probe_ai_write(&table).await?;
        Ok(())
    }

    /// Whether the `ai` role can change a row (it must not: it holds SELECT only and its
    /// sessions are read-only).
    async fn probe_ai_write(&self, table: &str) -> Result<bool> {
        let client = connect(&self.ai_config(1)).await?;
        let wrote = client
            .execute(
                &format!("UPDATE {table} SET amount_cents = 0 WHERE id = 1"),
                &[],
            )
            .await
            .is_ok();
        Ok(wrote)
    }

    /// Whether the `ai` role was able to write during provisioning (a breach if true).
    #[must_use]
    pub fn ai_write_allowed(&self) -> bool {
        self.ai_write_allowed
    }

    /// The schema-qualified table name.
    #[must_use]
    pub fn table_path(&self) -> String {
        format!("{}.{TABLE}", self.schema)
    }

    /// The `ai` role's name.
    #[must_use]
    pub fn role(&self) -> &str {
        &self.role
    }

    /// The config of the human pool: the scratch database's owner, `size` connections.
    #[must_use]
    pub fn human_config(&self, size: u32) -> PgConfig {
        let mut config = self.db.config();
        config.pool_max_size = size as usize;
        config.pool_wait_timeout = std::time::Duration::from_secs(10);
        "spot-ai-load-human".clone_into(&mut config.application_name);
        config
    }

    /// The config of the `ai` pool: the `ai` role, `size` connections.
    #[must_use]
    pub fn ai_config(&self, size: u32) -> PgConfig {
        let mut config = self.db.config();
        config.pool_max_size = size as usize;
        config.pool_wait_timeout = std::time::Duration::from_secs(10);
        config.user.clone_from(&self.role);
        config.password.clone_from(&self.password);
        "spot-ai-load-ai".clone_into(&mut config.application_name);
        config
    }

    /// A plain client on the scratch database for the sampler and the checks.
    ///
    /// # Errors
    ///
    /// [`HarnessError::Database`] when the connection fails.
    pub async fn observer(&self) -> Result<Client> {
        connect(&self.db.config()).await
    }

    /// Drops the schema and the role. Returns whether anything of the run is left.
    ///
    /// # Errors
    ///
    /// [`HarnessError::Database`] when the clean-up or the check after it fails.
    pub async fn teardown(&mut self) -> Result<bool> {
        let admin = self.db.raw_client().await;
        admin
            .batch_execute(&self.cleanup_sql())
            .await
            .map_err(|e| HarnessError::db("clean up", &e))?;
        self.torn_down = true;
        let left = admin
            .query_one(
                "SELECT (SELECT count(*) FROM pg_roles WHERE rolname = $1) \
                      + (SELECT count(*) FROM pg_namespace WHERE nspname = $2)",
                &[&self.role, &self.schema],
            )
            .await
            .map_err(|e| HarnessError::db("check the clean-up", &e))?;
        let count: i64 = left.get(0);
        Ok(count != 0)
    }

    /// The idempotent clean-up: the schema, then the role with everything granted to it.
    fn cleanup_sql(&self) -> String {
        let (role, schema, db) = (&self.role, &self.schema, self.db.name());
        format!(
            "DROP SCHEMA IF EXISTS {schema} CASCADE; \
             DO $$ BEGIN IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role}') THEN \
             EXECUTE 'REVOKE CONNECT ON DATABASE \"{db}\" FROM {role}'; \
             EXECUTE 'DROP OWNED BY {role}'; EXECUTE 'DROP ROLE {role}'; \
             END IF; END $$;"
        )
    }
}

impl Drop for Fixture {
    /// The net under [`Fixture::teardown`]: a run that failed or panicked still drops the role.
    /// The work happens on a short-lived thread with its own runtime, because this runs while
    /// the test's runtime may be shutting down.
    fn drop(&mut self) {
        if self.torn_down {
            return;
        }
        let sql = self.cleanup_sql();
        let db = &self.db;
        std::thread::scope(|scope| {
            let _ = scope
                .spawn(|| {
                    if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        rt.block_on(async {
                            let admin = db.raw_client().await;
                            let _ = admin.batch_execute(&sql).await;
                        });
                    }
                })
                .join();
        });
    }
}

/// Opens one plain client (its connection task is spawned) for `config`.
///
/// # Errors
///
/// [`HarnessError::Database`] when the connection fails.
pub async fn connect(config: &PgConfig) -> Result<Client> {
    let mut driver = tokio_postgres::Config::new();
    driver
        .host(&config.host)
        .port(config.port)
        .dbname(&config.dbname)
        .user(&config.user)
        .password(&config.password)
        .application_name(&config.application_name);
    let (client, connection) = driver
        .connect(NoTls)
        .await
        .map_err(|e| HarnessError::db("connect", &e))?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    Ok(client)
}

async fn run(client: &Client, step: &str, sql: &str) -> Result<()> {
    client
        .batch_execute(sql)
        .await
        .map_err(|e| HarnessError::db(step, &e))
}
