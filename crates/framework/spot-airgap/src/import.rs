//! Import from removable media: verify, stage, commit, index.
//!
//! [`import`] is the one entry point. It reads the bundle from the media, verifies its
//! signature and every file, refuses a downgrade (an older version than the one the repository
//! already holds for the same kind and name) and a same-version bundle whose digest changed,
//! then stages the files and renames the staging directory into the repository in one step. The
//! index is written atomically afterwards.
//!
//! An interrupted import is **resumable**: the staging directory and its marker are left
//! behind, and a second call with the same bundle skips the files already staged and verified
//! and finishes. Nothing is ever committed until every file has verified.
//!
//! Time is an argument (`imported_at`); this module reads no clock and contacts no network.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use spot_updater::Digest;

use crate::bundle::{OfflineBundle, read_media};
use crate::error::AirgapError;
use crate::manifest::{BundleKind, MEDIA_FILES_DIR};
use crate::repo::{LocalRepo, RepoEntry};
use crate::storage::{Storage, walk_files};
use crate::verify::{BundleTrust, verify_file};

/// The `format` field of a staging marker.
pub const STAGING_FORMAT: &str = "spot-airgap-staging";

/// What an import is asked to do.
#[derive(Clone, Copy, Debug)]
pub struct ImportRequest<'a> {
    /// The media directory that holds the bundle.
    pub media: &'a Path,
    /// The local repository to import into.
    pub repo: &'a LocalRepo,
    /// The trust material the bundle is checked against.
    pub trust: BundleTrust<'a>,
    /// When the import runs (UTC seconds); never read from a clock.
    pub imported_at: i64,
}

/// What an import did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportReport {
    /// What the bundle carries.
    pub kind: BundleKind,
    /// The bundle name.
    pub name: String,
    /// The bundle version.
    pub version: semver::Version,
    /// The bundle digest the signature covered.
    pub digest: Digest,
    /// How many files were staged.
    pub files: usize,
    /// Whether a partial staging directory from an earlier run was resumed.
    pub resumed: bool,
    /// Whether the repository already held this exact bundle.
    pub already_present: bool,
}

/// The staging marker: which bundle a staging directory belongs to.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StagingMarker {
    format: String,
    digest: String,
}

impl StagingMarker {
    fn of(digest: &Digest) -> Self {
        Self {
            format: STAGING_FORMAT.to_owned(),
            digest: digest.to_hex(),
        }
    }

    fn parse(text: &str) -> Result<Self, AirgapError> {
        let marker: StagingMarker =
            toml::from_str(text).map_err(|error| AirgapError::StagingMalformed {
                detail: error.to_string(),
            })?;
        if marker.format != STAGING_FORMAT {
            return Err(AirgapError::StagingMalformed {
                detail: format!("the format is `{}`, not `{STAGING_FORMAT}`", marker.format),
            });
        }
        Ok(marker)
    }
}

/// Imports one bundle from media into the local repository. See the module docs for the steps.
///
/// # Errors
/// [`AirgapError`] naming the first check that failed: a malformed manifest or document, a
/// signature that does not verify or is revoked, a missing or tampered file, a downgrade or a
/// digest change, a repository conflict, or an I/O failure.
pub fn import(
    storage: &dyn Storage,
    request: &ImportRequest<'_>,
) -> Result<ImportReport, AirgapError> {
    // 1-4. Read the bundle's documents and verify its signature and trust.
    let bundle = OfflineBundle::read(storage, request.media)?;
    bundle.verify(&request.trust)?;
    let manifest = &bundle.manifest;

    // 5. Refuse a downgrade and a changed digest before touching the repository.
    let digest = manifest.digest();
    let mut index = request.repo.read_index(storage)?;
    if let Some(installed) = index.installed(manifest.kind, &manifest.name) {
        if manifest.version < installed.version {
            return Err(AirgapError::Downgrade {
                installed: installed.version.to_string(),
                offered: manifest.version.to_string(),
            });
        }
        if manifest.version == installed.version && installed.digest != digest {
            return Err(AirgapError::DigestChanged {
                name: manifest.name.clone(),
                version: manifest.version.to_string(),
            });
        }
        if manifest.version == installed.version && installed.digest == digest {
            return Ok(ImportReport {
                kind: manifest.kind,
                name: manifest.name.clone(),
                version: manifest.version.clone(),
                digest,
                files: manifest.files.len(),
                resumed: false,
                already_present: true,
            });
        }
    }

    // 6. Every file the manifest names must be present and match, and nothing else may be.
    let media_files = request.media.join(MEDIA_FILES_DIR);
    if storage.exists(&media_files)? {
        let present = walk_files(storage, &media_files)?;
        for path in &present {
            if manifest.file(path).is_none() {
                return Err(AirgapError::ExtraFile { path: path.clone() });
            }
        }
    }
    for file in &manifest.files {
        let media_file = media_files.join(&file.path);
        if !storage.exists(&media_file)? {
            return Err(AirgapError::FileMissing {
                path: file.path.clone(),
            });
        }
        let bytes = read_media(storage, &media_file)?;
        verify_file(file, &bytes)?;
    }

    // 7. Stage: reuse a matching partial directory, or start a fresh one.
    let staging = request
        .repo
        .staging_dir(manifest.kind, &manifest.name, &manifest.version);
    let marker_path = staging_marker_path(&staging);
    let mut resumed = false;
    if storage.exists(&staging)? {
        let matches = storage
            .exists(&marker_path)?
            .then(|| {
                storage
                    .read(&marker_path)
                    .ok()
                    .and_then(|bytes| StagingMarker::parse(&String::from_utf8_lossy(&bytes)).ok())
            })
            .flatten()
            .is_some_and(|marker| marker.digest == digest.to_hex());
        if matches {
            resumed = true;
        } else {
            storage.remove_dir_all(&staging)?;
        }
    }
    storage.create_dir_all(&staging)?;
    storage.write_atomic(
        &marker_path,
        StagingMarker::of(&digest).to_toml().as_bytes(),
    )?;
    for file in &manifest.files {
        let target = staging.join(&file.path);
        if storage.exists(&target)?
            && let Ok(bytes) = storage.read(&target)
            && verify_file(file, &bytes).is_ok()
        {
            // Already staged and verified by an earlier, interrupted run.
            continue;
        }
        let bytes = read_media(storage, &media_files.join(&file.path))?;
        if let Some(parent) = target.parent() {
            storage.create_dir_all(parent)?;
        }
        storage.write_atomic(&target, &bytes)?;
    }

    // 8. Commit: rename the whole directory in one step (or finish a rename that happened
    //    before a crash), then index atomically.
    let bundle_dir = request
        .repo
        .bundle_dir(manifest.kind, &manifest.name, &manifest.version);
    if !storage.exists(&bundle_dir)? {
        if let Some(parent) = bundle_dir.parent() {
            storage.create_dir_all(parent)?;
        }
        storage.rename(&staging, &bundle_dir)?;
    }
    storage.remove_file(&marker_path)?;

    let entry = RepoEntry {
        kind: manifest.kind,
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        digest,
        imported_at: request.imported_at,
        files: manifest.files.clone(),
    };
    let added = index.upsert(entry)?;
    if added {
        request.repo.write_index(storage, &index)?;
    }

    Ok(ImportReport {
        kind: manifest.kind,
        name: manifest.name.clone(),
        version: manifest.version.clone(),
        digest,
        files: manifest.files.len(),
        resumed,
        already_present: !added,
    })
}

/// The marker path beside a staging directory (never inside it, so a rename moves only files).
#[must_use]
pub fn staging_marker_path(staging: &Path) -> PathBuf {
    PathBuf::from(format!("{}.marker", staging.display()))
}

impl StagingMarker {
    fn to_toml(&self) -> String {
        #[derive(Serialize)]
        struct Doc<'a> {
            format: &'a str,
            digest: &'a str,
        }
        toml::to_string_pretty(&Doc {
            format: &self.format,
            digest: &self.digest,
        })
        .unwrap_or_default()
    }
}
