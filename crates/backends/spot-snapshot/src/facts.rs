//! Invariant checks over a restored database, plus the fact-gathering a take needs:
//! catalogue version, feed schema, outbox generation and last published position,
//! node id, and every `spot_*` system table's row count. The take reads these through
//! its own connection (or held horizon); the shipped checks compare them against live
//! reads of the restored site. All database access here is read-only.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use crate::error::SnapshotResult;
use crate::manifest::SnapshotManifest;
use crate::restore::Violation;

/// The facts a take records about the logical state of the database it snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DatabaseFacts {
    /// `catalog_version` from `spot_meta` (empty when absent).
    pub catalogue_version: String,
    /// `feed_schema` from `spot_meta` (empty when absent).
    pub feed_schema: String,
    /// `feed_generation` from `spot_meta` (0 when absent).
    pub outbox_generation: u64,
    /// The last published outbox seq in this generation (`spot_outbox_pub` max, or 0).
    pub outbox_last_published_seq: u64,
    /// The node id (spot_meta `node_id`), when the database carries one; node-local
    /// and marked in the manifest for reset after a restore.
    pub node_id: Option<String>,
    /// Row counts of every public `spot_*` system table, integer, name-sorted.
    pub system_counts: BTreeMap<String, u64>,
}

/// Reads the facts through a plain driver client the caller owns.
///
/// # Errors
/// `snapshot.io` with the database's text when a read fails.
pub async fn read_database_facts(client: &tokio_postgres::Client) -> SnapshotResult<DatabaseFacts> {
    let meta = meta_map(client).await?;
    read_facts_from_meta(client, meta).await
}

/// Reads the facts through a transaction the caller holds (the take's horizon: the
/// facts then match the dumped data exactly).
///
/// # Errors
/// `snapshot.io` with the database's text when a read fails.
pub async fn read_database_facts_through(
    tx: &tokio_postgres::Transaction<'_>,
) -> SnapshotResult<DatabaseFacts> {
    let meta = meta_map_tx(tx).await?;
    read_facts_from_meta_tx(tx, meta).await
}

async fn meta_map(client: &tokio_postgres::Client) -> SnapshotResult<BTreeMap<String, String>> {
    let rows = client
        .query("SELECT key, value FROM spot_meta", &[])
        .await
        .map_err(db)?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
        .collect())
}

async fn meta_map_tx(
    tx: &tokio_postgres::Transaction<'_>,
) -> SnapshotResult<BTreeMap<String, String>> {
    let rows = tx
        .query("SELECT key, value FROM spot_meta", &[])
        .await
        .map_err(db)?;
    Ok(rows
        .into_iter()
        .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
        .collect())
}

async fn read_facts_from_meta(
    client: &tokio_postgres::Client,
    meta: BTreeMap<String, String>,
) -> SnapshotResult<DatabaseFacts> {
    let generation = meta
        .get("feed_generation")
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0);
    let last_seq = scalar_u64(client, "SELECT COALESCE(MAX(seq), 0) FROM spot_outbox_pub").await?;
    let system_counts = system_counts(client).await?;
    Ok(DatabaseFacts {
        catalogue_version: meta.get("catalog_version").cloned().unwrap_or_default(),
        feed_schema: meta.get("feed_schema").cloned().unwrap_or_default(),
        outbox_generation: generation,
        outbox_last_published_seq: last_seq,
        node_id: meta.get("node_id").cloned(),
        system_counts,
    })
}

async fn read_facts_from_meta_tx(
    tx: &tokio_postgres::Transaction<'_>,
    meta: BTreeMap<String, String>,
) -> SnapshotResult<DatabaseFacts> {
    let generation = meta
        .get("feed_generation")
        .and_then(|text| text.parse::<u64>().ok())
        .unwrap_or(0);
    let last_seq = scalar_u64_tx(tx, "SELECT COALESCE(MAX(seq), 0) FROM spot_outbox_pub").await?;
    let system_counts = system_counts_tx(tx).await?;
    Ok(DatabaseFacts {
        catalogue_version: meta.get("catalog_version").cloned().unwrap_or_default(),
        feed_schema: meta.get("feed_schema").cloned().unwrap_or_default(),
        outbox_generation: generation,
        outbox_last_published_seq: last_seq,
        node_id: meta.get("node_id").cloned(),
        system_counts,
    })
}

/// One unsigned-integer scalar query.
async fn scalar_u64(client: &tokio_postgres::Client, sql: &str) -> SnapshotResult<u64> {
    let row = client.query_one(sql, &[]).await.map_err(db)?;
    Ok(u64::try_from(row.get::<_, i64>(0)).unwrap_or(u64::MAX))
}

async fn scalar_u64_tx(tx: &tokio_postgres::Transaction<'_>, sql: &str) -> SnapshotResult<u64> {
    let row = tx.query_one(sql, &[]).await.map_err(db)?;
    Ok(u64::try_from(row.get::<_, i64>(0)).unwrap_or(u64::MAX))
}

/// The public `spot_*` tables, name-sorted.
async fn system_tables(client: &tokio_postgres::Client) -> SnapshotResult<Vec<String>> {
    let rows = client
        .query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name LIKE 'spot\\_%' \
             ORDER BY table_name",
            &[],
        )
        .await
        .map_err(db)?;
    Ok(rows
        .into_iter()
        .map(|row| row.get::<_, String>(0))
        .collect())
}

async fn system_tables_tx(tx: &tokio_postgres::Transaction<'_>) -> SnapshotResult<Vec<String>> {
    let rows = tx
        .query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = 'public' AND table_name LIKE 'spot\\_%' \
             ORDER BY table_name",
            &[],
        )
        .await
        .map_err(db)?;
    Ok(rows
        .into_iter()
        .map(|row| row.get::<_, String>(0))
        .collect())
}

/// The row count of every public `spot_*` table. Tables come from the database's own
/// `information_schema` (our schema, our names); the `COUNT(*)` text is built from
/// those validated identifiers only — the pattern of the storage crate itself.
async fn system_counts(client: &tokio_postgres::Client) -> SnapshotResult<BTreeMap<String, u64>> {
    let mut counts = BTreeMap::new();
    for table in system_tables(client).await? {
        let count = scalar_u64(client, &format!("SELECT COUNT(*) FROM {table}")).await?;
        counts.insert(table, count);
    }
    Ok(counts)
}

async fn system_counts_tx(
    tx: &tokio_postgres::Transaction<'_>,
) -> SnapshotResult<BTreeMap<String, u64>> {
    let mut counts = BTreeMap::new();
    for table in system_tables_tx(tx).await? {
        let count = scalar_u64_tx(tx, &format!("SELECT COUNT(*) FROM {table}")).await?;
        counts.insert(table, count);
    }
    Ok(counts)
}

fn db(e: tokio_postgres::Error) -> crate::error::SnapshotError {
    crate::error::SnapshotError::io(format!("reading the database facts failed: {e}"))
}

/// The shipped catalogue-version check's core: compares the manifest's
/// `catalogue_version` against the live read of the restored database. Returns the
/// violation when they differ (and none when they match).
pub fn catalogue_version_violation(
    manifest: &SnapshotManifest,
    restored_version: &str,
) -> Option<Violation> {
    if manifest.catalogue_version == restored_version {
        return None;
    }
    Some(Violation {
        check: "catalogue_version".to_owned(),
        detail: format!(
            "the restored database's catalogue_version is `{restored_version}` but the snapshot's is `{}`",
            manifest.catalogue_version
        ),
    })
}

/// The shipped row-count check's core: compares every manifest-recorded
/// `system_counts.*` mark against the live counts. The checks compare *every* table
/// the manifest recorded; a table present live but unrecorded is a violation too
/// (the restore must reproduce the snapshot, nothing more).
pub fn system_table_count_violations(
    manifest: &SnapshotManifest,
    restored_counts: &BTreeMap<String, u64>,
) -> Vec<Violation> {
    let recorded = manifest_counts(manifest);
    let mut names: Vec<String> = recorded
        .keys()
        .chain(restored_counts.keys())
        .cloned()
        .collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| {
            let before = recorded.get(&name).copied().unwrap_or(0);
            let after = restored_counts.get(&name).copied().unwrap_or(0);
            (before != after).then(|| Violation {
                check: "system_table_counts".to_owned(),
                detail: format!(
                    "system table {name} had {before} rows in the manifest but has {after} after the restore"
                ),
            })
        })
        .collect()
}

/// The recorded counts from the manifest's `system_counts.<table>` marks.
fn manifest_counts(manifest: &SnapshotManifest) -> BTreeMap<String, u64> {
    manifest
        .node_local
        .iter()
        .filter_map(|(key, value)| {
            key.strip_prefix(SYSTEM_COUNTS_PREFIX)
                .map(|table| (table.to_owned(), value.parse::<u64>().unwrap_or(0)))
        })
        .collect()
}

/// The mark prefix under which system-table row counts ride in `node_local`.
pub const SYSTEM_COUNTS_PREFIX: &str = "system_counts.";

/// Builds the shipped invariant checks: catalogue version and system-table counts,
/// wired with live reads through `backend`. The host registers them (plus its own)
/// when calling `restore_and_check`.
pub fn shipped_checks(
    backend: spot_postgres::PgBackend,
) -> Vec<Box<dyn crate::restore::InvariantCheck>> {
    vec![
        Box::new(CatalogueVersionCheck {
            backend: backend.clone(),
        }),
        Box::new(SystemTableCountsCheck { backend }),
    ]
}

/// The catalogue-version check: the restored database's `catalog_version` must equal
/// the snapshot manifest's.
struct CatalogueVersionCheck {
    backend: spot_postgres::PgBackend,
}

impl crate::restore::InvariantCheck for CatalogueVersionCheck {
    fn name(&self) -> &'static str {
        "catalogue_version"
    }

    fn check(
        &self,
        restored: &crate::manifest::Snapshot,
    ) -> Pin<Box<dyn Future<Output = Vec<Violation>> + Send + '_>> {
        let backend = self.backend.clone();
        let manifest = restored.manifest().clone();
        Box::pin(async move {
            match read_live_meta(&backend, "catalog_version").await {
                Ok(live) => catalogue_version_violation(&manifest, &live)
                    .into_iter()
                    .collect(),
                Err(e) => vec![Violation {
                    check: "catalogue_version".to_owned(),
                    detail: format!("the live read failed: {e}"),
                }],
            }
        })
    }
}

/// The system-table-counts check: every `spot_*` table's row count must equal the
/// manifest's recorded count.
struct SystemTableCountsCheck {
    backend: spot_postgres::PgBackend,
}

impl crate::restore::InvariantCheck for SystemTableCountsCheck {
    fn name(&self) -> &'static str {
        "system_table_counts"
    }

    fn check(
        &self,
        restored: &crate::manifest::Snapshot,
    ) -> Pin<Box<dyn Future<Output = Vec<Violation>> + Send + '_>> {
        let backend = self.backend.clone();
        let manifest = restored.manifest().clone();
        Box::pin(async move {
            match read_live_counts(&backend).await {
                Ok(counts) => system_table_count_violations(&manifest, &counts),
                Err(e) => vec![Violation {
                    check: "system_table_counts".to_owned(),
                    detail: format!("the live read failed: {e}"),
                }],
            }
        })
    }
}

/// Reads one `spot_meta` value through a raw connection to the restored site. The
/// backend's pool is not used `PgBackend::checkout` is crate-private to
/// `spot-postgres`); facts.rs builds its own one-off connections.
async fn read_live_meta(backend: &spot_postgres::PgBackend, key: &str) -> SnapshotResult<String> {
    let url = public_connection_url(backend.config());
    let held = crate::conn::connect_url(&url).await?;
    let rows = held
        .client
        .query("SELECT value FROM spot_meta WHERE key = $1", &[&key])
        .await
        .map_err(|e| crate::error::SnapshotError::io(format!("the live read failed: {e}")))?;
    Ok(rows
        .first()
        .map(|row| row.get::<_, String>(0))
        .unwrap_or_default())
}

/// Reads the live system-table counts through a raw connection.
async fn read_live_counts(
    backend: &spot_postgres::PgBackend,
) -> SnapshotResult<BTreeMap<String, u64>> {
    let url = public_connection_url(backend.config());
    let held = crate::conn::connect_url(&url).await?;
    system_counts(&held.client).await
}

/// Rebuilds the credential-free URL of a config. The password of a live site rides
/// the `PGPASSWORD` env of the connection builder (raw_connect reads the config), so
/// the URL text itself never carries it.
fn public_connection_url(config: &spot_postgres::PgConfig) -> String {
    format!(
        "postgres://{}@{}:{}/{}",
        config.user, config.host, config.port, config.dbname
    )
}
