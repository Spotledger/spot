//! The cloud target port and its recording fake.
//!
//! The transport and encryption live in `spot-cloud-backup` (WP-CTL-08, a later
//! package). This crate owns only the port [`CloudStore`], the adapter
//! [`CloudTarget`] and an in-memory recording implementation the tests drive.

use std::collections::BTreeMap;
use std::io::{Cursor, Read};
use std::sync::Mutex;

use crate::error::{BackupError, Result};
use crate::model::{StoredBackup, TargetStatus};
use crate::package::{PACKAGE_EXTENSION, is_sha256_hex};
use crate::target::{BackupTarget, parse_header};

/// One object in a cloud store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudObject {
    /// The object key.
    pub key: String,
    /// The object's bytes (the port moves whole packages; a real transport streams).
    pub bytes: Vec<u8>,
}

/// The cloud transport port: object put/get/list/delete plus the remaining quota.
pub trait CloudStore: Send + Sync {
    /// Writes `bytes` under `key`.
    ///
    /// # Errors
    /// [`BackupError::Io`] or [`BackupError::TargetUnavailable`].
    fn put_object(&self, key: &str, bytes: &[u8]) -> Result<()>;

    /// Reads the object at `key`.
    ///
    /// # Errors
    /// [`BackupError::NotFound`] or [`BackupError::Io`].
    fn get_object(&self, key: &str) -> Result<Vec<u8>>;

    /// Lists keys below `prefix`.
    ///
    /// # Errors
    /// [`BackupError::Io`].
    fn list_objects(&self, prefix: &str) -> Result<Vec<String>>;

    /// Deletes the object at `key`.
    ///
    /// # Errors
    /// [`BackupError::NotFound`] or [`BackupError::Io`].
    fn delete_object(&self, key: &str) -> Result<()>;

    /// Remaining bytes the account or bucket will accept.
    ///
    /// # Errors
    /// [`BackupError::TargetUnavailable`].
    fn remaining_bytes(&self) -> Result<u64>;
}

/// A target over a cloud prefix.
pub struct CloudTarget {
    store: std::sync::Arc<dyn CloudStore>,
    prefix: String,
}

impl CloudTarget {
    /// A target over `prefix`.
    pub fn new(store: std::sync::Arc<dyn CloudStore>, prefix: impl Into<String>) -> CloudTarget {
        CloudTarget {
            store,
            prefix: prefix.into(),
        }
    }

    fn key(&self, id: &str) -> Result<String> {
        if id.is_empty()
            || id.len() > 200
            || !id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(BackupError::ManifestInvalid {
                reason: format!("the backup id {id:?} is not usable as a cloud key"),
            });
        }
        let prefix = self.prefix.trim_end_matches('/');
        if prefix.is_empty() {
            Ok(format!("{id}.{PACKAGE_EXTENSION}"))
        } else {
            Ok(format!("{prefix}/{id}.{PACKAGE_EXTENSION}"))
        }
    }

    fn id_from_key(&self, key: &str) -> Option<String> {
        let name = key.rsplit('/').next()?;
        let id = name.strip_suffix(&format!(".{PACKAGE_EXTENSION}"))?;
        Some(id.to_owned())
    }

    fn stored(&self, id: &str, bytes: Vec<u8>) -> Result<StoredBackup> {
        let mut cursor = Cursor::new(bytes.as_slice());
        let manifest = parse_header(&mut cursor)?;
        Ok(StoredBackup {
            id: id.to_owned(),
            manifest,
            package_bytes: bytes.len() as u64,
        })
    }
}

impl std::fmt::Debug for CloudTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CloudTarget")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl BackupTarget for CloudTarget {
    fn describe(&self) -> String {
        format!("cloud:{}", self.prefix)
    }

    fn status(&self) -> TargetStatus {
        match self.store.remaining_bytes() {
            Ok(_) => TargetStatus::Ready,
            Err(error) => TargetStatus::Unavailable {
                reason: error.reason(),
            },
        }
    }

    fn put(
        &self,
        id: &str,
        mut body: Box<dyn Read + Send>,
        size: u64,
        digest: &str,
    ) -> Result<StoredBackup> {
        if !is_sha256_hex(digest) {
            return Err(BackupError::ManifestInvalid {
                reason: "a cloud put needs a SHA-256 digest".to_owned(),
            });
        }
        let mut bytes = Vec::new();
        body.read_to_end(&mut bytes)?;
        if bytes.len() as u64 != size {
            return Err(BackupError::VerifyFailed {
                reason: format!(
                    "the cloud target received {} bytes for {id} but the manifest says {size}",
                    bytes.len()
                ),
            });
        }
        // The store's own adapter re-hashes; this port also checks before uploading.
        let actual = sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(digest) {
            return Err(BackupError::VerifyFailed {
                reason: format!(
                    "the cloud target computed digest {actual} for {id} but the manifest says {digest}"
                ),
            });
        }
        let key = self.key(id)?;
        self.store.put_object(&key, &bytes)?;
        self.stored(id, bytes)
    }

    fn get(&self, id: &str) -> Result<Box<dyn Read + Send>> {
        let key = self.key(id)?;
        let bytes = self.store.get_object(&key)?;
        Ok(Box::new(Cursor::new(bytes)))
    }

    fn list(&self) -> Result<Vec<StoredBackup>> {
        let prefix = self.prefix.trim_end_matches('/');
        let prefix = if prefix.is_empty() {
            String::new()
        } else {
            format!("{prefix}/")
        };
        let mut out = Vec::new();
        for key in self.store.list_objects(&prefix)? {
            let Some(id) = self.id_from_key(&key) else {
                continue;
            };
            let bytes = self.store.get_object(&key)?;
            out.push(self.stored(&id, bytes)?);
        }
        out.sort_by(|left, right| {
            right
                .manifest
                .created_at
                .cmp(&left.manifest.created_at)
                .then_with(|| {
                    right
                        .manifest
                        .chain_sequence
                        .cmp(&left.manifest.chain_sequence)
                })
        });
        Ok(out)
    }

    fn delete(&self, id: &str) -> Result<()> {
        let key = self.key(id)?;
        self.store.delete_object(&key)
    }

    fn free_space(&self) -> Result<u64> {
        self.store.remaining_bytes()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// A recording cloud store: in-memory, deterministic, with a configurable quota.
#[derive(Debug, Default)]
pub struct RecordingCloud {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    quota: Mutex<u64>,
}

impl RecordingCloud {
    /// An empty store with `quota` remaining bytes.
    pub fn new(quota: u64) -> RecordingCloud {
        RecordingCloud {
            objects: Mutex::new(BTreeMap::new()),
            quota: Mutex::new(quota),
        }
    }

    /// The keys currently stored.
    ///
    /// # Errors
    /// [`BackupError::io`] when the mutex is poisoned.
    pub fn keys(&self) -> Result<Vec<String>> {
        let guard = self
            .objects
            .lock()
            .map_err(|_| BackupError::io("the recording cloud is poisoned"))?;
        Ok(guard.keys().cloned().collect())
    }
}

impl CloudStore for RecordingCloud {
    fn put_object(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let mut quota = self
            .quota
            .lock()
            .map_err(|_| BackupError::io("the recording cloud is poisoned"))?;
        let used = *quota;
        if (bytes.len() as u64) > used {
            return Err(BackupError::TargetUnavailable {
                target: "cloud".to_owned(),
                reason: "the account quota is exhausted".to_owned(),
            });
        }
        let mut objects = self
            .objects
            .lock()
            .map_err(|_| BackupError::io("the recording cloud is poisoned"))?;
        objects.insert(key.to_owned(), bytes.to_vec());
        *quota = used.saturating_sub(bytes.len() as u64);
        Ok(())
    }

    fn get_object(&self, key: &str) -> Result<Vec<u8>> {
        let objects = self
            .objects
            .lock()
            .map_err(|_| BackupError::io("the recording cloud is poisoned"))?;
        objects
            .get(key)
            .cloned()
            .ok_or_else(|| BackupError::NotFound { id: key.to_owned() })
    }

    fn list_objects(&self, prefix: &str) -> Result<Vec<String>> {
        let objects = self
            .objects
            .lock()
            .map_err(|_| BackupError::io("the recording cloud is poisoned"))?;
        Ok(objects
            .keys()
            .filter(|key| key.starts_with(prefix))
            .cloned()
            .collect())
    }

    fn delete_object(&self, key: &str) -> Result<()> {
        let mut objects = self
            .objects
            .lock()
            .map_err(|_| BackupError::io("the recording cloud is poisoned"))?;
        objects
            .remove(key)
            .map(|_| ())
            .ok_or_else(|| BackupError::NotFound { id: key.to_owned() })
    }

    fn remaining_bytes(&self) -> Result<u64> {
        let quota = self
            .quota
            .lock()
            .map_err(|_| BackupError::io("the recording cloud is poisoned"))?;
        Ok(*quota)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::manifest::{BackupManifest, MANIFEST_FORMAT};
    use crate::model::VerifyMode;
    use crate::package::{PackageEntry, build_from_files};
    use std::sync::Arc;

    fn manifest() -> BackupManifest {
        BackupManifest {
            format: MANIFEST_FORMAT.to_owned(),
            id: "acme-chain-1".to_owned(),
            tenant: "acme".to_owned(),
            created_at: "2026-10-19T12:00:00Z".to_owned(),
            platform_version: "0.1.0".to_owned(),
            snapshot_provider_version: "0.1.0".to_owned(),
            pg_major: 18,
            schema_version: 1,
            app_set: vec!["crm".to_owned()],
            digest: "0".repeat(64),
            size: 0,
            file_count: 0,
            chain_id: "chain".to_owned(),
            chain_sequence: 1,
            parent_digest: None,
            verify: VerifyMode::Quick,
            target: "cloud:tenant".to_owned(),
            encryption: None,
        }
    }

    #[test]
    fn records_puts_lists_and_deletes_under_the_prefix() {
        let work = tempfile::tempdir().expect("work");
        let store = Arc::new(RecordingCloud::new(1 << 20));
        let target = CloudTarget::new(store.clone(), "tenant/acme");
        let built = build_from_files(
            manifest(),
            vec![PackageEntry {
                path: "a".to_owned(),
                bytes: b"abc".to_vec(),
            }],
            work.path(),
        )
        .expect("build");
        let bytes = std::fs::read(built.package.path()).expect("read");
        let (size, digest) = built.package.stream_facts().expect("facts");
        target
            .put("acme-chain-1", Box::new(Cursor::new(bytes)), size, &digest)
            .expect("put");
        assert_eq!(target.list().expect("list").len(), 1);
        assert_eq!(
            store.keys().expect("keys"),
            vec!["tenant/acme/acme-chain-1.spotbak".to_owned()]
        );
        target.delete("acme-chain-1").expect("delete");
        assert!(target.list().expect("list").is_empty());
    }
}
