//! The folder target: where the journal's objects are written. Checked once at start.

use std::path::{Path, PathBuf};

use crate::error::ShipFault;

/// Canonicalises `target` and refuses a path that is unsafe for the journal.
///
/// The path must exist, be a directory the process can write, and must not be inside (or equal
/// to) any of `forbidden`: the site's own data directory and the application's code directory.
/// Keeping the journal inside the data it protects would let one disk failure take both.
///
/// # Errors
/// [`ShipFault::TargetUnsafe`] with a fixed sentence (never the path).
pub fn check_folder_target(target: &Path, forbidden: &[PathBuf]) -> Result<PathBuf, ShipFault> {
    let canonical = std::fs::canonicalize(target)
        .map_err(|_| ShipFault::TargetUnsafe("the target folder does not exist"))?;
    let meta = std::fs::metadata(&canonical)
        .map_err(|_| ShipFault::TargetUnsafe("the target folder cannot be read"))?;
    if !meta.is_dir() {
        return Err(ShipFault::TargetUnsafe("the target is not a directory"));
    }
    if meta.permissions().readonly() {
        return Err(ShipFault::TargetUnsafe("the target folder is read-only"));
    }
    for other in forbidden {
        // A forbidden directory that does not exist cannot contain the target.
        if let Ok(other) = std::fs::canonicalize(other)
            && canonical.starts_with(&other)
        {
            return Err(ShipFault::TargetUnsafe(
                "the target is inside the site's data or code directory",
            ));
        }
    }
    Ok(canonical)
}
