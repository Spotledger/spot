//! The canonical backup manifest (`backup.toml`) carried in every package header.
//!
//! A manifest names the tenant, the platform and schema versions, the app set, the
//! package digest and size, when it was made and which chain it belongs to. A backup
//! is only restorable once its package passed verification; the manifest alone is a
//! claim and [`crate::verify`] is the proof.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::error::{BackupError, Result};
use crate::model::{KeyRef, VerifyMode};

/// The file name of the manifest, inside a package and beside a hand-copied backup.
pub const MANIFEST_FILE: &str = "backup.toml";

/// The manifest format this crate writes and reads.
pub const MANIFEST_FORMAT: &str = "spot-backup/1";

/// Everything a package header records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupManifest {
    /// Always [`MANIFEST_FORMAT`].
    pub format: String,
    /// The backup id, unique within its target.
    pub id: String,
    /// The tenant the backup covers.
    pub tenant: String,
    /// When the package was created, RFC 3339 in UTC.
    pub created_at: String,
    /// The platform version that produced the backup.
    pub platform_version: String,
    /// The snapshot provider's own version.
    pub snapshot_provider_version: String,
    /// PostgreSQL major version of the data.
    pub pg_major: u32,
    /// Tenant schema version (the migration lifecycle's version) the data is at.
    pub schema_version: u64,
    /// Installed apps at take time, sorted.
    pub app_set: Vec<String>,
    /// SHA-256 of the package's payload section, lower-case hex.
    pub digest: String,
    /// Sum of the payload files' bytes, integer.
    pub size: u64,
    /// Number of files in the payload.
    pub file_count: u64,
    /// Stable id of the backup chain (the first backup's id).
    pub chain_id: String,
    /// Position in the chain, starting at 1.
    pub chain_sequence: u64,
    /// Digest of the previous backup in the chain, when any.
    pub parent_digest: Option<String>,
    /// The verification the job requested.
    pub verify: VerifyMode,
    /// The target as configured, for a human reading the manifest.
    pub target: String,
    /// The encryption key reference, when the payload is encrypted by the host.
    pub encryption: Option<KeyRef>,
}

impl BackupManifest {
    /// Renders the manifest as canonical TOML.
    ///
    /// # Errors
    /// [`BackupError::ManifestInvalid`] when serialisation fails.
    pub fn to_toml(&self) -> Result<String> {
        toml::to_string_pretty(self).map_err(|error| BackupError::ManifestInvalid {
            reason: format!("the manifest cannot be rendered: {error}"),
        })
    }

    /// Parses a manifest and checks its canonical rules.
    ///
    /// # Errors
    /// [`BackupError::ManifestInvalid`] when it does not parse or breaks a rule.
    pub fn from_toml(text: &str) -> Result<BackupManifest> {
        let manifest: BackupManifest =
            toml::from_str(text).map_err(|error| BackupError::ManifestInvalid {
                reason: format!("backup.toml does not parse: {error}"),
            })?;
        manifest.canonical_checks()?;
        Ok(manifest)
    }

    /// Checks the fields whose shape the rest of the crate relies on.
    ///
    /// # Errors
    /// [`BackupError::ManifestInvalid`] naming the broken rule.
    pub fn canonical_checks(&self) -> Result<()> {
        if self.format != MANIFEST_FORMAT {
            return Err(BackupError::ManifestInvalid {
                reason: format!(
                    "the manifest format {:?} is not {MANIFEST_FORMAT}",
                    self.format
                ),
            });
        }
        for (label, value) in [
            ("id", &self.id),
            ("tenant", &self.tenant),
            ("platform_version", &self.platform_version),
            ("snapshot_provider_version", &self.snapshot_provider_version),
            ("chain_id", &self.chain_id),
        ] {
            if value.trim().is_empty() || value.trim() != value {
                return Err(BackupError::ManifestInvalid {
                    reason: format!("the manifest's {label} is empty or padded"),
                });
            }
        }
        if !crate::package::is_sha256_hex(&self.digest) {
            return Err(BackupError::ManifestInvalid {
                reason: format!(
                    "the manifest's digest {:?} is not a SHA-256 hex string",
                    self.digest
                ),
            });
        }
        if self.chain_sequence == 0 {
            return Err(BackupError::ManifestInvalid {
                reason: "a manifest's chain_sequence must start at 1".to_owned(),
            });
        }
        if self.pg_major == 0 {
            return Err(BackupError::ManifestInvalid {
                reason: "the manifest names PostgreSQL major version 0".to_owned(),
            });
        }
        if self.app_set.iter().any(|app| app.trim().is_empty()) {
            return Err(BackupError::ManifestInvalid {
                reason: "the manifest's app_set names an empty app".to_owned(),
            });
        }
        let mut sorted = self.app_set.clone();
        sorted.sort();
        sorted.dedup();
        if sorted != self.app_set {
            return Err(BackupError::ManifestInvalid {
                reason: "the manifest's app_set must be sorted and without duplicates".to_owned(),
            });
        }
        self.created_at()?;
        Ok(())
    }

    /// The creation instant.
    ///
    /// # Errors
    /// [`BackupError::ManifestInvalid`] when `created_at` is not RFC 3339.
    pub fn created_at(&self) -> Result<OffsetDateTime> {
        OffsetDateTime::parse(&self.created_at, &Rfc3339).map_err(|error| {
            BackupError::ManifestInvalid {
                reason: format!(
                    "the manifest's created_at {:?} is not RFC 3339: {error}",
                    self.created_at
                ),
            }
        })
    }

    /// Renders an instant in the canonical RFC 3339 UTC form.
    pub fn format_instant(at: OffsetDateTime) -> String {
        at.to_offset(time::UtcOffset::UTC)
            .format(&Rfc3339)
            .unwrap_or_else(|_| at.replace_offset(time::UtcOffset::UTC).to_string())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::TargetRef;

    fn sample() -> BackupManifest {
        BackupManifest {
            format: MANIFEST_FORMAT.to_owned(),
            id: "acme-chain-1".to_owned(),
            tenant: "acme".to_owned(),
            created_at: "2026-10-19T12:00:00Z".to_owned(),
            platform_version: "0.1.0".to_owned(),
            snapshot_provider_version: "0.1.0".to_owned(),
            pg_major: 18,
            schema_version: 7,
            app_set: vec!["crm".to_owned(), "erp".to_owned()],
            digest: "a".repeat(64),
            size: 42,
            file_count: 1,
            chain_id: "chain".to_owned(),
            chain_sequence: 1,
            parent_digest: None,
            verify: VerifyMode::Quick,
            target: TargetRef::Folder {
                path: "/backups".into(),
            }
            .to_string(),
            encryption: None,
        }
    }

    #[test]
    fn round_trips_through_toml_and_rejects_a_bad_digest() {
        let manifest = sample();
        let text = manifest.to_toml().expect("render");
        assert!(text.contains("format = \"spot-backup/1\""));
        let parsed = BackupManifest::from_toml(&text).expect("parse");
        assert_eq!(parsed, manifest);

        let mut bad = sample();
        bad.digest = "xyz".to_owned();
        assert_eq!(
            BackupManifest::from_toml(&bad.to_toml().expect("render"))
                .expect_err("bad digest")
                .code(),
            "backup.manifest_invalid"
        );
    }

    #[test]
    fn rejects_unsorted_apps_and_bad_instants() {
        let mut manifest = sample();
        manifest.app_set = vec!["erp".to_owned(), "crm".to_owned()];
        assert!(manifest.canonical_checks().is_err());
        let mut manifest = sample();
        manifest.created_at = "yesterday".to_owned();
        assert!(manifest.canonical_checks().is_err());
    }
}
