//! The local repository: the on-disk layout an imported bundle lands in, and its index.
//!
//! ```text
//! <repo>/index.toml                                  every imported bundle, sorted
//! <repo>/bundles/<kind>/<name>/<version>/...          one imported bundle's files
//! <repo>/staging/<kind>/<name>/<version>.partial/...  an import being written, never active
//! ```
//!
//! The staging directory is renamed into `bundles/` only after every file verified, so a
//! reader never sees a half-imported bundle as a whole one; the index is written atomically
//! after the rename. An existing staging directory with the same manifest digest is resumed
//! rather than restarted.

use std::path::{Path, PathBuf};

use semver::Version;
use serde::Deserialize;
use spot_updater::Digest;

use crate::error::AirgapError;
use crate::manifest::{BundleKind, OfflineFile};
use crate::storage::Storage;

/// The repository index file name.
pub const REPO_INDEX: &str = "index.toml";
/// The directory imported bundles live under.
pub const REPO_BUNDLES_DIR: &str = "bundles";
/// The directory partial imports live under.
pub const REPO_STAGING_DIR: &str = "staging";
/// The suffix of a staging directory that is never mistaken for a complete bundle.
pub const PARTIAL_SUFFIX: &str = ".partial";

/// One imported bundle, as the repository index records it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoEntry {
    /// What the bundle carries.
    pub kind: BundleKind,
    /// The bundle name.
    pub name: String,
    /// The bundle version.
    pub version: Version,
    /// The bundle digest the signature covered.
    pub digest: Digest,
    /// When it was imported (UTC seconds).
    pub imported_at: i64,
    /// The files it holds, sorted by path.
    pub files: Vec<OfflineFile>,
}

/// The repository index: every imported bundle.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoIndex {
    /// The entries, sorted by kind, then name, then version.
    pub entries: Vec<RepoEntry>,
}

/// The raw TOML shape of the index.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawIndex {
    #[serde(default)]
    entry: Vec<RawEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    kind: String,
    name: String,
    version: String,
    digest: String,
    imported_at: i64,
    #[serde(default)]
    files: Vec<RawFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    path: String,
    size: u64,
    sha256: String,
}

impl RepoIndex {
    /// Parses an `index.toml`, checking every field.
    ///
    /// # Errors
    /// [`AirgapError::RepoIndexMalformed`] when the TOML does not parse, an unknown field is
    /// present, a version is not semver, a kind is unknown, or a digest is malformed.
    pub fn from_toml(text: &str) -> Result<Self, AirgapError> {
        let raw: RawIndex =
            toml::from_str(text).map_err(|error| AirgapError::RepoIndexMalformed {
                detail: error.to_string(),
            })?;
        let mut entries = raw
            .entry
            .into_iter()
            .map(|entry| {
                let version = Version::parse(&entry.version).map_err(|error| {
                    AirgapError::RepoIndexMalformed {
                        detail: format!("`{}` is not a semver version: {error}", entry.version),
                    }
                })?;
                let mut files = entry
                    .files
                    .into_iter()
                    .map(|file| {
                        Ok(OfflineFile {
                            path: file.path,
                            size: file.size,
                            sha256: Digest::from_hex(&file.sha256)?,
                        })
                    })
                    .collect::<Result<Vec<_>, AirgapError>>()?;
                files.sort_by(|a, b| a.path.cmp(&b.path));
                Ok(RepoEntry {
                    kind: BundleKind::parse(&entry.kind)?,
                    name: entry.name,
                    version,
                    digest: Digest::from_hex(&entry.digest)?,
                    imported_at: entry.imported_at,
                    files,
                })
            })
            .collect::<Result<Vec<_>, AirgapError>>()?;
        entries.sort_by(entry_order);
        Ok(Self { entries })
    }

    /// The index's TOML file form, entries and files sorted so the bytes are deterministic.
    #[must_use]
    pub fn to_toml(&self) -> String {
        #[derive(serde::Serialize)]
        struct FileDoc<'a> {
            path: &'a str,
            size: u64,
            sha256: String,
        }
        #[derive(serde::Serialize)]
        struct EntryDoc<'a> {
            kind: &'a str,
            name: &'a str,
            version: String,
            digest: String,
            imported_at: i64,
            files: Vec<FileDoc<'a>>,
        }
        #[derive(serde::Serialize)]
        struct IndexDoc<'a> {
            entry: Vec<EntryDoc<'a>>,
        }
        let mut entries: Vec<&RepoEntry> = self.entries.iter().collect();
        entries.sort_by(|a, b| entry_order(a, b));
        let doc = IndexDoc {
            entry: entries
                .into_iter()
                .map(|entry| EntryDoc {
                    kind: entry.kind.as_str(),
                    name: &entry.name,
                    version: entry.version.to_string(),
                    digest: entry.digest.to_hex(),
                    imported_at: entry.imported_at,
                    files: entry
                        .files
                        .iter()
                        .map(|file| FileDoc {
                            path: &file.path,
                            size: file.size,
                            sha256: file.sha256.to_hex(),
                        })
                        .collect(),
                })
                .collect(),
        };
        toml::to_string_pretty(&doc).unwrap_or_default()
    }

    /// The newest imported entry for a kind and name, if any.
    #[must_use]
    pub fn installed(&self, kind: BundleKind, name: &str) -> Option<&RepoEntry> {
        self.entries
            .iter()
            .filter(|entry| entry.kind == kind && entry.name == name)
            .max_by(|a, b| a.version.cmp(&b.version))
    }

    /// The entry for a kind, name and exact version, if any.
    #[must_use]
    pub fn find(&self, kind: BundleKind, name: &str, version: &Version) -> Option<&RepoEntry> {
        self.entries
            .iter()
            .find(|entry| entry.kind == kind && entry.name == name && &entry.version == version)
    }

    /// Adds an entry, or reports a conflict when the same kind, name and version is already
    /// present with a different digest. Returns whether a new entry was added.
    ///
    /// # Errors
    /// [`AirgapError::RepoConflict`] when the same kind, name and version exists with another
    /// digest.
    pub fn upsert(&mut self, entry: RepoEntry) -> Result<bool, AirgapError> {
        if let Some(existing) = self.find(entry.kind, &entry.name, &entry.version) {
            if existing.digest == entry.digest {
                return Ok(false);
            }
            return Err(AirgapError::RepoConflict {
                name: entry.name,
                version: entry.version.to_string(),
            });
        }
        self.entries.push(entry);
        self.entries.sort_by(entry_order);
        Ok(true)
    }
}

/// The sort order of index entries: kind, then name, then version.
fn entry_order(a: &RepoEntry, b: &RepoEntry) -> std::cmp::Ordering {
    a.kind
        .cmp(&b.kind)
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.version.cmp(&b.version))
}

/// The paths of one local repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalRepo {
    root: PathBuf,
}

impl LocalRepo {
    /// The repository rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The repository root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The index file.
    #[must_use]
    pub fn index_path(&self) -> PathBuf {
        self.root.join(REPO_INDEX)
    }

    /// The directory an imported bundle lives in.
    #[must_use]
    pub fn bundle_dir(&self, kind: BundleKind, name: &str, version: &Version) -> PathBuf {
        self.root
            .join(REPO_BUNDLES_DIR)
            .join(kind.as_str())
            .join(name)
            .join(version.to_string())
    }

    /// The staging directory an import is written into before it is committed.
    #[must_use]
    pub fn staging_dir(&self, kind: BundleKind, name: &str, version: &Version) -> PathBuf {
        self.root
            .join(REPO_STAGING_DIR)
            .join(kind.as_str())
            .join(name)
            .join(format!("{version}{PARTIAL_SUFFIX}"))
    }

    /// Reads the index; a missing file is an empty index.
    ///
    /// # Errors
    /// [`AirgapError::RepoIndexMalformed`] when the file exists but does not parse, and
    /// [`AirgapError::Io`] when it cannot be read.
    pub fn read_index(&self, storage: &dyn Storage) -> Result<RepoIndex, AirgapError> {
        let path = self.index_path();
        if !storage.exists(&path)? {
            return Ok(RepoIndex::default());
        }
        let bytes = storage.read(&path)?;
        let text = String::from_utf8_lossy(&bytes);
        RepoIndex::from_toml(&text)
    }

    /// Writes the index atomically, creating the repository root first.
    ///
    /// # Errors
    /// [`AirgapError::Io`] when the write fails.
    pub fn write_index(&self, storage: &dyn Storage, index: &RepoIndex) -> Result<(), AirgapError> {
        storage.create_dir_all(&self.root)?;
        storage.write_atomic(&self.index_path(), index.to_toml().as_bytes())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn entry(version: &str, digest_byte: u8) -> RepoEntry {
        RepoEntry {
            kind: BundleKind::Runtime,
            name: "spot-runtime".to_owned(),
            version: Version::parse(version).unwrap(),
            digest: Digest::from_bytes([digest_byte; 32]),
            imported_at: 1_770_000_000,
            files: vec![OfflineFile {
                path: "bin/spot".to_owned(),
                size: 3,
                sha256: Digest::of(b"abc"),
            }],
        }
    }

    #[test]
    fn index_round_trips() {
        let mut index = RepoIndex::default();
        assert!(index.upsert(entry("2.1.0", 1)).unwrap());
        assert!(index.upsert(entry("2.0.0", 2)).unwrap());
        let text = index.to_toml();
        let parsed = RepoIndex::from_toml(&text).unwrap();
        assert_eq!(parsed, index);
        assert_eq!(
            parsed
                .installed(BundleKind::Runtime, "spot-runtime")
                .unwrap()
                .version
                .to_string(),
            "2.1.0"
        );
    }

    #[test]
    fn upsert_is_idempotent_and_conflicts_on_digest() {
        let mut index = RepoIndex::default();
        assert!(index.upsert(entry("2.1.0", 1)).unwrap());
        assert!(!index.upsert(entry("2.1.0", 1)).unwrap());
        let error = index.upsert(entry("2.1.0", 2)).unwrap_err();
        assert_eq!(error.code(), "airgap.repo_conflict");
    }
}
