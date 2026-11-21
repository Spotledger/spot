//! Restores with invariant checks and the caller-wired `post_restore` step. A restore
//! that finishes but fails its checks is never silently kept: the caller moves the
//! restored data to the clearly named non-activated name this module generates
//! (`<name>.invariants-failed-<unix-ms>`) — the error carries the violations, nothing
//! is activated (ADR-068 rules 13 and 26: failure leaves consistent, labelled state,
//! and recovery paths are exercised).
//!
//! The outbox restart: after a restore the old feed generation's bookmark positions
//! are dead (the restore reproduced the snapshot's generation; continuing from an old
//! read position would skip or replay). [`post_restore`] starts a new generation
//! through `spot_postgres::start_new_generation`; bookmarks of the old generation then
//! fail with `ResyncRequired` and consumers rebuild from `FeedPosition::START`. The
//! step is **caller-wired**: this crate never opens a database on its own initiative —
//! the caller passes the backend it wants the step to run against.

use std::future::Future;
use std::pin::Pin;

use crate::error::{SnapshotError, SnapshotResult};
use crate::provider::{ProgressSink, SnapshotProvider};

/// One violation found by an invariant check against a restored site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The check that found it (its [`InvariantCheck::name`]).
    pub check: String,
    /// Plain-language statement of what does not match.
    pub detail: String,
}

/// A completed restore: the target the data landed in and the snapshot it came from.
/// Both providers return this; `restore_and_check` runs the invariant checks over the
/// restored site before the caller activates it.
#[derive(Debug, Clone)]
pub struct Restored {
    /// The snapshot that was restored.
    pub snapshot: crate::manifest::Snapshot,
    /// For a database restore the database name; for an instance restore the data
    /// directory's path.
    pub location: String,
    /// Whether the data is live at the target (a database restore into a fresh name
    /// is live at once; an instance switch is live after the atomic rename).
    pub activated: bool,
    /// The moved-aside copy's name when a populated target was replaced (a database
    /// name or a data directory path; never dropped by this crate).
    pub replaced_aside: Option<String>,
}

/// The clearly named, non-activated marker for a restore whose invariants failed:
/// `<target>.invariants-failed-<unix-ms>`. Nothing with this suffix is activated by
/// accident; it waits for a human decision.
pub fn invariants_failed_name(target: &str, at_unix_ms: u64) -> String {
    format!("{target}.invariants-failed-{at_unix_ms}")
}

/// One invariant check over a restored site. Shipped checks live in [`crate::facts`]
/// (they need a database connection the host wires); apps register their own — the
/// blueprint's L7: invariants hold after every run. Object-safe (boxed future); the
/// check inspects the restored snapshot's manifest (and whatever the host adds).
pub trait InvariantCheck: Send + Sync {
    /// The check's stable name, used in violations and error text.
    fn name(&self) -> &'static str;

    /// Runs the check against the restored site; every found problem becomes one
    /// [`Violation`]. An empty vector means the check passed.
    fn check(
        &self,
        restored: &crate::manifest::Snapshot,
    ) -> Pin<Box<dyn Future<Output = Vec<Violation>> + Send + '_>>;
}

/// Runs every registered check against `restored`; on failure, directory targets are
/// renamed aside to the marker name before the error is returned (the
/// `restore_and_check` guarantee: a failed restore is never left looking live).
///
/// # Errors
/// `snapshot.invariants_failed` listing the violations when any check fails; the
/// restored data is then quarantined (directory targets) or reported for the host to
/// quarantine (database targets — this crate never drops anything).
pub async fn run_checks(
    restored: &crate::manifest::Snapshot,
    target: &crate::provider::RestoreTarget,
    checks: &[Box<dyn InvariantCheck>],
    clock_ms: u64,
) -> SnapshotResult<()> {
    let mut violations: Vec<Violation> = Vec::new();
    for check in checks {
        violations.extend(check.check(restored).await);
    }
    if violations.is_empty() {
        return Ok(());
    }
    let listed = violations
        .iter()
        .map(|violation| format!("{}: {}", violation.check, violation.detail))
        .collect::<Vec<_>>()
        .join("; ");
    match target {
        crate::provider::RestoreTarget::Instance { layout, .. } => {
            let aside = quarantine_restored_dir(&layout.data_dir, clock_ms).await?;
            Err(SnapshotError::InvariantsFailed {
                violations: format!("{listed} (the failed data dir is marked {aside})"),
            })
        }
        crate::provider::RestoreTarget::Database { .. } => {
            Err(SnapshotError::InvariantsFailed { violations: listed })
        }
    }
}

/// The concrete `restore_and_check`: restores with `provider`, runs `checks`, and
/// quarantines a failed result (directory target renamed aside; database target
/// reported with its marker name for the host).
///
/// # Errors
/// The restore's own errors, then `snapshot.invariants_failed` with the violations.
#[allow(clippy::too_many_arguments)]
pub async fn restore_and_check(
    provider: &dyn SnapshotProvider,
    snapshot: &crate::manifest::Snapshot,
    target: &crate::provider::RestoreTarget,
    opts: &crate::provider::RestoreOptions,
    progress: std::sync::Arc<dyn ProgressSink>,
    checks: &[Box<dyn InvariantCheck>],
    clock_ms: u64,
) -> SnapshotResult<Restored> {
    opts.check_replace(target)?;
    let restored = provider.restore(snapshot, target, opts, progress).await?;
    run_checks(snapshot, target, checks, clock_ms).await?;
    Ok(restored)
}

/// Renames a restored instance's data directory aside to the non-activated marker.
///
/// # Errors
/// `snapshot.io` when the rename fails.
pub async fn quarantine_restored_dir(
    dir: &std::path::Path,
    clock_ms: u64,
) -> SnapshotResult<String> {
    let target = dir.display().to_string();
    let aside = invariants_failed_name(&target, clock_ms);
    tokio::fs::rename(dir, &aside).await.map_err(|e| {
        SnapshotError::io(format!(
            "cannot quarantine the failed restore {} as {aside}: {e}",
            dir.display()
        ))
    })?;
    Ok(aside)
}

/// The quarantine marker a database restore's host applies (this crate never drops a
/// database; the host decides between renaming it aside and dropping it).
pub fn quarantine_restored_database(database: &str, clock_ms: u64) -> String {
    invariants_failed_name(database, clock_ms)
}

/// Runs the caller-wired post-restore step: starts a new outbox generation on the
/// restored database (`spot_postgres::start_new_generation`) and returns the new
/// generation number. The caller wires this — never the providers — once the restored
/// database is up.
///
/// # Errors
/// `snapshot.io` wrapping the storage error's text; the caller can also call
/// `spot_postgres::start_new_generation` directly to match on its typed error.
pub async fn post_restore(backend: &spot_postgres::PgBackend) -> SnapshotResult<u64> {
    let generation = spot_postgres::start_new_generation(backend)
        .await
        .map_err(|e| {
            SnapshotError::io(format!("starting the new outbox generation failed: {e}"))
        })?;
    Ok(generation)
}

/// Resets the manifest's node-local state on the restored database through a raw
/// driver client the caller owns (in tests `TestDb::raw_client`; in production the
/// restored instance's pool): each marked key's `spot_meta` row (the node id first)
/// is deleted, so the next start picks a fresh one. Only the keys the manifest marks
/// are touched (the `system_counts.*` marks are snapshot facts, not node state, and
/// stay); running twice is safe (deleting a missing row deletes nothing).
///
/// # Errors
/// `snapshot.io` when a delete fails.
pub async fn reset_node_local(
    client: &tokio_postgres::Client,
    manifest: &crate::manifest::SnapshotManifest,
) -> SnapshotResult<()> {
    for key in manifest.node_local.keys() {
        if key.starts_with(crate::facts::SYSTEM_COUNTS_PREFIX) {
            continue;
        }
        client
            .execute("DELETE FROM spot_meta WHERE key = $1", &[&key])
            .await
            .map_err(|e| {
                SnapshotError::io(format!("resetting node-local key {key} failed: {e}"))
            })?;
    }
    Ok(())
}
