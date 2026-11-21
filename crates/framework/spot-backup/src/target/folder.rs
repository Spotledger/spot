//! A backup folder: a fixed disk, a network share, or the folder on a removable
//! volume after the platform mounted it. Writes are temp + fsync + atomic rename.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{BackupError, Result};
use crate::model::{StoredBackup, TargetStatus};
use crate::target::{
    BackupTarget, SpaceProbe, delete_package, folder_free_space, list_packages, read_package,
    write_package_atomic,
};

/// A directory-backed target. The directory is created by `put`; `list` on a missing
/// directory is empty rather than an error, so a first run starts cleanly.
pub struct FolderTarget {
    base: PathBuf,
    label: String,
    space: Arc<dyn SpaceProbe>,
}

impl FolderTarget {
    /// A target over `base`, probing free space with `space`.
    pub fn new(base: impl Into<PathBuf>, space: Arc<dyn SpaceProbe>) -> FolderTarget {
        let base = base.into();
        let label = format!("folder:{}", base.display());
        FolderTarget { base, label, space }
    }

    /// The directory.
    pub fn base(&self) -> &Path {
        &self.base
    }
}

impl std::fmt::Debug for FolderTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FolderTarget")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

impl BackupTarget for FolderTarget {
    fn describe(&self) -> String {
        self.label.clone()
    }

    fn status(&self) -> TargetStatus {
        TargetStatus::Ready
    }

    fn put(
        &self,
        id: &str,
        body: Box<dyn Read + Send>,
        size: u64,
        digest: &str,
    ) -> Result<StoredBackup> {
        write_package_atomic(&self.base, id, body, size, digest)
    }

    fn get(&self, id: &str) -> Result<Box<dyn Read + Send>> {
        read_package(&self.base, id)
    }

    fn list(&self) -> Result<Vec<StoredBackup>> {
        list_packages(&self.base)
    }

    fn delete(&self, id: &str) -> Result<()> {
        delete_package(&self.base, id)
    }

    fn free_space(&self) -> Result<u64> {
        folder_free_space(&self.base, &self.space).map_err(|error| match error {
            BackupError::Unsupported { .. } => BackupError::Unsupported {
                feature: "free-space probing for a folder target".to_owned(),
            },
            other => other,
        })
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::VerifyMode;
    use crate::package::{PackageEntry, build_from_files};
    use crate::target::FixedSpaceProbe;
    use std::io::Cursor;

    fn manifest(id: &str, created: &str) -> crate::manifest::BackupManifest {
        crate::manifest::BackupManifest {
            format: crate::manifest::MANIFEST_FORMAT.to_owned(),
            id: id.to_owned(),
            tenant: "acme".to_owned(),
            created_at: created.to_owned(),
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
            target: "folder:/backups".to_owned(),
            encryption: None,
        }
    }

    #[test]
    fn writes_lists_and_deletes_atomically() {
        let work = tempfile::tempdir().expect("work");
        let target = FolderTarget::new(
            work.path().join("backups"),
            Arc::new(FixedSpaceProbe(1 << 30)),
        );
        let built = build_from_files(
            manifest("acme-chain-1", "2026-10-19T12:00:00Z"),
            vec![PackageEntry {
                path: "data".to_owned(),
                bytes: b"payload".to_vec(),
            }],
            work.path(),
        )
        .expect("build");
        let bytes = std::fs::read(built.package.path()).expect("read package");
        let (size, digest) = built.package.stream_facts().expect("facts");
        let stored = target
            .put("acme-chain-1", Box::new(Cursor::new(bytes)), size, &digest)
            .expect("put");
        assert_eq!(stored.id, "acme-chain-1");
        assert_eq!(target.list().expect("list").len(), 1);
        assert_eq!(target.free_space().expect("space"), 1 << 30);
        target.delete("acme-chain-1").expect("delete");
        assert!(target.list().expect("list").is_empty());
    }

    #[test]
    fn an_interrupted_write_is_not_listed() {
        let work = tempfile::tempdir().expect("work");
        let target = FolderTarget::new(work.path(), Arc::new(FixedSpaceProbe(1 << 30)));
        struct Interrupt;
        impl Read for Interrupt {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("the USB drive was unplugged"))
            }
        }
        let error = target
            .put("acme-chain-1", Box::new(Interrupt), 100, &"a".repeat(64))
            .expect_err("interrupted");
        assert_eq!(error.code(), "backup.io");
        assert!(target.list().expect("list").is_empty());
    }

    #[test]
    fn a_digest_mismatch_is_refused_and_not_listed() {
        let work = tempfile::tempdir().expect("work");
        let target = FolderTarget::new(work.path(), Arc::new(FixedSpaceProbe(1 << 30)));
        let error = target
            .put(
                "acme-chain-1",
                Box::new(Cursor::new(b"abc".to_vec())),
                3,
                &"0".repeat(64),
            )
            .expect_err("bad digest");
        assert_eq!(error.code(), "backup.verify_failed");
        assert!(target.list().expect("list").is_empty());
    }
}
