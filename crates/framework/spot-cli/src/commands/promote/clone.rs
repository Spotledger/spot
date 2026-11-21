//! The rehearsal on a clone of the **target**: `pg_dump` the target site into a scratch database
//! (never the live name), apply the approved bundle to the clone's customisation records, check
//! that a second plan of the bundle against the clone is empty, and measure; the scratch is
//! destroyed on every exit path. `abort` drops what a killed rehearsal left behind.
//!
//! The engine, the snapshot provider and the scratch reaper are the ones `spot migrate-rehearse`
//! uses; the only new part is the applier, which applies a *bundle* where the migration
//! rehearsal applies schema steps.

use std::path::PathBuf;
use std::sync::Arc;

use spot_core::{Clock, SiteId};
use spot_custom_bundle::{ApplyPlan, Bundle, SectionRegistry, apply, plan_apply};
use spot_customise::{RecordStore, StorageRecordStore};
use spot_environments::EnvRef;
use spot_migration_plan::ReleasePlan;
use spot_postgres::{PgBackend, PgConfig, Tls};
use spot_promotion::{PromotionError, RehearsalRequest, RehearsalRunner, TargetSite};
use spot_rehearsal::{
    Applier, ApplyOutcome, BoxFuture, CloneRequest, CloneTarget, FingerprintSpec, Rehearsal,
    RehearsalReport, SnapshotScratch, StorageFingerprintSource,
};
use spot_snapshot::{
    Destination, PgDumpProvider, RestoreTarget, ScratchMarker, SnapshotTarget, drop_scratch,
    scratch::MARKER_PREFIX,
};
use spot_storage_contract::Catalog;

use super::super::migrate_rehearse::site_scratch_prefix;
use super::target::{Parts, Publish, load_target};

/// The promotion's part of a scratch database name: lower-case identifier characters, at most
/// fourteen, so the whole name stays within PostgreSQL's 63 bytes.
fn id_part(id: &str) -> String {
    id.chars()
        .take(14)
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect()
}

/// The prefix of every scratch database of this promotion's rehearsals on `site`.
#[must_use]
pub fn promotion_prefix(site: &str, id: &str) -> String {
    format!("{}p{}_", site_scratch_prefix(site), id_part(id))
}

/// The scratch database name: the promotion's prefix and the time in seconds; never a live name.
#[must_use]
pub fn scratch_name(site: &str, id: &str, now_secs: i64) -> String {
    format!("{}{now_secs}", promotion_prefix(site, id))
}

fn connection(cfg: &PgConfig, database: &str) -> Option<String> {
    let mut url = url::Url::parse("postgres://placeholder/").ok()?;
    url.set_host(Some(&cfg.host)).ok()?;
    url.set_port(Some(cfg.port)).ok()?;
    url.set_username(&cfg.user).ok()?;
    if !cfg.password.is_empty() {
        url.set_password(Some(&cfg.password)).ok()?;
    }
    url.path_segments_mut().ok()?.clear().push(database);
    if matches!(cfg.tls, Tls::Require) {
        url.query_pairs_mut().append_pair("sslmode", "require");
    }
    Some(url.into())
}

/// The directory holding `pg_dump`: `SPOT_PG_BIN` (absolute) or the first one on `PATH`.
#[must_use]
pub fn pg_bin() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SPOT_PG_BIN").map(PathBuf::from) {
        return dir.is_absolute().then_some(dir);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|dir| dir.join("pg_dump").is_file() && dir.is_absolute())
}

fn work_dir() -> PathBuf {
    std::env::var_os("SPOT_REHEARSAL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
}

/// Applies the bundle to the clone's records and proves a second plan is empty.
struct ClonePromoter {
    plan: ApplyPlan,
    bundle: Bundle,
    parts: Parts,
    catalog: Arc<Catalog>,
    env: EnvRef,
    actor: String,
}

impl Applier for ClonePromoter {
    fn apply<'a>(
        &'a self,
        plan: &'a ReleasePlan,
        clone: &'a CloneTarget,
    ) -> BoxFuture<'a, spot_rehearsal::Result<ApplyOutcome>> {
        Box::pin(async move {
            let fail = |reason: String| spot_rehearsal::Error::ApplyFailed { reason };
            let RestoreTarget::Database {
                connection,
                database,
            } = clone.restore()
            else {
                return Err(fail(
                    "the promotion rehearsal needs a database clone".to_owned(),
                ));
            };
            let config = PgConfig::from_url(connection)
                .map(|config| config.with_dbname(database))
                .map_err(|e| fail(e.to_string()))?;
            let backend = Arc::new(
                PgBackend::connect(config)
                    .await
                    .map_err(|e| fail(e.to_string()))?,
            );
            let mut target = load_target(
                &self.parts,
                &backend,
                &self.catalog,
                self.env.clone(),
                &self.actor,
                Publish::Deferred,
            )
            .await
            .map_err(fail)?;
            let registry = SectionRegistry::new();
            let report = apply(&self.plan, target.site_mut(), &registry)
                .map_err(|e| fail(format!("{}: {}", e.code(), e.item())))?;
            let ops = target.site_mut().take_committed();
            let store =
                StorageRecordStore::new(Arc::new((*backend).clone()), Arc::clone(&self.catalog));
            store
                .commit(ops)
                .await
                .map_err(|_| fail("the clone's customisation records were not written".into()))?;
            let again = load_target(
                &self.parts,
                &backend,
                &self.catalog,
                self.env.clone(),
                &self.actor,
                Publish::Deferred,
            )
            .await
            .map_err(fail)?;
            let remaining = plan_apply(&self.bundle, TargetSite::as_reader(&again), &registry)
                .map_err(|errors| {
                    fail(format!(
                        "the bundle cannot be planned against the clone ({})",
                        errors.first().map_or("unknown", |e| e.code())
                    ))
                })?;
            if !remaining.is_empty() {
                return Err(fail(format!(
                    "{} change(s) remain after the apply on the clone",
                    remaining.changes().len()
                )));
            }
            Ok(ApplyOutcome {
                executed: report
                    .applied()
                    .iter()
                    .map(|change| format!("{}/{}", change.section, change.key))
                    .collect(),
                plan_hash: self.plan.hash().to_owned(),
                plan_digest: plan.plan_digest.clone(),
                backfilled_rows: 0,
            })
        })
    }
}

/// The rehearsal runner of one promotion: a clone of the target site.
pub struct CloneRunner {
    /// The runtime the engine's database work runs on.
    pub handle: tokio::runtime::Handle,
    /// The target site's name.
    pub site: String,
    /// The target site's backend.
    pub backend: PgBackend,
    /// The target's catalogue (for the fingerprint source).
    pub catalog: Arc<Catalog>,
    /// What the target's image is built from.
    pub parts: Parts,
    /// The bundle being promoted.
    pub bundle: Bundle,
    /// The target's address.
    pub env: EnvRef,
    /// The clock (a scratch name carries the time).
    pub clock: Arc<dyn Clock>,
    /// The directory holding `pg_dump`.
    pub bin: PathBuf,
    /// The promotion's id.
    pub promotion: String,
    /// The acting identity (recorded on the clone's records).
    pub actor: String,
}

fn not_run() -> PromotionError {
    PromotionError::RehearsalFailed {
        verdict: "not_run".to_owned(),
        findings: 0,
    }
}

impl RehearsalRunner for CloneRunner {
    fn rehearse(&self, request: &RehearsalRequest<'_>) -> Result<RehearsalReport, PromotionError> {
        let cfg = self.backend.config().clone();
        let now = self.clock.now_utc().unix_timestamp();
        let scratch = scratch_name(&self.site, &self.promotion, now);
        let (Some(live), Some(scratch_conn)) =
            (connection(&cfg, &cfg.dbname), connection(&cfg, &scratch))
        else {
            tracing::warn!(site = %self.site, "promotion rehearsal: no connection URL");
            return Err(not_run());
        };
        let Ok(site_id) = SiteId::new(&self.site) else {
            return Err(not_run());
        };
        let target = RestoreTarget::Database {
            connection: scratch_conn,
            database: scratch.clone(),
        };
        let mut clone_request = CloneRequest::new(
            SnapshotTarget::Database {
                connection: live.clone(),
                tenant: self.site.clone(),
                database: cfg.dbname.clone(),
            },
            Destination {
                dir: work_dir().join(format!("{scratch}-snapshot")),
            },
            target.clone(),
        );
        clone_request.reaper = Some(Arc::new(SnapshotScratch::new(live, target)));
        let provider = Arc::new(PgDumpProvider::new(
            self.bin.clone(),
            Arc::clone(&self.clock),
        ));
        // A runner always receives a plan with its apply plan (`plan.rs`): none is not run.
        let Some(apply_plan) = request.plan.apply_plan().cloned() else {
            return Err(not_run());
        };
        let applier = Arc::new(ClonePromoter {
            plan: apply_plan,
            bundle: self.bundle.clone(),
            parts: self.parts.clone(),
            catalog: Arc::clone(&self.catalog),
            env: self.env.clone(),
            actor: self.actor.clone(),
        });
        // No fingerprint source is registered yet (the ERP registers its queries): the spec is
        // empty, the measurement is a recorded pass and the source is never asked anything.
        let fingerprint = Arc::new(StorageFingerprintSource::new(
            Arc::new(self.backend.clone()),
            Arc::clone(&self.catalog),
        ));
        let engine = Rehearsal::new(
            provider,
            applier,
            fingerprint,
            FingerprintSpec::new(Vec::new()),
            clone_request,
        );
        self.handle
            .block_on(engine.run(request.plan.release(), request.ticket, &site_id))
            .map_err(|error| {
                tracing::warn!(site = %self.site, error = %error, "promotion rehearsal failed");
                not_run()
            })
    }
}

/// Drops the scratch databases this promotion's rehearsals left behind on `site` (only those
/// that carry the scratch marker; never the live database). Returns their names.
///
/// # Errors
/// A message when the databases cannot be listed or the connection cannot be built.
pub async fn drop_leftovers(
    backend: &PgBackend,
    site: &str,
    promotion: &str,
) -> Result<Vec<String>, String> {
    let cfg = backend.config().clone();
    let live = connection(&cfg, &cfg.dbname)
        .ok_or_else(|| "the connection URL cannot be built".to_owned())?;
    let like = format!("{}%", promotion_prefix(site, promotion).replace('_', "\\_"));
    let client = backend
        .pool()
        .get()
        .await
        .map_err(|_| "the site's database is not reachable".to_owned())?;
    let rows = client
        .query(
            "SELECT datname, shobj_description(oid, 'pg_database') FROM pg_database \
             WHERE datname LIKE $1 AND datname <> current_database()",
            &[&like],
        )
        .await
        .map_err(|_| "the scratch databases cannot be listed".to_owned())?;
    let mut dropped = Vec::new();
    for row in rows {
        let name: String = row.get(0);
        let comment: Option<String> = row.get(1);
        let Some(id) = comment
            .as_deref()
            .and_then(|c| c.strip_prefix(MARKER_PREFIX))
        else {
            continue;
        };
        let target = RestoreTarget::Database {
            connection: live.clone(),
            database: name.clone(),
        };
        if drop_scratch(&live, &target, &ScratchMarker::for_snapshot_id(id))
            .await
            .is_ok()
        {
            dropped.push(name);
        }
    }
    Ok(dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scratch_name_is_never_a_live_name_and_fits_postgres() {
        let name = scratch_name(
            "a-very-long-site-name",
            "promo-0123456789abcdef-1700000000",
            1_700_000_000,
        );
        assert!(name.starts_with("spot_rehearsal_"));
        assert!(name.len() <= 63, "{name}");
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        );
    }

    #[test]
    fn the_prefix_names_the_site_and_the_promotion() {
        let prefix = promotion_prefix("test", "promo-1");
        assert!(prefix.starts_with(&site_scratch_prefix("test")));
        assert!(scratch_name("test", "promo-1", 5).starts_with(&prefix));
        assert!(!scratch_name("test", "promo-2", 5).starts_with(&prefix));
    }
}
