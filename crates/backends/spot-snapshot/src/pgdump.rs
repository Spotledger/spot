//! The pg-dump provider: a transaction-consistent logical snapshot of one tenant
//! database while the system runs, and the restore of it.
//!
//! Take: `pg_dump --format=directory --jobs=N --snapshot=X` where `X` is the snapshot
//! identifier (`pg_export_snapshot()`) of a held repeatable-read transaction, so the
//! dump reflects exactly that transaction horizon while writers continue (the brief's
//! "single exported snapshot"; never torn). The horizon's xmin/epoch — read through
//! `pg_current_snapshot()` inside that same transaction — goes into the manifest's
//! `consistency`.
//!
//! Restore: `pg_restore --jobs=N --exit-on-error` into a new empty database, or — only
//! with `replace = true` **and** the target's confirmation token — over the same name
//! after the old database is renamed aside (`<name>.pre-restore-<unix-ms>`); the old
//! database is never dropped by this crate. The database-exists/create/rename steps
//! run on an admin connection that carries the restore target's credentials (servers
//! behind password auth are the normal case); the secret rides the parsed config
//! in-process only, never a command line or a log.
//!
//! Secrets: the connection string is split; the password travels only in the child's
//! environment (`PGPASSWORD`). The command line holds a credential-free keyword string
//! only. Binaries come from the [`spot_pg_supervisor::layout::PgLayout`] when built
//! bundled, else from an explicit `pg_bin` path; a missing tool is
//! `snapshot.tool_missing`.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::str::FromStr;
use std::time::Duration;

use tokio::process::Command;

use crate::conn::{quote_ident, split_connection, unix_ms};
use crate::error::{SnapshotError, SnapshotResult};
use crate::hash;
use crate::manifest::{
    Consistency, DestinationPath, Snapshot, SnapshotManifest, check_destination, staging_of,
};
use crate::provider::{
    Capabilities, ChildEnv, Destination, Progress, ProgressSink, RestoreOptions, RestoreTarget,
    SnapshotOptions, SnapshotProvider, SnapshotTarget, TakeOutcome, VerifyReport,
};
use crate::restore::Restored;

/// Hard bounds for tree walks.
const MAX_FILES: u64 = 5_000_000;
const MAX_DEPTH: usize = 64;

/// Formats the `--jobs` count: clamped to the provider's maximum, at least 1.
fn jobs_argument(parallel: u32, max: u32) -> String {
    parallel.min(max).max(1).to_string()
}

/// Locates one tool: bundled layout first, then the explicit `pg_bin` path.
fn locate_tool(
    layout: Option<&spot_pg_supervisor::layout::PgLayout>,
    pg_bin: Option<&Path>,
    tool: &str,
) -> SnapshotResult<PathBuf> {
    let mut searched: Vec<String> = Vec::new();
    if let Some(layout) = layout {
        let candidate = layout.bin_dir.join(tool);
        if candidate.is_file() {
            return Ok(candidate);
        }
        searched.push(layout.bin_dir.display().to_string());
    }
    if let Some(pg_bin) = pg_bin {
        let candidate = pg_bin.join(tool);
        if candidate.is_file() {
            return Ok(candidate);
        }
        searched.push(pg_bin.display().to_string());
    }
    Err(SnapshotError::ToolMissing {
        tool: tool.to_owned(),
        searched: searched.join(", "),
    })
}

/// The pg-dump provider over one tenant database.
#[derive(Clone)]
pub struct PgDumpProvider {
    /// The bundled layout whose `bin_dir` holds the tools, when provided.
    bundle: Option<spot_pg_supervisor::layout::PgLayout>,
    /// The explicit PostgreSQL binary directory, used when no bundle is given.
    pg_bin: Option<PathBuf>,
    /// The injected clock: manifest instants and bench timing come from it only.
    clock: std::sync::Arc<dyn spot_core::Clock>,
}

impl std::fmt::Debug for PgDumpProvider {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PgDumpProvider")
            .field("pg_bin", &self.pg_bin)
            .field("bundled", &self.bundle.is_some())
            .finish()
    }
}

impl PgDumpProvider {
    /// A provider using `pg_bin` for the client tools (the `PG_BIN` path of the brief).
    /// The clock is injected (ADR-068 rule 14: tests drive the time).
    pub fn new(pg_bin: impl Into<PathBuf>, clock: std::sync::Arc<dyn spot_core::Clock>) -> Self {
        PgDumpProvider {
            bundle: None,
            pg_bin: Some(pg_bin.into()),
            clock,
        }
    }

    /// A provider using the bundled layout's binaries.
    pub fn bundled(
        bundle: spot_pg_supervisor::layout::PgLayout,
        clock: std::sync::Arc<dyn spot_core::Clock>,
    ) -> Self {
        PgDumpProvider {
            bundle: Some(bundle),
            pg_bin: None,
            clock,
        }
    }
}

impl SnapshotProvider for PgDumpProvider {
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            consistent_online: true,
            needs_stopped_instance: false,
            incremental: false,
            max_parallel: 8,
            estimated_throughput: 0,
        }
    }

    fn name(&self) -> &'static str {
        "pgdump"
    }

    fn take(
        &self,
        target: &SnapshotTarget,
        dest: &Destination,
        opts: &SnapshotOptions,
        progress: std::sync::Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<TakeOutcome>> + Send + '_>> {
        let clock = self.clock.clone();
        let bundle = self.bundle.clone();
        let pg_bin = self.pg_bin.clone();
        let target = target.clone();
        let dest = dest.clone();
        let opts = opts.clone();
        Box::pin(async move {
            take_pgdump(&target, &dest, &opts, &bundle, &pg_bin, clock, &*progress).await
        })
    }

    fn restore(
        &self,
        snapshot: &Snapshot,
        target: &RestoreTarget,
        opts: &RestoreOptions,
        progress: std::sync::Arc<dyn ProgressSink>,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<Restored>> + Send + '_>> {
        let clock = self.clock.clone();
        let bundle = self.bundle.clone();
        let pg_bin = self.pg_bin.clone();
        let snapshot = snapshot.clone();
        let target = target.clone();
        let opts = opts.clone();
        Box::pin(async move {
            restore_pgdump(
                &snapshot, &target, &opts, &bundle, &pg_bin, clock, &*progress,
            )
            .await
        })
    }

    fn verify(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<VerifyReport>> + Send + '_>> {
        let snapshot = snapshot.clone();
        Box::pin(async move { snapshot.verify().await })
    }

    fn delete(
        &self,
        snapshot: &Snapshot,
    ) -> Pin<Box<dyn Future<Output = SnapshotResult<()>> + Send + '_>> {
        let dir = snapshot.dir().to_path_buf();
        Box::pin(async move {
            tokio::fs::remove_dir_all(&dir)
                .await
                .map_err(SnapshotError::from)
        })
    }
}

/// The take, factored from the trait so every step reads in order.
#[allow(clippy::too_many_arguments)]
async fn take_pgdump(
    target: &SnapshotTarget,
    dest: &Destination,
    opts: &SnapshotOptions,
    bundle: &Option<spot_pg_supervisor::layout::PgLayout>,
    pg_bin: &Option<PathBuf>,
    clock: std::sync::Arc<dyn spot_core::Clock>,
    progress: &dyn ProgressSink,
) -> SnapshotResult<TakeOutcome> {
    let SnapshotTarget::Database {
        connection,
        tenant,
        database,
    } = target
    else {
        return Err(SnapshotError::io(
            "the pg-dump provider takes snapshots of databases, not of stopped instances",
        ));
    };
    let split = split_connection(connection)?;
    if split.database != *database {
        return Err(SnapshotError::io(format!(
            "the target database {database} does not match the connection's database {}",
            split.database
        )));
    }
    check_destination(&DestinationPath(dest.dir.clone())).await?;
    let started_at = clock.now_utc();
    progress.on(Progress::Started {
        phase: 1,
        files_total: 0,
    });

    // Stage into `<dest>.partial`; the manifest moves last and drops the suffix.
    let staged = staging_of(&dest.dir);
    let _ = tokio::fs::remove_dir_all(&staged).await;
    tokio::fs::create_dir_all(&staged).await?;
    let dump_dir = staged.join("dump");
    tokio::fs::create_dir_all(&dump_dir).await?;

    let pg_dump = locate_tool(bundle.as_ref(), pg_bin.as_deref(), "pg_dump")?;

    // One held repeatable-read transaction owns the horizon: its connection stays
    // open under `held`, the transaction under `tx`, until the dump has run and the
    // facts have been read through the same view. `pg_export_snapshot()` returns the
    // identifier (procNumber-lxid-count) that `pg_dump --snapshot=` imports through
    // `SET TRANSACTION SNAPSHOT`; `pg_current_snapshot()` reads that same horizon's
    // xmin/epoch for the manifest's consistency statement.
    let mut held = crate::conn::connect_url(connection).await?;
    let tx = held
        .client
        .build_transaction()
        .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
        .start()
        .await
        .map_err(db_err)?;
    let snapshot_id: String = tx
        .query_one("SELECT pg_export_snapshot()", &[])
        .await
        .map_err(db_err)?
        .get(0);
    let snapshot_text: String = tx
        .query_one("SELECT pg_current_snapshot()::text", &[])
        .await
        .map_err(db_err)?
        .get(0);
    let (epoch, xmin) = decode_snapshot(&snapshot_text);

    let dump_result = run_dump(
        &pg_dump,
        &dump_dir,
        &split,
        &snapshot_id,
        opts.parallel,
        opts.deadline,
    )
    .await;
    let facts_result = crate::facts::read_database_facts_through(&tx).await;
    let _ = tx.rollback().await;
    drop(held);

    let dumped = match dump_result {
        Ok(dumped) => dumped,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&staged).await;
            return Err(e);
        }
    };
    if !dumped {
        let _ = tokio::fs::remove_dir_all(&staged).await;
        return Err(SnapshotError::io(
            "pg_dump failed; the staged destination was removed",
        ));
    }
    let facts = match facts_result {
        Ok(facts) => facts,
        Err(e) => {
            let _ = tokio::fs::remove_dir_all(&staged).await;
            return Err(e);
        }
    };

    let inventory = hash_inventory_tree(&staged, progress).await?;
    let finished_at = clock.now_utc();
    let manifest = build_manifest(
        tenant,
        "pgdump",
        &facts,
        started_at,
        finished_at,
        Consistency::OnlineConsistent { xmin, epoch },
        &inventory,
    );
    // Written last; drops the `.partial` staging suffix exactly now.
    manifest.publish(&dest.dir).await?;
    progress.on(Progress::Finished { ok: true });
    Ok(TakeOutcome {
        snapshot: Snapshot::open(&dest.dir).await?,
    })
}

/// Runs `pg_dump` against the exported horizon.
async fn run_dump(
    pg_dump: &Path,
    dump_dir: &Path,
    split: &crate::conn::SplitConnection,
    snapshot_text: &str,
    parallel: u32,
    deadline: Duration,
) -> SnapshotResult<bool> {
    let mut cmd = Command::new(pg_dump);
    cmd.arg("--format=directory")
        .arg("--file")
        .arg(dump_dir)
        .arg("--jobs")
        .arg(jobs_argument(parallel, 8))
        .arg(format!("--snapshot={snapshot_text}"))
        .arg(&split.keyword);
    split.env.apply(&mut cmd);
    run_child(&mut cmd, "pg_dump", deadline).await
}

/// Hashes an existing staging tree in place into an inventory (no copying).
pub(crate) async fn hash_inventory_tree(
    root: &Path,
    progress: &dyn ProgressSink,
) -> SnapshotResult<hash::Inventory> {
    let mut inventory = hash::Inventory::new();
    let prefix_len = root.to_string_lossy().len().saturating_add(1);
    let mut queue: std::collections::VecDeque<(PathBuf, usize)> = std::collections::VecDeque::new();
    queue.push_back((root.to_path_buf(), 0));
    let mut bytes_done: u64 = 0;
    let mut walked: u64 = 0;
    while let Some((current, depth)) = queue.pop_front() {
        walked += 1;
        if walked > MAX_FILES || depth > MAX_DEPTH {
            return Err(SnapshotError::io("the tree exceeds the walk bounds"));
        }
        let mut entries = tokio::fs::read_dir(&current).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let meta = tokio::fs::symlink_metadata(&path).await?;
            if meta.is_dir() {
                queue.push_back((path, depth + 1));
                continue;
            }
            if meta.file_type().is_symlink() {
                return Err(SnapshotError::io(format!(
                    "refusing to hash symlink {} out of the snapshot",
                    path.display()
                )));
            }
            let checksum = hash::sha256_file(&path).await?;
            let rel = path
                .to_string_lossy()
                .get(prefix_len..)
                .unwrap_or("")
                .replace('\\', "/");
            bytes_done = bytes_done.saturating_add(checksum.bytes);
            inventory.insert(rel, checksum);
            progress.on(Progress::FileDone { bytes_done });
        }
    }
    Ok(inventory)
}

/// Builds the manifest from the collected facts (shared by both providers).
pub(crate) fn build_manifest(
    tenant: &str,
    provider_kind: &str,
    facts: &crate::facts::DatabaseFacts,
    started_at: time::OffsetDateTime,
    finished_at: time::OffsetDateTime,
    consistency: Consistency,
    inventory: &hash::Inventory,
) -> SnapshotManifest {
    let total_bytes: u64 = inventory
        .values()
        .try_fold(0u64, |sum, file| Some(sum.saturating_add(file.bytes)))
        .unwrap_or(u64::MAX);
    SnapshotManifest {
        id: uuid::Uuid::now_v7().to_string(),
        tenant: tenant.to_owned(),
        provider: crate::PROVIDER_NAME.to_owned(),
        provider_version: crate::PROVIDER_VERSION.to_owned(),
        provider_kind: provider_kind.to_owned(),
        pg_major: crate::SUPPORTED_MAJOR_VERSION,
        catalogue_version: facts.catalogue_version.clone(),
        feed_schema: facts.feed_schema.clone(),
        outbox_generation: facts.outbox_generation,
        outbox_last_published_seq: facts.outbox_last_published_seq,
        started_at: SnapshotManifest::format_instant(started_at),
        finished_at: SnapshotManifest::format_instant(finished_at),
        consistency,
        total_bytes,
        file_count: u64::try_from(inventory.len()).unwrap_or(u64::MAX),
        files: inventory.clone(),
        node_local: node_local_marks(facts),
        platform_extra: Default::default(),
    }
}

/// The node-local marks of one take: the node id (when present) and the system-table
/// row counts a restore must reproduce (the shipped checks compare them).
pub(crate) fn node_local_marks(facts: &crate::facts::DatabaseFacts) -> BTreeMap<String, String> {
    let mut marks = BTreeMap::new();
    if let Some(node_id) = &facts.node_id {
        marks.insert(crate::NODE_LOCAL_NODE_ID_KEY.to_owned(), node_id.clone());
    }
    for (table, count) in &facts.system_counts {
        marks.insert(
            format!("{}{table}", crate::facts::SYSTEM_COUNTS_PREFIX),
            count.to_string(),
        );
    }
    marks
}

/// The restore, factored from the trait.
#[allow(clippy::too_many_arguments)]
async fn restore_pgdump(
    snapshot: &Snapshot,
    target: &RestoreTarget,
    opts: &RestoreOptions,
    bundle: &Option<spot_pg_supervisor::layout::PgLayout>,
    pg_bin: &Option<PathBuf>,
    clock: std::sync::Arc<dyn spot_core::Clock>,
    progress: &dyn ProgressSink,
) -> SnapshotResult<Restored> {
    opts.check_replace(target)?;
    let RestoreTarget::Database {
        connection,
        database,
    } = target
    else {
        return Err(SnapshotError::io(
            "the pg-dump provider restores into databases, not into stopped instances",
        ));
    };
    progress.on(Progress::Started {
        phase: 2,
        files_total: 0,
    });
    // A changed byte refuses the restore before anything is touched.
    snapshot.verify().await?;
    let split = split_connection(connection)?;
    if split.database != *database {
        return Err(SnapshotError::io(format!(
            "the target database {database} does not match the connection's database {}",
            split.database
        )));
    }

    let pg_restore = locate_tool(bundle.as_ref(), pg_bin.as_deref(), "pg_restore")?;
    let replaced_aside = if opts.replace {
        Some(rename_database_aside(connection, database, unix_ms(&clock)).await?)
    } else {
        None
    };
    // The name must be free now; a take-over without replace is refused (the caller
    // named a populated database and did not consent to replace).
    if database_exists(connection, database).await? {
        return Err(SnapshotError::ReplaceRequired {
            target: database.clone(),
        });
    }
    create_database(connection, database).await?;
    // A restore into a fresh name is a scratch the caller may drop again (`drop_scratch`);
    // a `replace` takes over a live name, so that database is never marked scratch.
    if !opts.replace {
        crate::scratch::mark_scratch(
            connection,
            database,
            &crate::scratch::ScratchMarker::for_snapshot(snapshot),
        )
        .await?;
    }

    // Materialise the dump files into a scratch dir, verified as they land.
    let scratch =
        std::env::temp_dir().join(format!("spot-restore-{}", uuid::Uuid::now_v7().simple()));
    let dump_inventory: hash::Inventory = snapshot
        .manifest()
        .files
        .iter()
        .filter(|(rel, _)| rel.starts_with("dump/"))
        .map(|(rel, checksum)| (rel.clone(), checksum.clone()))
        .collect();
    let copied =
        hash::copy_manifest_files(snapshot.dir(), &dump_inventory, &scratch, "dump/", progress);
    if let Err(e) = copied.await {
        let _ = tokio::fs::remove_dir_all(&scratch).await;
        return Err(e);
    }

    let (_, env) = ChildEnv::from_connection(connection)?;
    let mut cmd = Command::new(&pg_restore);
    cmd.arg("--format=directory")
        .arg("--dbname")
        .arg(dbname_keyword(connection, database))
        .arg("--jobs")
        .arg(jobs_argument(opts.parallel, 8))
        .arg("--exit-on-error")
        .arg(&scratch);
    env.apply(&mut cmd);
    let resolved = run_child(&mut cmd, "pg_restore", opts.deadline).await;
    let _ = tokio::fs::remove_dir_all(&scratch).await;
    if !resolved? {
        return Err(SnapshotError::io(format!(
            "pg_restore failed; the restored database {database} is left incomplete and non-activated"
        )));
    }
    progress.on(Progress::Finished { ok: true });
    Ok(Restored {
        snapshot: snapshot.clone(),
        location: database.clone(),
        activated: true,
        replaced_aside: replaced_aside.map(|aside| format!("db {aside}")),
    })
}

/// The credential-free keyword string narrowed to `database` on the same server.
fn dbname_keyword(connection: &str, database: &str) -> String {
    crate::conn::split_connection(&server_url_of(connection, database))
        .map(|split| split.keyword)
        .unwrap_or_default()
}

/// The URL of `database` on the same server as `connection` (credential-free; the
/// password rides the child's env built from the original connection separately).
fn server_url_of(connection: &str, database: &str) -> String {
    let parsed = match tokio_postgres::Config::from_str(connection) {
        Ok(parsed) => parsed,
        Err(_) => return format!("postgres://localhost:5432/{database}"),
    };
    let user = parsed.get_user().unwrap_or("postgres");
    let host = match parsed.get_hosts().first() {
        Some(tokio_postgres::config::Host::Tcp(host)) => host.clone(),
        _ => "localhost".to_owned(),
    };
    let port = parsed.get_ports().first().copied().unwrap_or(5432);
    format!("postgres://{user}@{host}:{port}/{database}")
}

/// Runs one child to completion, enforcing the deadline at the await points.
async fn run_child(cmd: &mut Command, tool: &str, deadline: Duration) -> SnapshotResult<bool> {
    let mut child = match cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Err(SnapshotError::io(format!("cannot start {tool}: {e}"))),
    };
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| SnapshotError::io(format!("{tool}'s stderr pipe is missing")))?;
    let stderr_reader = tokio::spawn(async move {
        let mut buffer = Vec::new();
        let mut stderr = stderr;
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut buffer).await;
        String::from_utf8_lossy(&buffer).trim().to_owned()
    });
    let run = async {
        child
            .wait()
            .await
            .map(|status| status.success())
            .map_err(|e| SnapshotError::io(format!("{tool} failed while running: {e}")))
    };
    let result = if deadline.is_zero() {
        run.await
    } else {
        match tokio::time::timeout(deadline, run).await {
            Ok(result) => result,
            Err(_) => Err(SnapshotError::io(format!(
                "{tool} did not finish within the deadline; it is not killed mid-write, retry the operation"
            ))),
        }
    };
    let stderr_text = stderr_reader.await.unwrap_or_default();
    match result {
        // A failed child's stderr is the only diagnostic the caller gets; it names
        // objects and servers, never secrets (the connection rides the child env).
        Ok(false) => Err(SnapshotError::io(format!(
            "{tool} failed{}",
            if stderr_text.is_empty() {
                ".".to_owned()
            } else {
                format!(": {stderr_text}")
            }
        ))),
        Err(e) => Err(e),
        Ok(true) => Ok(true),
    }
}

/// Decodes `pg_current_snapshot()` text (`xmin:xmax:xip-list`, or with a leading epoch
/// field after an xid wrap) into `(epoch, xmin)`.
fn decode_snapshot(text: &str) -> (u64, u64) {
    let mut fields = text.split(':');
    let a = fields.next().unwrap_or("");
    let b = fields.next().unwrap_or("");
    let parsed_a = a.parse::<u64>().unwrap_or(0);
    // After a wrap the text starts with the epoch (a number far beyond the u32 xid
    // range, 11+ digits in practice) and the xmin follows; before it, the first field
    // is the xmin itself.
    if a.len() >= 11 && !b.is_empty() && b.parse::<u64>().is_ok() {
        (parsed_a, b.parse::<u64>().unwrap_or(0))
    } else {
        (0, parsed_a)
    }
}

/// The admin connection string (same server, maintenance database `postgres`),
/// carrying `connection`'s credentials: the database-exists/create/rename steps run
/// against servers that require a password (never a trust setup), and the secret
/// rides the parsed config in-process only — never onto a command line.
pub(crate) fn admin_url_of(connection: &str) -> String {
    let parsed = match tokio_postgres::Config::from_str(connection) {
        Ok(parsed) => parsed,
        Err(_) => return "postgres://localhost:5432/postgres".to_owned(),
    };
    let user = parsed.get_user().unwrap_or("postgres");
    let host = match parsed.get_hosts().first() {
        Some(tokio_postgres::config::Host::Tcp(host)) => host.clone(),
        _ => "localhost".to_owned(),
    };
    let port = parsed.get_ports().first().copied().unwrap_or(5432);
    match parsed.get_password() {
        Some(password) => format!(
            "postgres://{}:{}@{host}:{port}/postgres",
            user,
            percent_encode(&String::from_utf8_lossy(password))
        ),
        None => format!("postgres://{user}@{host}:{port}/postgres"),
    }
}

/// Percent-encodes every byte outside the URL's unreserved set, so a decoded
/// password rounds through a credential URL safely (byte-wise; ASCII-safe).
fn percent_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// Whether `database` exists on the target server (admin connection, credentials
/// from the restore target's connection string).
async fn database_exists(connection: &str, database: &str) -> SnapshotResult<bool> {
    let held = crate::conn::connect_url(&admin_url_of(connection)).await?;
    let row = held
        .client
        .query_opt("SELECT 1 FROM pg_database WHERE datname = $1", &[&database])
        .await
        .map_err(db_err)?;
    Ok(row.is_some())
}

/// Creates an empty database (template0, UTF-8, ICU root locale) as the restore target.
async fn create_database(connection: &str, database: &str) -> SnapshotResult<()> {
    let held = crate::conn::connect_url(&admin_url_of(connection)).await?;
    let quoted = quote_ident(database)?;
    held.client
        .batch_execute(&format!(
            "CREATE DATABASE {quoted} TEMPLATE template0 ENCODING 'UTF8' \
             LOCALE 'C' LOCALE_PROVIDER icu ICU_LOCALE 'und'"
        ))
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Renames the populated target database aside; the old database is kept and never
/// dropped by this crate.
async fn rename_database_aside(
    connection: &str,
    database: &str,
    at_unix_ms: u64,
) -> SnapshotResult<String> {
    let aside = format!("{database}.pre-restore-{at_unix_ms}");
    let held = crate::conn::connect_url(&admin_url_of(connection)).await?;
    // Terminate live backends first: PostgreSQL refuses RENAME under load.
    held.client
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = $1 AND pid <> pg_backend_pid()",
            &[&database],
        )
        .await
        .map_err(db_err)?;
    let quoted_old = quote_ident(database)?;
    let quoted_new = quote_ident(&aside)?;
    held.client
        .batch_execute(&format!(
            "ALTER DATABASE {quoted_old} RENAME TO {quoted_new}"
        ))
        .await
        .map_err(db_err)?;
    Ok(aside)
}

pub(crate) fn db_err(e: tokio_postgres::Error) -> SnapshotError {
    SnapshotError::io(format!("the database refused the operation: {e}"))
}
