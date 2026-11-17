//! `services::backup_targets` (WP-PRD-04.T5): removable-drive detection against a fake device
//! table (no hardware), the cloud adapter against an in-process object store (the same
//! `ObjectStore` port the S3 transport implements), and the target configuration rules.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use spot_backup::package::{PackageEntry, build_from_files};
use spot_backup::testing::sample_manifest;
use spot_backup::{BackupTarget, CloudStore, TargetStatus};
use spot_cli::services::backup_targets::{
    BackupTargets, ChunkedCloudStore, CloudAccess, CloudConnector, Credential, CredentialSource,
    DeviceEntry, DeviceSnapshot, FakeDevices, SelectedVolume, TARGET_ABSENT, TARGET_AMBIGUOUS,
    TARGET_FOREIGN, TARGET_INVALID, TARGET_UNSAFE, TARGET_UNSUPPORTED, TargetError, TargetRegistry,
    TargetSpec, VolumeSelector, parse_logical_disks, parse_mountinfo, physical_disk_of,
    throttle_delay_ms, unescape_octal, unescape_udev,
};
use spot_cloud_backup::{
    CloudBackupError, MemoryObjectStore, ObjectInfo, ObjectStore, PutOutcome, RecordingStore,
    RecoveryKey, StoreFuture,
};

static SEQUENCE: AtomicUsize = AtomicUsize::new(0);

fn temp(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "spot_backup_t5_{label}_{}_{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn stick(mount: Option<&PathBuf>) -> DeviceEntry {
    DeviceEntry {
        device: "/dev/sdb1".to_owned(),
        uuid: Some("1234-ABCD".to_owned()),
        label: Some("SPOT-BACKUP".to_owned()),
        removable: true,
        physical_disk: "sdb".to_owned(),
        mount_point: mount.cloned(),
        free_bytes: 1 << 30,
    }
}

fn snapshot(devices: Vec<DeviceEntry>) -> DeviceSnapshot {
    DeviceSnapshot {
        devices,
        system_disks: vec!["sda".to_owned()],
    }
}

fn removable(volume: &str) -> TargetSpec {
    TargetSpec::Removable {
        volume: volume.to_owned(),
        subdir: None,
    }
}

fn registry(devices: Arc<FakeDevices>, spec: TargetSpec) -> TargetRegistry {
    let mut specs = BTreeMap::new();
    specs.insert("usb".to_owned(), spec);
    TargetRegistry::new(specs, devices)
}

/// A package of `tenant` built under `work`, with its stream size and digest.
fn package(work: &Path, id: &str, tenant: &str) -> (spot_backup::PackageFile, u64, String) {
    let built = build_from_files(
        sample_manifest(id, tenant, 7),
        vec![PackageEntry {
            path: "data/a.txt".to_owned(),
            bytes: b"hello backup".to_vec(),
        }],
        work,
    )
    .unwrap();
    let (size, digest) = built.package.stream_facts().unwrap();
    (built.package, size, digest)
}

fn put(target: &dyn BackupTarget, pkg: &spot_backup::PackageFile, id: &str, size: u64, d: &str) {
    target.put(id, pkg.reader().unwrap(), size, d).unwrap();
}

// ----- the volume selector ------------------------------------------------------------------

#[test]
fn a_volume_is_named_by_label_or_uuid_never_by_letter_or_path() {
    assert_eq!(
        VolumeSelector::parse("SPOT-BACKUP").unwrap(),
        VolumeSelector::Label("SPOT-BACKUP".into())
    );
    assert_eq!(
        VolumeSelector::parse("1234-abcd").unwrap(),
        VolumeSelector::Uuid("1234-ABCD".into())
    );
    assert_eq!(
        VolumeSelector::parse("2f1c2a64-3a9b-4c11-9d0e-0123456789ab").unwrap(),
        VolumeSelector::Uuid("2F1C2A64-3A9B-4C11-9D0E-0123456789AB".into())
    );
    // A bare 8-digit serial needs the prefix, so a label that happens to be hex stays a label.
    assert_eq!(
        VolumeSelector::parse("BADCAFE0").unwrap(),
        VolumeSelector::Label("BADCAFE0".into())
    );
    assert_eq!(
        VolumeSelector::parse("uuid:badcafe0").unwrap(),
        VolumeSelector::Uuid("BADCAFE0".into())
    );
    assert_eq!(
        VolumeSelector::parse("label:E-DRIVE").unwrap(),
        VolumeSelector::Label("E-DRIVE".into())
    );
    for bad in [
        "",
        "E:",
        "e:",
        "label:E:",
        "/mnt/usb",
        "D:\\backups",
        "uuid:not-hex!",
        "a\nb",
        "uuid:",
    ] {
        let error = VolumeSelector::parse(bad).unwrap_err();
        assert_eq!(error.code(), TARGET_INVALID, "{bad:?}");
    }
}

// ----- removable detection (fake devices, no hardware) -------------------------------------

#[test]
fn a_mounted_removable_volume_is_accepted_by_label_and_by_uuid() {
    let mount = temp("accept");
    let devices = Arc::new(FakeDevices::new(snapshot(vec![stick(Some(&mount))])));
    for volume in ["SPOT-BACKUP", "1234-ABCD", "uuid:1234-abcd"] {
        let selected = SelectedVolume::new(devices.clone(), VolumeSelector::parse(volume).unwrap());
        let found = selected.assess().unwrap();
        assert_eq!(found.volume.mount_point, mount);
        assert_eq!(found.device, "/dev/sdb1");
        assert_eq!(found.uuid.as_deref(), Some("1234-ABCD"));
    }
    let adapter = registry(devices, removable("SPOT-BACKUP"))
        .open("usb")
        .unwrap();
    assert_eq!(adapter.kind(), "removable");
    adapter.preflight().unwrap();
    assert!(matches!(adapter.target().status(), TargetStatus::Ready));
    assert!(adapter.eject_hint().unwrap().contains("/dev/sdb1"));
}

#[test]
fn an_unplugged_volume_is_absent_and_nothing_is_written_to_the_mount_directory() {
    let mount = temp("absent");
    let work = temp("absent_work");
    let (pkg, size, digest) = package(&work, "site-main-1", "acme");
    // The directory the stick used to be mounted on exists and is empty.
    let devices = Arc::new(FakeDevices::new(snapshot(Vec::new())));
    let adapter = registry(devices.clone(), removable("SPOT-BACKUP"))
        .open("usb")
        .unwrap();

    let error = adapter.preflight().unwrap_err();
    assert_eq!(error.code(), TARGET_ABSENT);
    assert!(matches!(
        adapter.target().status(),
        TargetStatus::WaitingForMedia { .. }
    ));
    assert!(
        adapter
            .target()
            .put("site-main-1", pkg.reader().unwrap(), size, &digest)
            .is_err()
    );
    assert!(adapter.eject_hint().is_none());

    // Plugged in but not mounted: still absent, still nothing written.
    devices.set(snapshot(vec![stick(None)]));
    assert_eq!(adapter.preflight().unwrap_err().code(), TARGET_ABSENT);
    assert!(
        adapter
            .target()
            .put("site-main-1", pkg.reader().unwrap(), size, &digest)
            .is_err()
    );
    assert_eq!(std::fs::read_dir(&mount).unwrap().count(), 0);
}

#[test]
fn a_volume_on_the_system_disk_or_not_removable_is_refused() {
    let mount = temp("unsafe");
    let work = temp("unsafe_work");
    let (pkg, size, digest) = package(&work, "site-main-1", "acme");

    let mut on_system = stick(Some(&mount));
    on_system.physical_disk = "sda".to_owned();
    let mut fixed = stick(Some(&mount));
    fixed.removable = false;
    for entry in [on_system, fixed] {
        let devices = Arc::new(FakeDevices::new(snapshot(vec![entry])));
        let adapter = registry(devices, removable("SPOT-BACKUP"))
            .open("usb")
            .unwrap();
        assert_eq!(adapter.preflight().unwrap_err().code(), TARGET_UNSAFE);
        assert!(matches!(
            adapter.target().status(),
            TargetStatus::Unavailable { .. }
        ));
        assert!(
            adapter
                .target()
                .put("site-main-1", pkg.reader().unwrap(), size, &digest)
                .is_err()
        );
    }
    assert_eq!(std::fs::read_dir(&mount).unwrap().count(), 0);
}

#[test]
fn two_volumes_with_one_label_are_refused_until_named_by_uuid() {
    let mount = temp("ambiguous");
    let mut other = stick(Some(&mount));
    other.device = "/dev/sdc1".to_owned();
    other.uuid = Some("FFFF-0000".to_owned());
    other.physical_disk = "sdc".to_owned();
    let devices = Arc::new(FakeDevices::new(snapshot(vec![stick(Some(&mount)), other])));

    let by_label = SelectedVolume::new(
        devices.clone(),
        VolumeSelector::parse("SPOT-BACKUP").unwrap(),
    );
    assert_eq!(by_label.assess().unwrap_err().code(), TARGET_AMBIGUOUS);
    let by_uuid = SelectedVolume::new(devices, VolumeSelector::parse("FFFF-0000").unwrap());
    assert_eq!(by_uuid.assess().unwrap().device, "/dev/sdc1");
}

#[test]
fn removing_the_stick_after_a_backup_makes_the_target_absent_again() {
    let mount = temp("replug");
    let work = temp("replug_work");
    let (pkg, size, digest) = package(&work, "site-main-1", "acme");
    let devices = Arc::new(FakeDevices::new(snapshot(vec![stick(Some(&mount))])));
    let adapter = registry(devices.clone(), removable("SPOT-BACKUP"))
        .open("usb")
        .unwrap();
    put(adapter.target(), &pkg, "site-main-1", size, &digest);
    assert_eq!(adapter.list().unwrap().len(), 1);
    devices.set(snapshot(Vec::new()));
    assert_eq!(adapter.list().unwrap_err().code(), TARGET_ABSENT);
}

// ----- restore through the adapter ----------------------------------------------------------

#[test]
fn restore_fetches_from_the_stick_and_refuses_another_tenants_package() {
    let mount = temp("fetch");
    let work = temp("fetch_work");
    let devices = Arc::new(FakeDevices::new(snapshot(vec![stick(Some(&mount))])));
    let adapter = registry(devices, removable("SPOT-BACKUP"))
        .open("usb")
        .unwrap();
    let (mine, size, digest) = package(&work, "acme-main-1", "acme");
    put(adapter.target(), &mine, "acme-main-1", size, &digest);
    let (foreign, size, digest) = package(&work, "other-main-1", "other");
    put(adapter.target(), &foreign, "other-main-1", size, &digest);

    let mut bytes = Vec::new();
    adapter
        .fetch("acme-main-1", "acme")
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes.len() as u64, mine.package_bytes());

    let refused = adapter.fetch("other-main-1", "acme").err().unwrap();
    assert_eq!(refused.code(), TARGET_FOREIGN);
    assert_eq!(
        adapter.fetch("missing-1", "acme").err().unwrap().code(),
        "backup.not_found"
    );
    assert!(adapter.fetch("../escape", "acme").is_err());
}

#[test]
fn a_damaged_package_on_the_stick_fails_its_digest_check() {
    let mount = temp("damaged");
    let work = temp("damaged_work");
    let devices = Arc::new(FakeDevices::new(snapshot(vec![stick(Some(&mount))])));
    let adapter = registry(devices, removable("SPOT-BACKUP"))
        .open("usb")
        .unwrap();
    let (pkg, size, digest) = package(&work, "acme-main-1", "acme");
    put(adapter.target(), &pkg, "acme-main-1", size, &digest);
    let file = mount.join("spot-backups").join("acme-main-1.spotbak");
    let mut raw = std::fs::read(&file).unwrap();
    let last = raw.len() - 1;
    raw[last] ^= 0xff;
    std::fs::write(&file, raw).unwrap();
    let error = adapter.fetch("acme-main-1", "acme").err().unwrap();
    assert_eq!(error.code(), "backup.verify_failed");
}

// ----- platform parsers (pure, run on Linux for both platforms) -----------------------------

#[test]
fn mountinfo_lines_are_parsed_and_unescaped() {
    let text = "\
36 25 8:17 / /run/media/alice/SPOT\\040BACKUP rw,nosuid shared:3 - vfat /dev/sdb1 rw,fmask=0022
25 1 259:2 / / rw shared:1 - btrfs /dev/nvme0n1p2 rw
garbage line
40 25 0:35 / /tmp rw - tmpfs tmpfs rw";
    let mounts = parse_mountinfo(text);
    assert_eq!(mounts.len(), 3);
    assert_eq!(mounts[0].source, "/dev/sdb1");
    assert_eq!(
        mounts[0].mount_point,
        PathBuf::from("/run/media/alice/SPOT BACKUP")
    );
    assert_eq!(mounts[0].fstype, "vfat");
    assert_eq!(mounts[1].mount_point, PathBuf::from("/"));
    assert_eq!(unescape_octal("a\\040b\\134c\\09"), "a b\\c\\09");
}

#[test]
fn udev_label_escapes_and_physical_disks_are_decoded() {
    assert_eq!(unescape_udev("SPOT\\x20BACKUP"), "SPOT BACKUP");
    assert_eq!(unescape_udev("plain"), "plain");
    assert_eq!(
        physical_disk_of(
            "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-1/1-1:1.0/host6/target6:0:0/6:0:0:0/block/sdb/sdb1",
            "sdb1"
        ),
        "sdb"
    );
    assert_eq!(physical_disk_of("/sys/devices/virtual/misc/x", "x"), "x");
}

#[test]
fn windows_volumes_use_drive_type_two_as_removable() {
    let text = "C:|3|Windows|AAAA1111|1000\nE:|2|SPOT-BACKUP|1A2B3C4D|2048\nD:|5||0|0\nnot|a|line\nF:|x|bad|1|1\n";
    let table = parse_logical_disks(text, "c:");
    assert_eq!(table.system_disks, vec!["C:".to_owned()]);
    assert_eq!(table.devices.len(), 3);
    let e = table.devices.iter().find(|d| d.device == "E:").unwrap();
    assert!(e.removable);
    assert_eq!(e.label.as_deref(), Some("SPOT-BACKUP"));
    assert_eq!(e.uuid.as_deref(), Some("1A2B3C4D"));
    assert_eq!(e.free_bytes, 2048);
    assert_eq!(e.mount_point.as_deref(), Some(Path::new("E:\\")));
    let c = table.devices.iter().find(|d| d.device == "C:").unwrap();
    assert!(!c.removable);

    // Through the rules: the stick is accepted by serial, the system drive is refused.
    let devices = Arc::new(FakeDevices::new(table));
    let by_serial = SelectedVolume::new(
        devices.clone(),
        VolumeSelector::parse("uuid:1a2b3c4d").unwrap(),
    );
    assert_eq!(by_serial.assess().unwrap().device, "E:");
    let by_label = SelectedVolume::new(devices, VolumeSelector::parse("Windows").unwrap());
    assert_eq!(by_label.assess().unwrap_err().code(), TARGET_UNSAFE);
}

// ----- configuration ------------------------------------------------------------------------

#[test]
fn target_specs_are_read_from_toml_and_checked_without_echoing_secrets() {
    let table: BTreeMap<String, TargetSpec> = toml::from_str(
        r#"
[usb]
kind = "removable"
volume = "SPOT-BACKUP"

[offsite]
kind = "cloud"
endpoint = "https://s3.example.com"
key_id = "AKIA-EXAMPLE"
credential_ref = "secret:backup-s3"
prefix = "acme"
max_kbps = 512
"#,
    )
    .unwrap();
    assert!(table["usb"].problems().is_empty());
    assert!(table["offsite"].problems().is_empty());

    assert!(toml::from_str::<BTreeMap<String, TargetSpec>>(
        "[x]\nkind = \"cloud\"\nendpoint = \"https://h\"\nkey_id = \"k\"\ncredential_ref = \"secret:s\"\ninsecure = true\n"
    )
    .is_err());

    let cloud = |endpoint: &str, credential_ref: &str, max_kbps: Option<u32>| TargetSpec::Cloud {
        endpoint: endpoint.into(),
        key_id: "k".into(),
        credential_ref: credential_ref.into(),
        prefix: None,
        max_kbps,
        quota_bytes: None,
    };
    assert!(
        !cloud("http://s3.example.com", "secret:s", None)
            .problems()
            .is_empty()
    );
    assert!(
        !cloud("https://user:pw@s3.example.com", "secret:s", None)
            .problems()
            .is_empty()
    );
    assert!(
        !cloud("https://s3.example.com?x=1", "secret:s", None)
            .problems()
            .is_empty()
    );
    let inline = cloud("https://s3.example.com", "wJalrXUtnFEMI/K7MDENG", None).problems();
    assert!(!inline.is_empty());
    assert!(inline.iter().all(|p| !p.contains("wJalr")));
    assert!(
        !cloud("https://s3.example.com", "secret:s", Some(0))
            .problems()
            .is_empty()
    );
    assert!(!removable("E:").problems().is_empty());
    assert!(
        !TargetSpec::Removable {
            volume: "X".into(),
            subdir: Some("../up".into())
        }
        .problems()
        .is_empty()
    );
    let devices = Arc::new(FakeDevices::default());
    let error = registry(devices, removable("/mnt/usb"))
        .open("usb")
        .err()
        .unwrap();
    assert_eq!(error.code(), TARGET_INVALID);
    assert_eq!(
        registry(Arc::new(FakeDevices::default()), removable("X"))
            .open("nope")
            .err()
            .unwrap()
            .code(),
        "backup.not_found"
    );
}

// ----- cloud ---------------------------------------------------------------------------------

const SECRET: &str = "TOPSECRET-ACCESS-KEY-123";

struct Credentials;
impl CredentialSource for Credentials {
    fn credential(&self, reference: &str) -> Result<Credential, TargetError> {
        if reference == "secret:backup-s3" {
            Ok(Credential::new(SECRET.to_owned()))
        } else {
            Err(TargetError::new(
                "backup.encryption_unavailable",
                "the secret is not in the store",
            ))
        }
    }
}

struct Connector(Arc<dyn ObjectStore>);
impl CloudConnector for Connector {
    fn connect(
        &self,
        _endpoint: &str,
        _key_id: &str,
        credential: &Credential,
    ) -> Result<Arc<dyn ObjectStore>, TargetError> {
        assert_eq!(credential.expose(), SECRET);
        Ok(self.0.clone())
    }
}

fn cloud_spec(max_kbps: Option<u32>, quota_bytes: Option<u64>) -> TargetSpec {
    TargetSpec::Cloud {
        endpoint: "https://s3.example.com".into(),
        key_id: "AKIA-EXAMPLE".into(),
        credential_ref: "secret:backup-s3".into(),
        prefix: Some("acme".into()),
        max_kbps,
        quota_bytes,
    }
}

fn cloud_registry(
    store: Arc<dyn ObjectStore>,
    key: RecoveryKey,
    spec: TargetSpec,
) -> TargetRegistry {
    let mut specs = BTreeMap::new();
    specs.insert("offsite".to_owned(), spec);
    TargetRegistry::new(specs, Arc::new(FakeDevices::default())).with_cloud(CloudAccess {
        credentials: Arc::new(Credentials),
        connector: Arc::new(Connector(store)),
        tenant_key: key,
    })
}

#[test]
fn a_cloud_target_round_trips_and_uploads_only_ciphertext() {
    let work = temp("cloud");
    let recording = Arc::new(RecordingStore::new());
    let key = RecoveryKey::generate().unwrap();
    let adapter = cloud_registry(recording.clone(), key, cloud_spec(None, None))
        .open("offsite")
        .unwrap();
    assert_eq!(adapter.kind(), "cloud");
    adapter.preflight().unwrap();

    let (pkg, size, digest) = package(&work, "acme-main-1", "acme");
    put(adapter.target(), &pkg, "acme-main-1", size, &digest);

    let listed = adapter.list().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, "acme-main-1");
    assert_eq!(listed[0].manifest.tenant, "acme");

    let mut back = Vec::new();
    adapter
        .fetch("acme-main-1", "acme")
        .unwrap()
        .read_to_end(&mut back)
        .unwrap();
    assert_eq!(back, std::fs::read(pkg.path()).unwrap());
    assert_eq!(
        adapter.fetch("acme-main-1", "other").err().unwrap().code(),
        TARGET_FOREIGN
    );

    // What crossed the port: no plaintext, no tenant name, and the objects sit under the prefix.
    let plain = std::fs::read(pkg.path()).unwrap();
    for (key, bytes) in recording.puts() {
        assert!(key.starts_with("acme/acme-main-1.spotbak/"), "{key}");
        assert!(!bytes.windows(12).any(|w| w == b"hello backup"));
        assert!(
            !bytes
                .windows(plain.len().min(16))
                .any(|w| w == &plain[..plain.len().min(16)])
        );
    }
}

/// An object store that fails its `fail_at`-th put (counting from 1) once, to kill an upload.
struct Flaky {
    inner: MemoryObjectStore,
    puts: AtomicUsize,
    fail_at: AtomicUsize,
}

impl ObjectStore for Flaky {
    fn put<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, PutOutcome> {
        Box::pin(async move {
            let n = self.puts.fetch_add(1, Ordering::SeqCst) + 1;
            if n == self.fail_at.load(Ordering::SeqCst) {
                return Err(CloudBackupError::Store {
                    reason: "connection reset".to_owned(),
                });
            }
            self.inner.put(key, bytes).await
        })
    }
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Vec<u8>> {
        self.inner.get(key)
    }
    fn list<'a>(&'a self, prefix: &'a str) -> StoreFuture<'a, Vec<ObjectInfo>> {
        self.inner.list(prefix)
    }
    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        self.inner.delete(key)
    }
}

#[test]
fn a_killed_upload_resumes_without_resending_finished_chunks_and_is_never_listed_half_done() {
    let flaky = Arc::new(Flaky {
        inner: MemoryObjectStore::new(),
        puts: AtomicUsize::new(0),
        fail_at: AtomicUsize::new(4),
    });
    let key = RecoveryKey::generate().unwrap();
    let store = ChunkedCloudStore::new(flaky.clone(), key, "acme", None, None).with_chunk_bytes(8);
    let data: Vec<u8> = (0..60u8).collect(); // 8 chunks of 8 bytes (the last is 4)

    assert!(store.put_object("acme/pkg.spotbak", &data).is_err());
    // Three chunks landed; the package has no manifest, so it does not exist for `list`.
    assert!(store.list_objects("acme/").unwrap().is_empty());
    assert!(store.get_object("acme/pkg.spotbak").is_err());
    assert_eq!(flaky.inner.keys().len(), 3);

    let before = flaky.puts.load(Ordering::SeqCst);
    store.put_object("acme/pkg.spotbak", &data).unwrap();
    let sent = flaky.puts.load(Ordering::SeqCst) - before;
    // 8 chunks + the manifest, minus the 3 finished ones.
    assert_eq!(sent, 8 + 1 - 3);
    assert_eq!(
        store.list_objects("acme/").unwrap(),
        vec!["acme/pkg.spotbak".to_owned()]
    );
    assert_eq!(store.get_object("acme/pkg.spotbak").unwrap(), data);
}

#[test]
fn a_changed_chunk_and_a_wrong_key_are_detected() {
    let backing = Arc::new(MemoryObjectStore::new());
    let key = RecoveryKey::generate().unwrap();
    let store = ChunkedCloudStore::new(backing.clone(), key.clone(), "acme", None, None)
        .with_chunk_bytes(8);
    let data: Vec<u8> = (0..40u8).collect();
    store.put_object("acme/pkg.spotbak", &data).unwrap();

    // Another tenant key cannot open the manifest, let alone a chunk.
    let intruder = ChunkedCloudStore::new(
        backing.clone(),
        RecoveryKey::generate().unwrap(),
        "acme",
        None,
        None,
    );
    assert_eq!(
        intruder.get_object("acme/pkg.spotbak").unwrap_err().code(),
        "backup.decrypt_failed"
    );

    // A chunk replaced by garbage of the same key: the recorded digest no longer matches.
    let chunk = "acme/pkg.spotbak/c00000002";
    run_future(async {
        backing.delete(chunk).await.unwrap();
        backing.put(chunk, vec![7u8; 60]).await.unwrap();
    });
    let error = store.get_object("acme/pkg.spotbak").unwrap_err();
    assert_eq!(error.code(), "backup.verify_failed");

    // A chunk swapped with another chunk of the same package fails its digest too.
    run_future(async {
        let other = backing.get("acme/pkg.spotbak/c00000001").await.unwrap();
        backing.delete(chunk).await.unwrap();
        backing.put(chunk, other).await.unwrap();
    });
    assert!(store.get_object("acme/pkg.spotbak").is_err());
}

/// Runs a future on a throwaway runtime (the test itself is synchronous).
fn run_future<F: std::future::Future<Output = ()>>(future: F) {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(future);
}

#[test]
fn deleting_a_cloud_backup_removes_the_manifest_and_every_chunk() {
    let backing = Arc::new(MemoryObjectStore::new());
    let store = ChunkedCloudStore::new(
        backing.clone(),
        RecoveryKey::generate().unwrap(),
        "acme",
        None,
        None,
    )
    .with_chunk_bytes(8);
    store.put_object("acme/a.spotbak", &[1u8; 20]).unwrap();
    store.put_object("acme/b.spotbak", &[2u8; 20]).unwrap();
    store.delete_object("acme/a.spotbak").unwrap();
    assert_eq!(
        store.list_objects("acme/").unwrap(),
        vec!["acme/b.spotbak".to_owned()]
    );
    assert!(
        backing
            .keys()
            .iter()
            .all(|k| k.starts_with("acme/b.spotbak/"))
    );
    assert_eq!(
        store.delete_object("acme/a.spotbak").unwrap_err().code(),
        "backup.not_found"
    );
}

#[test]
fn bandwidth_and_quota_are_integer_rules() {
    // 1 MiB at 1024 KiB/s takes one second; at 256 KiB/s four.
    assert_eq!(throttle_delay_ms(1024 * 1024, 1024), 1000);
    assert_eq!(throttle_delay_ms(1024 * 1024, 256), 4000);
    assert_eq!(throttle_delay_ms(0, 256), 0);
    assert_eq!(throttle_delay_ms(u64::MAX, 1), u64::MAX / 1024);

    let backing = Arc::new(MemoryObjectStore::new());
    let store = ChunkedCloudStore::new(
        backing,
        RecoveryKey::generate().unwrap(),
        "acme",
        None,
        Some(10_000),
    )
    .with_chunk_bytes(1024);
    let before = store.remaining_bytes().unwrap();
    assert_eq!(before, 10_000);
    store.put_object("acme/a.spotbak", &[9u8; 2048]).unwrap();
    let after = store.remaining_bytes().unwrap();
    assert!(after < before && after > 0);
    let unlimited = ChunkedCloudStore::new(
        Arc::new(MemoryObjectStore::new()),
        RecoveryKey::generate().unwrap(),
        "x",
        None,
        None,
    );
    assert_eq!(unlimited.remaining_bytes().unwrap(), u64::MAX);
}

#[test]
fn a_throttled_upload_still_completes() {
    let store = ChunkedCloudStore::new(
        Arc::new(MemoryObjectStore::new()),
        RecoveryKey::generate().unwrap(),
        "acme",
        Some(1_000_000), // about a thousandth of a second per KiB
        None,
    )
    .with_chunk_bytes(64);
    let data = vec![3u8; 200];
    store.put_object("acme/t.spotbak", &data).unwrap();
    assert_eq!(store.get_object("acme/t.spotbak").unwrap(), data);
}

#[test]
fn cloud_access_is_refused_without_a_connector_and_a_credential_never_appears_in_text() {
    // No cloud access configured on the host.
    let mut specs = BTreeMap::new();
    specs.insert("offsite".to_owned(), cloud_spec(None, None));
    let bare = TargetRegistry::new(specs, Arc::new(FakeDevices::default()));
    let error = bare.open("offsite").err().unwrap();
    assert_eq!(error.code(), TARGET_UNSUPPORTED);

    // A missing secret is reported by reference, not by value.
    let mut spec = cloud_spec(None, None);
    if let TargetSpec::Cloud { credential_ref, .. } = &mut spec {
        *credential_ref = "secret:missing".to_owned();
    }
    let registry = cloud_registry(
        Arc::new(MemoryObjectStore::new()),
        RecoveryKey::generate().unwrap(),
        spec,
    );
    let error = registry.open("offsite").err().unwrap();
    assert_eq!(error.code(), "backup.encryption_unavailable");

    // Nothing printable carries the secret or the key.
    let credential = Credential::new(SECRET.to_owned());
    assert!(!format!("{credential:?}").contains(SECRET));
    let key = RecoveryKey::generate().unwrap();
    let store = ChunkedCloudStore::new(
        Arc::new(MemoryObjectStore::new()),
        key.clone(),
        "acme",
        None,
        None,
    );
    let shown = format!("{store:?}");
    assert!(!shown.contains(&format!("{:?}", key.expose())));
    let text = format!("{error} {error:?} {shown}");
    assert!(!text.contains(SECRET));
}
