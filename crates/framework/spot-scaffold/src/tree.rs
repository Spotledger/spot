//! The virtual file tree: every generator writes into it, and [`FileTree::write_to_disk`] is
//! the only I/O the crate performs.
//!
//! The tree is ordered (`BTreeMap`), so the same arguments always produce the same bytes and
//! the same listing. Writing refuses, before anything is created:
//!
//! - an absolute path, a `..` segment, a backslash or a colon (Windows-absolute), and any
//!   name that would leave the output root — the same rules the bundle uses
//!   ([`spot_pack::pathsafe::is_safe_rel_path`]);
//! - a symlink anywhere on the path it would write through;
//! - overwriting an existing file unless `--force` is given;
//! - a write whose parent does not canonicalise inside the root.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use crate::error::{Result, ScaffoldError};

/// A validated, `/`-separated, app-relative path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RelPath(String);

impl RelPath {
    /// Validates a relative path.
    ///
    /// # Errors
    /// `scaffold.path_escape` when the path is absolute, holds `..`, `.`, an empty segment, a
    /// backslash or a colon, or is empty.
    pub fn parse(raw: &str) -> Result<Self> {
        if !spot_pack::pathsafe::is_safe_rel_path(raw) {
            return Err(ScaffoldError::path_escape(format!(
                "the path `{raw}` would escape the output root: use a plain relative path such \
                 as `doctypes/note.toml`"
            )));
        }
        Ok(Self(raw.to_owned()))
    }

    /// The path as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for RelPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What a write did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriteReport {
    /// The files written, in path order.
    pub files: Vec<String>,
    /// The total bytes written.
    pub bytes: usize,
}

/// Whether an existing file may be replaced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WritePolicy {
    /// Replace an existing file (`--force`); otherwise an existing file is refused.
    pub force: bool,
}

impl WritePolicy {
    /// A policy that refuses to overwrite.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A policy that overwrites.
    #[must_use]
    pub fn force() -> Self {
        Self { force: true }
    }
}

/// The ordered virtual tree of files a generator produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileTree {
    files: BTreeMap<RelPath, Vec<u8>>,
}

impl FileTree {
    /// An empty tree.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one file. The path is validated; a second insert of the same path is an error
    /// (a generator bug, never something an author did).
    ///
    /// # Errors
    /// `scaffold.path_escape` for an unsafe path, `scaffold.duplicate` when the path is
    /// already in the tree.
    pub fn insert(&mut self, path: &str, bytes: impl Into<Vec<u8>>) -> Result<()> {
        let rel = RelPath::parse(path)?;
        if self.files.contains_key(&rel) {
            return Err(ScaffoldError::duplicate(format!(
                "the scaffold produced `{path}` twice"
            )));
        }
        self.files.insert(rel, bytes.into());
        Ok(())
    }

    /// Adds one file whose content is a rendered template.
    ///
    /// # Errors
    /// As [`FileTree::insert`].
    pub fn insert_text(&mut self, path: &str, text: &str) -> Result<()> {
        self.insert(path, text.as_bytes().to_vec())
    }

    /// The paths in the tree, in order.
    #[must_use]
    pub fn paths(&self) -> Vec<&str> {
        self.files.keys().map(RelPath::as_str).collect()
    }

    /// How many files the tree holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether the tree holds no files.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The total bytes in the tree.
    #[must_use]
    pub fn total_bytes(&self) -> usize {
        self.files.values().map(Vec::len).sum()
    }

    /// One file's bytes, when present.
    #[must_use]
    pub fn get(&self, path: &str) -> Option<&[u8]> {
        self.files.get(&RelPath(path.to_owned())).map(Vec::as_slice)
    }

    /// The human dry-run listing: one `path (N bytes)` line per file and a total line.
    #[must_use]
    pub fn dry_run(&self) -> String {
        let mut out = String::new();
        for (path, bytes) in &self.files {
            out.push_str(&format!("{} ({} bytes)\n", path.as_str(), bytes.len()));
        }
        out.push_str(&format!(
            "{} files, {} bytes total\n",
            self.files.len(),
            self.total_bytes()
        ));
        out
    }

    /// The `--json` form of the tree: the paths, in order, and the byte count.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "files": self.paths(),
            "bytes": self.total_bytes(),
        })
    }

    /// Writes every file under `root`, which is the only I/O this crate does.
    ///
    /// # Errors
    /// `scaffold.io` for a failed read, create or write; `scaffold.path_escape` when a parent
    /// canonicalises outside `root`; `scaffold.symlink` when a symlink sits on the path;
    /// `scaffold.exists` when a file exists and `policy.force` is false.
    pub fn write_to_disk(&self, root: &Path, policy: &WritePolicy) -> Result<WriteReport> {
        std::fs::create_dir_all(root).map_err(|e| {
            ScaffoldError::io(format!(
                "the output directory {} could not be created: {e}",
                root.display()
            ))
        })?;
        let canonical_root = std::fs::canonicalize(root).map_err(|e| {
            ScaffoldError::io(format!(
                "the output directory {} could not be resolved: {e}",
                root.display()
            ))
        })?;

        // Phase 1: create the directories and validate every target, so a refusal leaves no
        // file half-written. Only directories are created here; no file is written until the
        // whole set has passed.
        let mut targets = Vec::with_capacity(self.files.len());
        for (rel, bytes) in &self.files {
            let target = canonical_root.join(rel.as_str());
            refuse_symlinks(&canonical_root, rel.as_str())?;
            let parent = target.parent().unwrap_or(&canonical_root);
            std::fs::create_dir_all(parent).map_err(|e| {
                ScaffoldError::io(format!(
                    "the directory for `{}` could not be created: {e}",
                    rel.as_str()
                ))
            })?;
            ensure_inside(&canonical_root, parent, rel.as_str())?;
            if let Ok(meta) = std::fs::symlink_metadata(&target) {
                if meta.file_type().is_symlink() {
                    return Err(ScaffoldError::symlink(format!(
                        "`{}` is a symlink; a scaffold never writes through one",
                        rel.as_str()
                    )));
                }
                if !policy.force {
                    return Err(ScaffoldError::exists(format!(
                        "`{}` already exists; pass --force to replace it",
                        rel.as_str()
                    )));
                }
            }
            targets.push((rel.as_str(), target, bytes));
        }

        // Phase 2: write every file.
        let mut report = WriteReport::default();
        for (rel, target, bytes) in targets {
            std::fs::write(&target, bytes)
                .map_err(|e| ScaffoldError::io(format!("`{rel}` could not be written: {e}")))?;
            report.files.push(rel.to_owned());
            report.bytes += bytes.len();
        }
        Ok(report)
    }
}

/// Refuses a symlink on any component of `rel` under `root` (the root itself is resolved
/// already, so it is not checked).
fn refuse_symlinks(root: &Path, rel: &str) -> Result<()> {
    let mut here = root.to_path_buf();
    for part in rel.split('/') {
        here.push(part);
        if std::fs::symlink_metadata(&here).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(ScaffoldError::symlink(format!(
                "`{rel}` writes through the symlink {}; a scaffold never writes through one",
                here.display()
            )));
        }
    }
    Ok(())
}

/// Checks that a canonicalised parent is inside the root.
fn ensure_inside(root: &Path, parent: &Path, rel: &str) -> Result<()> {
    let canonical = std::fs::canonicalize(parent).map_err(|e| {
        ScaffoldError::io(format!(
            "the directory for `{rel}` could not be resolved: {e}"
        ))
    })?;
    if !canonical.starts_with(root) {
        return Err(ScaffoldError::path_escape(format!(
            "`{rel}` resolves to {}, outside the output root {}",
            canonical.display(),
            root.display()
        )));
    }
    Ok(())
}

/// Whether `path` holds a `..` component (a cheap pre-check for a caller that has not
/// validated its input).
#[must_use]
pub fn has_parent_segment(path: &Path) -> bool {
    path.components().any(|c| matches!(c, Component::ParentDir))
}

/// Joins a validated relative path onto a root.
#[must_use]
pub fn join(root: &Path, rel: &str) -> PathBuf {
    root.join(rel)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn relative_paths_validate() {
        assert!(RelPath::parse("manifest.toml").is_ok());
        assert!(RelPath::parse("doctypes/note.toml").is_ok());
        for bad in ["/abs", "../x", "a/../b", "a//b", "a\\b", "a:b", ""] {
            assert!(RelPath::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn dry_run_lists_and_counts() {
        let mut tree = FileTree::new();
        tree.insert("a.txt", "hello").expect("safe path");
        assert!(tree.dry_run().contains("a.txt (5 bytes)"));
        assert!(tree.dry_run().contains("1 files, 5 bytes total"));
    }
}
