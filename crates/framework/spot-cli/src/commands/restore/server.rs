//! The PostgreSQL side: the maintenance gate, quiescing the site, the atomic swap and its undo.

use spot_core::{Clock, SiteId};
use spot_migration_apply::gate::{AdmissionGate, GateStatus};
use spot_migration_apply::journal::Progress as JournalProgress;
use spot_migration_apply_pg::{PgAdmissionGate, PgApplyJournal, PgApplySchema};
use spot_postgres::{PgBackend, PgConfig};
use spot_snapshot::ScratchMarker;
use std::sync::Arc;
use std::time::Duration;

use super::guard::{LockState, StateLock};
use super::{CommandCtx, DRAIN_MICROS, Prepared, ident, micros, say, state_dir};

pub(super) fn pool_error(_: impl std::fmt::Display) -> String {
    "the database server could not be reached".to_owned()
}

/// A short-lived backend on the `postgres` maintenance database, for create/rename/drop.
pub(super) async fn admin(cfg: &PgConfig) -> Result<PgBackend, String> {
    let mut config = cfg.clone().with_dbname("postgres");
    config.pool_max_size = 2;
    PgBackend::connect(config).await.map_err(pool_error)
}

pub(super) async fn on_db(cfg: &PgConfig, database: &str) -> Result<PgBackend, String> {
    let mut config = cfg.clone().with_dbname(database);
    config.pool_max_size = 2;
    PgBackend::connect(config).await.map_err(pool_error)
}

/// Runs `sql` on the maintenance database.
pub(super) async fn admin_exec(cfg: &PgConfig, sql: &str) -> Result<(), String> {
    let backend = admin(cfg).await?;
    let client = backend.pool().get().await.map_err(pool_error)?;
    client.batch_execute(sql).await.map_err(|e| {
        format!(
            "the database server refused: {}",
            e.as_db_error().map_or("an error", |d| d.message())
        )
    })
}

/// Why the site may not be restored right now: its gate is closed or a migration is running.
pub(super) async fn busy(
    cfg: &PgConfig,
    database: &str,
    site: &str,
) -> Result<Option<String>, String> {
    let backend = on_db(cfg, database).await?;
    let client = backend.pool().get().await.map_err(pool_error)?;
    let present = client
        .query_one(
            "SELECT to_regclass('spot_migration_control.gate') IS NOT NULL",
            &[],
        )
        .await
        .map_err(pool_error)?;
    if !present.get::<_, bool>(0) {
        return Ok(None);
    }
    let gate = client
        .query_opt(
            "SELECT state, reason FROM spot_migration_control.gate WHERE site = $1",
            &[&site],
        )
        .await
        .map_err(pool_error)?;
    if let Some(row) = gate.filter(|r| r.get::<_, String>(0) == "closed") {
        let reason: String = row.get(1);
        return Ok(Some(format!(
            "restore.site_busy: the maintenance gate of the site is closed ({reason}); another operation holds it"
        )));
    }
    let site_id =
        SiteId::new(site).map_err(|_| "the site name is not a valid site id".to_owned())?;
    let entries = PgApplyJournal::new(backend.pool(), site_id).read().await.map_err(|_| "restore.site_busy: the migration journal cannot be read, so a running migration cannot be ruled out".to_owned())?;
    let progress = JournalProgress::fold(entries);
    if progress.started && progress.gate_open.is_none() {
        return Ok(Some("restore.site_busy: a migration of this site is in progress or was left unfinished; finish or restore it first".to_owned()));
    }
    Ok(None)
}

/// Opens the gate of the live database again and lets connections in; used by every way back.
pub(super) async fn reopen(
    cfg: &PgConfig,
    database: &str,
    site: &str,
    clock: &dyn Clock,
) -> Result<(), String> {
    admin_exec(
        cfg,
        &format!("ALTER DATABASE {} ALLOW_CONNECTIONS true", ident(database)?),
    )
    .await?;
    let backend = on_db(cfg, database).await?;
    PgApplySchema::new(backend.pool())
        .prepare()
        .await
        .map_err(|_| "the control schema could not be prepared".to_owned())?;
    let site_id =
        SiteId::new(site).map_err(|_| "the site name is not a valid site id".to_owned())?;
    let gate = PgAdmissionGate::new(
        backend.pool(),
        site_id,
        Arc::new(FixedMicros(micros(clock))),
        Duration::from_secs(1),
    );
    gate.open(micros(clock))
        .await
        .map_err(|_| "the maintenance gate could not be opened".to_owned())
}

/// A clock frozen at one instant, for the gate's constructor (it only needs `now`).
pub(super) struct FixedMicros(i64);
impl Clock for FixedMicros {
    fn now_utc(&self) -> time::OffsetDateTime {
        time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(self.0) * 1000)
            .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
    }
    fn monotonic(&self) -> Duration {
        Duration::ZERO
    }
}

/// Closes the gate row, tells the host, drains in-flight work, bars new connections and ends the
/// rest. `Ok` means the site is proven quiescent.
pub(super) async fn quiesce(ctx: &mut CommandCtx, prep: &Prepared) -> Result<(), String> {
    let (db, site) = (prep.site.database.clone(), prep.site.site.clone());
    let backend = on_db(&prep.cfg, &db).await?;
    PgApplySchema::new(backend.pool())
        .prepare()
        .await
        .map_err(|_| "the control schema could not be prepared".to_owned())?;
    let site_id =
        SiteId::new(&site).map_err(|_| "the site name is not a valid site id".to_owned())?;
    let gate = PgAdmissionGate::new(
        backend.pool(),
        site_id,
        ctx.clock().clone(),
        Duration::from_secs(1),
    );
    let now = micros(ctx.clock().as_ref());
    gate.close("restore", now)
        .await
        .map_err(|_| "the maintenance gate could not be closed".to_owned())?;
    if let Ok(GateStatus::Open) = gate.refresh(now).await {
        return Err("the maintenance gate did not close".to_owned());
    }
    say(ctx, "maintenance gate closed");
    if spot_cli::request_reload(ctx.root(), &site).is_err() {
        say(
            ctx,
            "the host could not be told to reload (no running host is assumed)",
        );
    }
    let drained = gate
        .drain(DRAIN_MICROS, now)
        .await
        .map_err(|_| "in-flight work could not be observed".to_owned())?;
    if drained.timed_out {
        return Err("work was still in flight when the drain deadline passed".to_owned());
    }
    drop(gate);
    drop(backend);
    admin_exec(
        &prep.cfg,
        &format!("ALTER DATABASE {} ALLOW_CONNECTIONS false", ident(&db)?),
    )
    .await?;
    let sessions = "SELECT count(*) FROM (SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()) t";
    let count =
        "SELECT count(*) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()";
    let control = admin(&prep.cfg).await?;
    let client = control.pool().get().await.map_err(pool_error)?;
    for _ in 0..25 {
        client.query(sessions, &[&db]).await.map_err(pool_error)?;
        let left: i64 = client
            .query_one(count, &[&db])
            .await
            .map_err(pool_error)?
            .get(0);
        if left == 0 {
            say(
                ctx,
                "no other session holds the database; the site is quiescent",
            );
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Err(
        "sessions of the host still hold the database, so the site cannot be proven quiescent"
            .to_owned(),
    )
}

/// `<db>_replaced_<seconds>`, within the 63 bytes of an identifier.
pub(super) fn quarantine_name(database: &str, secs: i64) -> String {
    let head: String = database.chars().take(40).collect();
    format!("{head}_replaced_{secs}")
}

pub(super) fn quarantine_marker(site: &str, secs: i64) -> ScratchMarker {
    ScratchMarker::for_snapshot_id(&format!("quarantine:{site}:{secs}"))
}

/// One atomic swap: the live database becomes the quarantine, the scratch becomes the live one.
/// Returns the quarantine's name when there was a live database.
pub(super) async fn swap(
    cfg: &PgConfig,
    live: &str,
    scratch: &str,
    site: &str,
    secs: i64,
    replace: bool,
) -> Result<Option<String>, String> {
    let (live_q, scratch_q) = (ident(live)?, ident(scratch)?);
    let backend = admin(cfg).await?;
    let client = backend.pool().get().await.map_err(pool_error)?;
    client
        .execute("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()", &[&scratch])
        .await
        .map_err(pool_error)?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut sql = String::from("BEGIN;");
    let mut kept = None;
    if replace {
        let name = quarantine_name(live, secs);
        let q = ident(&name)?;
        let marker = quarantine_marker(site, secs);
        sql += &format!("ALTER DATABASE {live_q} RENAME TO {q};");
        sql += &format!("ALTER DATABASE {scratch_q} RENAME TO {live_q};");
        sql += &format!("COMMENT ON DATABASE {q} IS '{}';", marker.as_str());
        sql += &format!("ALTER DATABASE {q} ALLOW_CONNECTIONS true;");
        kept = Some(name);
    } else {
        sql += &format!("ALTER DATABASE {scratch_q} RENAME TO {live_q};");
    }
    sql += &format!(
        "COMMENT ON DATABASE {live_q} IS NULL; ALTER DATABASE {live_q} ALLOW_CONNECTIONS true; COMMIT;"
    );
    client.batch_execute(&sql).await.map_err(|e| {
        format!(
            "the swap was refused: {}",
            e.as_db_error().map_or("an error", |d| d.message())
        )
    })?;
    Ok(kept)
}

/// Puts the previous database back after a swap whose follow-up failed; the restored copy is kept
/// aside as `<db>_restored_<seconds>`.
pub(super) async fn unswap(
    cfg: &PgConfig,
    live: &str,
    kept: Option<&str>,
    secs: i64,
) -> Result<(), String> {
    let aside = ident(&format!(
        "{}_restored_{secs}",
        live.chars().take(40).collect::<String>()
    ))?;
    let live_q = ident(live)?;
    let mut sql = format!("BEGIN; ALTER DATABASE {live_q} RENAME TO {aside};");
    if let Some(kept) = kept {
        sql += &format!(
            "ALTER DATABASE {} RENAME TO {live_q}; COMMENT ON DATABASE {live_q} IS NULL;",
            ident(kept)?
        );
    }
    admin_exec(cfg, &(sql + "COMMIT;")).await
}

/// Recovers from a restore that died: a lock whose process is gone means the live database may
/// still bar connections and the gate row may still be closed. Reopens both and clears the lock.
pub(super) async fn recover_stale(ctx: &mut CommandCtx, prep: &Prepared) -> Result<(), String> {
    let dir = state_dir(ctx.root(), &prep.site.site)
        .map_err(|_| "the site's state directory cannot be used".to_owned())?;
    match StateLock::acquire(&dir) {
        Ok(lock) => {
            drop(lock);
            Ok(())
        }
        Err(LockState::Busy) => Ok(()),
        Err(LockState::Failed) => Err("the restore lock cannot be created".to_owned()),
        Err(LockState::Stale) => {
            say(
                ctx,
                "a previous restore did not finish; reopening the site first",
            );
            reopen(
                &prep.cfg,
                &prep.site.database,
                &prep.site.site,
                ctx.clock().as_ref(),
            )
            .await?;
            let dead = std::fs::read_to_string(dir.join("restore.lock"))
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok());
            release_dead_ledger_lock(&prep.dir, dead);
            std::fs::remove_file(dir.join("restore.lock"))
                .map_err(|_| "the stale restore lock cannot be removed".to_owned())
        }
    }
}

/// A restore that died also left the backup target's `ledger.lock` it took, and nothing else
/// would ever remove it, so every later backup, prune and restore of the target would be
/// refused. The lock is removed only when it names the dead restore's own process id, so a lock
/// of a live backup run (or of an owner this restore does not know) stays.
fn release_dead_ledger_lock(target: &std::path::Path, dead: Option<u32>) {
    let Some(dead) = dead else {
        return;
    };
    let path = target.join(spot_backup::ledger_file::LOCK_FILE);
    let owner = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok());
    if owner == Some(dead) {
        let _ = std::fs::remove_file(&path);
    }
}
