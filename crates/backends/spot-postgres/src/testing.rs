//! Test support: one fresh database per test (feature `testing`).
//!
//! ```text
//! SPOT_TEST_PG_URL=postgres://spot:spot@localhost:5432/postgres   # an admin connection URL
//! SPOT_REQUIRE_PG=1                                               # make a missing URL a failure
//! ```
//!
//! [`TestDb::create`] returns `None` after printing one `SKIPPED:` line when no server is
//! configured, unless `SPOT_REQUIRE_PG=1`, in which case it panics so CI cannot pass by skipping.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio_postgres::Client;
use uuid::Uuid;

use crate::config::PgConfig;
use crate::conn::{TlsMode, connect_raw};

/// Test databases older than this are dropped by the sweep in [`TestDb::create`].
const SWEEP_AFTER: Duration = Duration::from_secs(60 * 60);

const NAME_PREFIX: &str = "spot_test_";

/// A scratch database created for one test and dropped with it.
pub struct TestDb {
    admin: PgConfig,
    name: String,
    config: PgConfig,
}

impl TestDb {
    /// Creates `spot_test_<uuidv7>` with ICU as its collation provider and returns it, or `None`
    /// (after printing `SKIPPED: set SPOT_TEST_PG_URL`) when no server is configured. Also drops
    /// test databases older than one hour left behind by earlier runs.
    ///
    /// # Panics
    ///
    /// When `SPOT_TEST_PG_URL` is unset and `SPOT_REQUIRE_PG=1`, and when the configured server
    /// cannot create the database: a test harness that cannot run must be loud.
    pub async fn create() -> Option<TestDb> {
        let Ok(url) = std::env::var("SPOT_TEST_PG_URL") else {
            if std::env::var("SPOT_REQUIRE_PG").as_deref() == Ok("1") {
                panic!("SPOT_REQUIRE_PG=1 but SPOT_TEST_PG_URL is not set");
            }
            println!("SKIPPED: set SPOT_TEST_PG_URL");
            return None;
        };
        let admin = PgConfig::from_url(&url)
            .unwrap_or_else(|e| panic!("SPOT_TEST_PG_URL is not usable: {e}"));
        let tls = tls_for(&admin);
        let client = connect_raw(&admin, &tls)
            .await
            .unwrap_or_else(|e| panic!("cannot connect to SPOT_TEST_PG_URL: {e}"));
        sweep(&client).await;
        let name = format!("{NAME_PREFIX}{}", Uuid::now_v7().simple());
        // The name is generated here from hex digits; identifiers cannot be bound parameters.
        let create = format!(
            "CREATE DATABASE \"{name}\" TEMPLATE template0 ENCODING 'UTF8' \
             LOCALE 'C' LOCALE_PROVIDER icu ICU_LOCALE 'und'"
        );
        client
            .batch_execute(&create)
            .await
            .unwrap_or_else(|e| panic!("cannot create the test database: {e}"));
        let config = admin.clone().with_dbname(&name);
        Some(TestDb {
            admin,
            name,
            config,
        })
    }

    /// The config for this scratch database.
    pub fn config(&self) -> PgConfig {
        self.config.clone()
    }

    /// The database's name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// A plain driver client on the scratch database, for creating tables and checking state.
    pub async fn raw_client(&self) -> Client {
        connect_raw(&self.config, &tls_for(&self.config))
            .await
            .unwrap_or_else(|e| panic!("cannot connect to the test database: {e}"))
    }

    /// Creates the extensions in `names` in this scratch database (ADR-087: `postgis`,
    /// `btree_gist`, `pg_trgm`, `unaccent`) with the test harness' own connection, standing in
    /// for the audited `extension.install` operation. Returns the names the server does not
    /// offer (nothing is created for them); a name it offers but cannot create panics.
    pub async fn create_extensions(&self, names: &[&str]) -> Vec<String> {
        let client = self.raw_client().await;
        let mut missing = Vec::new();
        for name in names {
            let offered = client
                .query_opt(
                    "SELECT 1 FROM pg_available_extensions WHERE name = $1",
                    &[name],
                )
                .await
                .unwrap_or_else(|e| panic!("cannot read pg_available_extensions: {e}"))
                .is_some();
            if !offered {
                missing.push((*name).to_owned());
                continue;
            }
            // The name is one of the closed list above, never user input.
            client
                .batch_execute(&format!("CREATE EXTENSION IF NOT EXISTS \"{name}\""))
                .await
                .unwrap_or_else(|e| panic!("cannot create the extension {name}: {e}"));
        }
        missing
    }

    /// A plain driver client on the admin database, for `pg_terminate_backend` and the like.
    pub async fn admin_client(&self) -> Client {
        connect_raw(&self.admin, &tls_for(&self.admin))
            .await
            .unwrap_or_else(|e| panic!("cannot connect as admin: {e}"))
    }
}

impl Drop for TestDb {
    /// Drops the database, best effort. The runtime of a `#[tokio::test]` is shutting down when
    /// this runs, so the work happens on a short-lived thread with its own runtime; failures
    /// are left to the next run's sweep.
    fn drop(&mut self) {
        let admin = self.admin.clone();
        let name = self.name.clone();
        let job = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                if let Ok(client) = connect_raw(&admin, &tls_for(&admin)).await {
                    drop_database(&client, &name).await;
                }
            });
        });
        let _ = job.join();
    }
}

fn tls_for(config: &PgConfig) -> TlsMode {
    TlsMode::new(config.tls).unwrap_or(TlsMode::Plain)
}

async fn drop_database(client: &Client, name: &str) {
    // `name` is one of ours (`spot_test_` and hex), checked by the callers.
    if !is_test_db_name(name) {
        return;
    }
    let _ = client
        .batch_execute(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
        .await;
}

/// Drops test databases whose UUIDv7 name says they are older than [`SWEEP_AFTER`].
async fn sweep(client: &Client) {
    let Ok(rows) = client
        .query(
            "SELECT datname FROM pg_database WHERE datname LIKE 'spot\\_test\\_%'",
            &[],
        )
        .await
    else {
        return;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    for row in rows {
        let name: String = row.get(0);
        if let Some(created) = created_at(&name)
            && now.saturating_sub(created) > SWEEP_AFTER
        {
            drop_database(client, &name).await;
        }
    }
}

fn is_test_db_name(name: &str) -> bool {
    name.strip_prefix(NAME_PREFIX)
        .is_some_and(|rest| rest.len() == 32 && rest.chars().all(|c| c.is_ascii_hexdigit()))
}

/// When a test database was created, from the UUIDv7 in its name.
fn created_at(name: &str) -> Option<Duration> {
    if !is_test_db_name(name) {
        return None;
    }
    let id = Uuid::parse_str(name.strip_prefix(NAME_PREFIX)?).ok()?;
    let (secs, nanos) = id.get_timestamp()?.to_unix();
    Some(Duration::new(secs, nanos))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn names_carry_their_creation_time() {
        let name = format!("{NAME_PREFIX}{}", Uuid::now_v7().simple());
        assert!(is_test_db_name(&name));
        let age = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .saturating_sub(created_at(&name).unwrap());
        assert!(age < Duration::from_secs(5));
    }

    #[test]
    fn only_our_names_are_swept() {
        assert!(created_at("postgres").is_none());
        assert!(created_at("spot_test_zz").is_none());
        assert!(created_at("spot_test_0123456789abcdef0123456789abcdex").is_none());
        assert!(!is_test_db_name("spot_prod"));
    }
}
