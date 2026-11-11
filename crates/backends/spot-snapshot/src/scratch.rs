//! Dropping what a restore created (WP-MIG-05.T2): scratch databases and scratch directories.
//!
//! This crate never drops live data (`restore` renames a populated target aside). A rehearsal
//! restores a snapshot into a scratch and must destroy it again, so the two drops live here, each
//! behind a guard that cannot be talked past:
//!
//! - [`drop_scratch`] drops a database only if it carries the [`ScratchMarker`] the restore wrote
//!   into the database comment, is not the configured live database, and is not a system
//!   database. Its sessions are terminated first. A database that is already gone is success.
//! - [`drop_scratch_dir`] removes a data directory only if it lies strictly under the declared
//!   scratch root and no component below the root is a symlink. A directory already gone is
//!   success.
//!
//! The marker is `spot-scratch:v1:<snapshot id>`. A restore writes it when it creates a fresh
//! database; a restore with `replace` takes over a live name and is never marked. Anything
//! refused is `snapshot.not_scratch`, whatever the caller says.

use std::path::{Component, Path};

use crate::conn::{connect_url, quote_ident, split_connection};
use crate::error::{SnapshotError, SnapshotResult};
use crate::manifest::Snapshot;
use crate::pgdump::admin_url_of;
use crate::provider::RestoreTarget;

/// The marker's fixed prefix.
pub const MARKER_PREFIX: &str = "spot-scratch:v1:";

/// Databases that are never scratch, marker or not.
const SYSTEM_DATABASES: [&str; 3] = ["postgres", "template0", "template1"];

/// The marker a restore writes into a scratch database, and a drop must present.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchMarker(String);

impl ScratchMarker {
    /// The marker of the scratch restored from `snapshot`.
    pub fn for_snapshot(snapshot: &Snapshot) -> Self {
        Self::for_snapshot_id(&snapshot.manifest().id)
    }

    /// The marker of the scratch restored from the snapshot with this id.
    pub fn for_snapshot_id(id: &str) -> Self {
        Self(format!("{MARKER_PREFIX}{id}"))
    }

    /// The marker text, as stored in the database comment.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The comment literal. The marker is a prefix plus a snapshot id, but a quote is doubled
    /// anyway so a hand-built id cannot break out of the literal.
    fn literal(&self) -> String {
        format!("'{}'", self.0.replace('\'', "''"))
    }
}

/// What a drop did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropOutcome {
    /// The scratch existed and is gone.
    Dropped,
    /// There was nothing to drop (an earlier drop already did it).
    NotFound,
}

/// Writes `marker` into the database's comment. Called by the restore right after it creates the
/// database, so a restore that fails halfway still leaves a marked, droppable scratch.
///
/// # Errors
/// `snapshot.io` when the database refuses the comment.
pub(crate) async fn mark_scratch(
    connection: &str,
    database: &str,
    marker: &ScratchMarker,
) -> SnapshotResult<()> {
    let held = connect_url(&admin_url_of(connection)).await?;
    let quoted = quote_ident(database)?;
    held.client
        .batch_execute(&format!(
            "COMMENT ON DATABASE {quoted} IS {}",
            marker.literal()
        ))
        .await
        .map_err(crate::pgdump::db_err)?;
    Ok(())
}

fn refuse(target: &str, reason: impl Into<String>) -> SnapshotError {
    SnapshotError::NotScratch {
        target: target.to_owned(),
        reason: reason.into(),
    }
}

/// Drops the scratch database `target` names, if and only if it carries `expect_marker`.
///
/// `conn` is a connection string to the server whose own database is the **live** one: the
/// database it names is never dropped, even if it carries the marker. The maintenance database
/// `postgres` of the same server runs the drop, with `conn`'s credentials. Sessions on the
/// scratch are terminated first. Dropping a database that does not exist is success.
///
/// # Errors
/// `snapshot.not_scratch` for a database that is the live one, a system database, unmarked or
/// marked differently; `snapshot.io` for a target that is not a database or a server failure.
pub async fn drop_scratch(
    conn: &str,
    target: &RestoreTarget,
    expect_marker: &ScratchMarker,
) -> SnapshotResult<DropOutcome> {
    let RestoreTarget::Database { database, .. } = target else {
        return Err(SnapshotError::io(
            "drop_scratch drops databases; use drop_scratch_dir for an instance",
        ));
    };
    let live = split_connection(conn)?.database;
    if *database == live {
        return Err(refuse(database, "it is the configured live database"));
    }
    if SYSTEM_DATABASES.contains(&database.as_str()) {
        return Err(refuse(database, "it is a system database"));
    }
    let quoted = quote_ident(database)?;
    let held = connect_url(&admin_url_of(conn)).await?;
    let row = held
        .client
        .query_opt(
            "SELECT shobj_description(oid, 'pg_database') FROM pg_database WHERE datname = $1",
            &[database],
        )
        .await
        .map_err(crate::pgdump::db_err)?;
    let Some(row) = row else {
        return Ok(DropOutcome::NotFound);
    };
    let comment: Option<String> = row.get(0);
    match comment {
        Some(found) if found == expect_marker.as_str() => {}
        Some(_) => {
            return Err(refuse(
                database,
                "it carries another scratch marker or comment",
            ));
        }
        None => return Err(refuse(database, "it carries no scratch marker")),
    }
    held.client
        .execute(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
             WHERE datname = $1 AND pid <> pg_backend_pid()",
            &[database],
        )
        .await
        .map_err(crate::pgdump::db_err)?;
    // FORCE closes a session that connected between the terminate and the drop.
    held.client
        .batch_execute(&format!("DROP DATABASE IF EXISTS {quoted} WITH (FORCE)"))
        .await
        .map_err(crate::pgdump::db_err)?;
    Ok(DropOutcome::Dropped)
}

/// Removes the scratch data directory of an instance `target`, if and only if it lies strictly
/// under `scratch_root` and no component below the root is a symlink (so nothing outside the
/// root is ever reached through a link). A directory that does not exist is success.
///
/// # Errors
/// `snapshot.not_scratch` for a path outside the root, the root itself, a relative path, one
/// with `..`, or one that crosses a symlink; `snapshot.io` for a target that is not an instance
/// or a failure while removing.
pub async fn drop_scratch_dir(
    scratch_root: &Path,
    target: &RestoreTarget,
) -> SnapshotResult<DropOutcome> {
    let RestoreTarget::Instance { layout, .. } = target else {
        return Err(SnapshotError::io(
            "drop_scratch_dir removes instance directories; use drop_scratch for a database",
        ));
    };
    let dir = layout.data_dir.as_path();
    let shown = dir.display().to_string();
    if !scratch_root.is_absolute() || !dir.is_absolute() {
        return Err(refuse(
            &shown,
            "the scratch root and the directory must be absolute",
        ));
    }
    if dir.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(refuse(&shown, "it contains a `..` component"));
    }
    let Ok(below) = dir.strip_prefix(scratch_root) else {
        return Err(refuse(&shown, "it lies outside the declared scratch root"));
    };
    if below.as_os_str().is_empty() {
        return Err(refuse(&shown, "it is the scratch root itself"));
    }
    // Every component below the root, the directory included, must be a real directory entry.
    let mut walked = scratch_root.to_path_buf();
    for component in below.components() {
        walked.push(component);
        match tokio::fs::symlink_metadata(&walked).await {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(refuse(
                    &shown,
                    "it crosses a symlink below the scratch root",
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DropOutcome::NotFound);
            }
            Err(e) => {
                return Err(SnapshotError::io(format!(
                    "cannot inspect {}: {e}",
                    walked.display()
                )));
            }
        }
    }
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => Ok(DropOutcome::Dropped),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(DropOutcome::NotFound),
        Err(e) => Err(SnapshotError::io(format!("cannot remove {shown}: {e}"))),
    }
}
