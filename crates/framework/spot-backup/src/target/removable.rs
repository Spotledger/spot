//! A removable target (USB drive): discovers its volume, refuses a target on the same
//! physical disk as the live data when configured to, and reports absent media as a
//! waiting state rather than a failure.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{BackupError, Result};
use crate::model::{StoredBackup, TargetStatus};
use crate::target::volume::Volume;
use crate::target::volume::VolumeProbe;
use crate::target::{
    BackupTarget, SpaceProbe, delete_package, list_packages, read_package, write_package_atomic,
};

/// A target over one volume id, below a subdirectory of its mount point.
pub struct RemovableTarget {
    probe: Arc<dyn VolumeProbe>,
    volume_id: String,
    subdir: String,
    data_disk: Option<String>,
    refuse_same_disk: bool,
    space: Option<Arc<dyn SpaceProbe>>,
}

impl RemovableTarget {
    /// A target over `volume_id`.
    ///
    /// `data_disk` is the physical disk that holds the live tenant data (for example
    /// `sda`); when `refuse_same_disk` is set, a volume backed by the same disk is
    /// refused. `space` overrides the volume's own free-space figure when supplied.
    pub fn new(
        probe: Arc<dyn VolumeProbe>,
        volume_id: impl Into<String>,
        subdir: impl Into<String>,
        data_disk: Option<String>,
        refuse_same_disk: bool,
        space: Option<Arc<dyn SpaceProbe>>,
    ) -> RemovableTarget {
        RemovableTarget {
            probe,
            volume_id: volume_id.into(),
            subdir: subdir.into(),
            data_disk,
            refuse_same_disk,
            space,
        }
    }

    /// The volume id this target expects.
    pub fn volume_id(&self) -> &str {
        &self.volume_id
    }

    /// Finds the volume, if it is present.
    fn volume(&self) -> Result<Option<Volume>> {
        Ok(self
            .probe
            .volumes()?
            .into_iter()
            .find(|volume| volume.id == self.volume_id && volume.present))
    }

    /// The directory to write into.
    fn target_dir(&self, volume: &Volume) -> PathBuf {
        volume.mount_point.join(&self.subdir)
    }

    fn missing(&self, reason: &str) -> BackupError {
        BackupError::TargetUnavailable {
            target: format!("removable:{}", self.volume_id),
            reason: reason.to_owned(),
        }
    }

    fn check_same_disk(&self, volume: &Volume) -> Result<()> {
        if self.refuse_same_disk
            && let Some(data_disk) = &self.data_disk
            && data_disk == &volume.physical_disk
        {
            return Err(BackupError::SamePhysicalDisk {
                target: format!("removable:{}", self.volume_id),
                disk: volume.physical_disk.clone(),
            });
        }
        Ok(())
    }

    /// The directory when the media is present, or the typed reason it is not.
    fn ready_dir(&self) -> Result<PathBuf> {
        let volume = self
            .volume()?
            .ok_or_else(|| self.missing("the volume is not inserted"))?;
        self.check_same_disk(&volume)?;
        Ok(self.target_dir(&volume))
    }
}

impl std::fmt::Debug for RemovableTarget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemovableTarget")
            .field("volume_id", &self.volume_id)
            .field("subdir", &self.subdir)
            .field("refuse_same_disk", &self.refuse_same_disk)
            .finish_non_exhaustive()
    }
}

impl BackupTarget for RemovableTarget {
    fn describe(&self) -> String {
        format!("removable:{}", self.volume_id)
    }

    fn status(&self) -> TargetStatus {
        match self.volume() {
            Err(error) => TargetStatus::Unavailable {
                reason: error.reason(),
            },
            Ok(None) => TargetStatus::WaitingForMedia {
                volume: self.volume_id.clone(),
            },
            Ok(Some(volume)) => {
                if self.refuse_same_disk
                    && self
                        .data_disk
                        .as_ref()
                        .is_some_and(|disk| disk == &volume.physical_disk)
                {
                    TargetStatus::Unavailable {
                        reason: format!(
                            "the volume shares the live data's physical disk {}",
                            volume.physical_disk
                        ),
                    }
                } else {
                    TargetStatus::Ready
                }
            }
        }
    }

    fn put(
        &self,
        id: &str,
        body: Box<dyn Read + Send>,
        size: u64,
        digest: &str,
    ) -> Result<StoredBackup> {
        let dir = self.ready_dir()?;
        write_package_atomic(&dir, id, body, size, digest)
    }

    fn get(&self, id: &str) -> Result<Box<dyn Read + Send>> {
        let dir = self.ready_dir()?;
        read_package(&dir, id)
    }

    fn list(&self) -> Result<Vec<StoredBackup>> {
        let dir = self.ready_dir()?;
        list_packages(&dir)
    }

    fn delete(&self, id: &str) -> Result<()> {
        let dir = self.ready_dir()?;
        delete_package(&dir, id)
    }

    fn free_space(&self) -> Result<u64> {
        let volume = self
            .volume()?
            .ok_or_else(|| self.missing("the volume is not inserted"))?;
        self.check_same_disk(&volume)?;
        if let Some(space) = &self.space {
            return space.free_bytes(Path::new(&volume.mount_point));
        }
        Ok(volume.free_space)
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::model::VerifyMode;
    use crate::package::{PackageEntry, build_from_files};
    use crate::target::FakeVolumeProbe;
    use crate::target::FixedSpaceProbe;
    use std::io::Cursor;

    fn volume(present: bool) -> Volume {
        Volume {
            id: "usb-1".to_owned(),
            label: "BACKUP".to_owned(),
            mount_point: "/media/backup".into(),
            removable: true,
            physical_disk: "sdb".to_owned(),
            present,
            free_space: 4_000,
        }
    }

    fn manifest() -> crate::manifest::BackupManifest {
        crate::manifest::BackupManifest {
            format: crate::manifest::MANIFEST_FORMAT.to_owned(),
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
            target: "removable:usb-1".to_owned(),
            encryption: None,
        }
    }

    #[test]
    fn absent_media_waits_then_appears() {
        let probe = Arc::new(FakeVolumeProbe::with(vec![volume(false)]));
        let target = RemovableTarget::new(
            probe.clone(),
            "usb-1",
            "spot",
            Some("sda".to_owned()),
            true,
            None,
        );
        assert_eq!(
            target.status(),
            TargetStatus::WaitingForMedia {
                volume: "usb-1".to_owned()
            }
        );
        assert_eq!(
            target.free_space().expect_err("absent").code(),
            "backup.target_unavailable"
        );
        probe.set_present("usb-1", true).expect("present");
        assert_eq!(target.status(), TargetStatus::Ready);
    }

    #[test]
    fn the_same_physical_disk_is_refused() {
        let probe = Arc::new(FakeVolumeProbe::with(vec![volume(true)]));
        let target =
            RemovableTarget::new(probe, "usb-1", "spot", Some("sdb".to_owned()), true, None);
        assert!(matches!(target.status(), TargetStatus::Unavailable { .. }));
        assert_eq!(
            target.free_space().expect_err("refused").code(),
            "backup.same_physical_disk"
        );
    }

    #[test]
    fn writes_into_the_mounted_volume() {
        let work = tempfile::tempdir().expect("work");
        let mount = work.path().join("mount");
        std::fs::create_dir_all(&mount).expect("mount");
        let mut mounted = volume(true);
        mounted.mount_point = mount;
        let probe = Arc::new(FakeVolumeProbe::with(vec![mounted]));
        let target = RemovableTarget::new(
            probe,
            "usb-1",
            "spot",
            Some("sda".to_owned()),
            true,
            Some(Arc::new(FixedSpaceProbe(9_000))),
        );
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
        assert_eq!(target.free_space().expect("space"), 9_000);
    }
}
