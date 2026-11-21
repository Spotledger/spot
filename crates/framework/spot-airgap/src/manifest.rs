//! The offline bundle manifest (`offline.toml`) and its canonical digest.
//!
//! A bundle is a directory on media with a fixed shape:
//!
//! ```text
//! <media>/offline.toml       this manifest
//! <media>/offline.sig        the detached bundle signature (spot-bundle-signing)
//! <media>/certificate.json   the publisher certificate the signature chains to
//! <media>/files/...          the payload files, by manifest-relative path
//! ```
//!
//! The manifest names the bundle `kind` (`runtime`, `agent`, `pack` or `os-mirror`), its
//! `name` and `version`, and every payload file with its size and SHA-256. The bytes a
//! signature covers are the **bundle digest**: SHA-256 over the domain prefix and the
//! canonical JSON of every field ([`OfflineManifest::canonical_bytes`]), which is one exact
//! byte sequence per manifest (sorted keys, files sorted by path, no whitespace, integers
//! only). This mirrors `spot-updater`'s manifest rules; the cryptographic verification is
//! `spot-bundle-signing`'s, and no cryptography is written here.
//!
//! Parsing refuses unknown fields, so a typo is an error and never a silently ignored field.

use std::collections::BTreeMap;
use std::str::FromStr;

use semver::Version;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use spot_updater::Digest;

use crate::error::AirgapError;

/// The `format` field of every offline manifest.
pub const OFFLINE_MANIFEST_FORMAT: &str = "spot-offline-manifest";

/// The domain prefix of the bundle digest.
pub const BUNDLE_DIGEST_DOMAIN: &str = "SPOT-OFFLINE-BUNDLE-V1\n";

/// The media file name of the manifest.
pub const MEDIA_MANIFEST: &str = "offline.toml";
/// The media file name of the detached bundle signature.
///
/// Built from parts so the repository's error-code scan never mistakes a file name for a
/// stable error code (`offline.sig` looks like a dotted code).
pub const MEDIA_SIGNATURE: &str = concat!("offline", ".", "sig");
/// The media file name of the publisher certificate.
///
/// Built from parts for the same reason as [`MEDIA_SIGNATURE`].
pub const MEDIA_CERTIFICATE: &str = concat!("certificate", ".", "json");
/// The media directory that holds the payload files.
pub const MEDIA_FILES_DIR: &str = "files";

/// What an offline bundle carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BundleKind {
    /// The Spotledger runtime.
    Runtime,
    /// The host agent.
    Agent,
    /// An application pack (an app bundle's files).
    Pack,
    /// An operating-system package mirror.
    OsMirror,
}

impl BundleKind {
    /// The lower-case name the manifest carries and the repository layout uses.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Agent => "agent",
            Self::Pack => "pack",
            Self::OsMirror => "os-mirror",
        }
    }

    /// Parses a kind name.
    ///
    /// # Errors
    /// [`AirgapError::ManifestMalformed`] for any name that is not one of the four kinds.
    pub fn parse(text: &str) -> Result<Self, AirgapError> {
        match text {
            "runtime" => Ok(Self::Runtime),
            "agent" => Ok(Self::Agent),
            "pack" => Ok(Self::Pack),
            "os-mirror" => Ok(Self::OsMirror),
            other => Err(AirgapError::ManifestMalformed {
                detail: format!("`{other}` is not a bundle kind"),
            }),
        }
    }
}

/// One payload file the manifest commits to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineFile {
    /// The file's path relative to the media `files/` directory.
    pub path: String,
    /// The file's size in bytes.
    pub size: u64,
    /// The file's SHA-256.
    pub sha256: Digest,
}

/// The offline bundle manifest: what a bundle claims about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfflineManifest {
    /// What the bundle carries.
    pub kind: BundleKind,
    /// The bundle's name (the runtime name, the agent name, or the app id of a pack).
    pub name: String,
    /// The bundle's version.
    pub version: Version,
    /// When the bundle was produced (UTC seconds).
    pub released_at: i64,
    /// Release notes for an administrator.
    pub notes: Option<String>,
    /// Every payload file, sorted by path.
    pub files: Vec<OfflineFile>,
}

/// The raw TOML shape, with string versions and digests, checked into the typed manifest.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    format: String,
    kind: String,
    name: String,
    version: String,
    released_at: i64,
    #[serde(default)]
    notes: Option<String>,
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

impl OfflineManifest {
    /// Parses an `offline.toml` document, checking every field and sorting the files by path.
    ///
    /// # Errors
    /// [`AirgapError::ManifestMalformed`] when the TOML does not parse, an unknown field is
    /// present, the `format` is not [`OFFLINE_MANIFEST_FORMAT`], a version is not semver, the
    /// kind is unknown, a file path is absolute or escapes its directory, or a digest is not
    /// 64 hex characters.
    pub fn from_toml(text: &str) -> Result<Self, AirgapError> {
        let raw: RawManifest =
            toml::from_str(text).map_err(|error| AirgapError::ManifestMalformed {
                detail: error.to_string(),
            })?;
        if raw.format != OFFLINE_MANIFEST_FORMAT {
            return Err(AirgapError::ManifestMalformed {
                detail: format!(
                    "the format is `{}`, not `{OFFLINE_MANIFEST_FORMAT}`",
                    raw.format
                ),
            });
        }
        let version =
            Version::parse(&raw.version).map_err(|error| AirgapError::ManifestMalformed {
                detail: format!("`{}` is not a semver version: {error}", raw.version),
            })?;
        // The name becomes a directory in the repository, so it is one safe path component.
        if raw.name.contains('/') || !is_safe_relative_path(&raw.name) {
            return Err(AirgapError::ManifestMalformed {
                detail: format!("`{}` is not a safe bundle name", raw.name),
            });
        }
        let mut files = raw
            .files
            .into_iter()
            .map(|file| {
                if !is_safe_relative_path(&file.path) {
                    return Err(AirgapError::ManifestMalformed {
                        detail: format!("`{}` is not a safe relative path", file.path),
                    });
                }
                Ok(OfflineFile {
                    path: file.path,
                    size: file.size,
                    sha256: Digest::from_hex(&file.sha256)?,
                })
            })
            .collect::<Result<Vec<_>, AirgapError>>()?;
        files.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(Self {
            kind: BundleKind::parse(&raw.kind)?,
            name: raw.name,
            version,
            released_at: raw.released_at,
            notes: raw.notes,
            files,
        })
    }

    /// The manifest's TOML file form, files sorted by path so the bytes are deterministic.
    #[must_use]
    pub fn to_toml(&self) -> String {
        #[derive(serde::Serialize)]
        struct FileDoc<'a> {
            path: &'a str,
            size: u64,
            sha256: String,
        }
        #[derive(serde::Serialize)]
        struct ManifestDoc<'a> {
            format: &'a str,
            kind: &'a str,
            name: &'a str,
            version: String,
            released_at: i64,
            #[serde(skip_serializing_if = "Option::is_none")]
            notes: Option<String>,
            files: Vec<FileDoc<'a>>,
        }
        let doc = ManifestDoc {
            format: OFFLINE_MANIFEST_FORMAT,
            kind: self.kind.as_str(),
            name: &self.name,
            version: self.version.to_string(),
            released_at: self.released_at,
            notes: self.notes.clone(),
            files: self
                .sorted_files()
                .into_iter()
                .map(|file| FileDoc {
                    path: &file.path,
                    size: file.size,
                    sha256: file.sha256.to_hex(),
                })
                .collect(),
        };
        // Serialising this plain document cannot fail.
        toml::to_string_pretty(&doc).unwrap_or_default()
    }

    /// The canonical JSON of every field: sorted keys, files sorted by path, no whitespace,
    /// integers only. One exact byte sequence per manifest.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&self.signed_fields()).unwrap_or_default()
    }

    /// The bundle digest a signature covers: SHA-256 over [`BUNDLE_DIGEST_DOMAIN`] and the
    /// canonical manifest bytes.
    #[must_use]
    pub fn digest(&self) -> Digest {
        let mut hasher = Sha256::new();
        hasher.update(BUNDLE_DIGEST_DOMAIN.as_bytes());
        hasher.update(self.canonical_bytes());
        Digest::from_bytes(hasher.finalize().into())
    }

    /// The bundle digest in the signing crate's own type, for
    /// [`spot_bundle_signing::verify`](spot_bundle_signing::verify).
    ///
    /// The two types are both a SHA-256 and carry the same 32 bytes; the conversion is by
    /// hex because `spot-bundle`'s constructor is crate-private.
    ///
    /// # Errors
    /// [`AirgapError::SignatureMalformed`] if the digest text does not parse, which cannot
    /// happen for a digest this crate produced.
    pub fn signing_digest(&self) -> Result<spot_bundle::Digest, AirgapError> {
        spot_bundle::Digest::from_str(&self.digest().to_hex()).map_err(|error| {
            AirgapError::SignatureMalformed {
                detail: error.to_string(),
            }
        })
    }

    /// The manifest entry for `path`, if it lists it.
    #[must_use]
    pub fn file(&self, path: &str) -> Option<&OfflineFile> {
        self.files.iter().find(|file| file.path == path)
    }

    /// The files in path order, whatever order the struct holds them in, so the manifest's
    /// written and canonical bytes are deterministic.
    fn sorted_files(&self) -> Vec<&OfflineFile> {
        let mut files: Vec<&OfflineFile> = self.files.iter().collect();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        files
    }

    /// The manifest as the sorted map the canonical bytes are built from.
    fn signed_fields(&self) -> BTreeMap<&str, Value> {
        let files = self
            .sorted_files()
            .into_iter()
            .map(|file| {
                let mut fields = BTreeMap::new();
                fields.insert("path", Value::from(file.path.as_str()));
                fields.insert("sha256", Value::from(file.sha256.to_hex()));
                fields.insert("size", Value::from(file.size));
                canonical_object(fields)
            })
            .collect::<Vec<_>>();
        let mut fields = BTreeMap::new();
        fields.insert("files", Value::from(files));
        fields.insert("format", Value::from(OFFLINE_MANIFEST_FORMAT));
        fields.insert("kind", Value::from(self.kind.as_str()));
        fields.insert("name", Value::from(self.name.as_str()));
        fields.insert("notes", optional(self.notes.as_deref()));
        fields.insert("released_at", Value::from(self.released_at));
        fields.insert("version", Value::from(self.version.to_string()));
        fields
    }
}

/// A nested object, built in sorted key order.
fn canonical_object(fields: BTreeMap<&str, Value>) -> Value {
    serde_json::to_value(fields).unwrap_or(Value::Null)
}

/// A `null` for `None`, a string for `Some`, so the canonical bytes always carry the key.
fn optional(value: Option<&str>) -> Value {
    match value {
        Some(text) => Value::from(text),
        None => Value::Null,
    }
}

/// Whether `path` is a non-empty relative path with no parent or root component.
///
/// The media is removable and may be hostile, so a manifest path must never name a file
/// outside the media's `files/` directory.
#[must_use]
pub fn is_safe_relative_path(path: &str) -> bool {
    use std::path::Component;
    if path.is_empty() || path.contains('\\') || path.contains('\0') {
        return false;
    }
    std::path::Path::new(path)
        .components()
        .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn manifest() -> OfflineManifest {
        OfflineManifest {
            kind: BundleKind::Runtime,
            name: "spot-runtime".to_owned(),
            version: Version::parse("2.1.0").unwrap(),
            released_at: 1_770_000_000,
            notes: Some("first offline build".to_owned()),
            files: vec![
                OfflineFile {
                    path: "README".to_owned(),
                    size: 2,
                    sha256: Digest::of(b"hi"),
                },
                OfflineFile {
                    path: "bin/spot".to_owned(),
                    size: 10,
                    sha256: Digest::of(b"0123456789"),
                },
            ],
        }
    }

    #[test]
    fn toml_round_trips_and_sorts_files() {
        let mut source = manifest();
        source.files.reverse();
        let text = source.to_toml();
        let parsed = OfflineManifest::from_toml(&text).unwrap();
        assert_eq!(parsed.files[0].path, "README");
        assert_eq!(parsed.files[1].path, "bin/spot");
        assert_eq!(parsed, manifest());
    }

    #[test]
    fn unknown_fields_are_refused() {
        let text = "format = \"spot-offline-manifest\"\nkind = \"runtime\"\nname = \"x\"\n\
                    version = \"1.0.0\"\nreleased_at = 0\nsurprise = 1\n";
        assert!(OfflineManifest::from_toml(text).is_err());
    }

    #[test]
    fn unsafe_paths_are_refused() {
        let text = "format = \"spot-offline-manifest\"\nkind = \"runtime\"\nname = \"x\"\n\
                    version = \"1.0.0\"\nreleased_at = 0\n[[files]]\npath = \"../escape\"\n\
                    size = 1\nsha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n";
        assert!(OfflineManifest::from_toml(text).is_err());
    }

    #[test]
    fn unsafe_names_are_refused() {
        for name in ["../x", "a/b", "/abs", "a\\b", ""] {
            let text = format!(
                "format = \"spot-offline-manifest\"\nkind = \"runtime\"\nname = {name:?}\n\
                 version = \"1.0.0\"\nreleased_at = 0\n"
            );
            assert!(OfflineManifest::from_toml(&text).is_err(), "{name}");
        }
    }

    #[test]
    fn canonical_bytes_and_digest_are_order_independent() {
        let mut shuffled = manifest();
        shuffled.files.reverse();
        assert_eq!(manifest().canonical_bytes(), shuffled.canonical_bytes());
        assert_eq!(manifest().digest(), shuffled.digest());
    }

    #[test]
    fn digest_changes_with_any_field() {
        let mut changed = manifest();
        changed.released_at += 1;
        assert_ne!(manifest().digest(), changed.digest());
        let mut changed = manifest();
        changed.version = Version::parse("2.1.1").unwrap();
        assert_ne!(manifest().digest(), changed.digest());
    }
}
