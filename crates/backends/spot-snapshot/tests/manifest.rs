//! Unit tests: the manifest's canonical rules — UUIDv7 id, major-version refusal,
//! path safety, byte sums, the `.partial` staging rule, atomic publish, and the
//! changed-byte verification that names the file.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;

use spot_snapshot::error::SnapshotError;
use spot_snapshot::hash::{FileChecksum, Inventory};
use spot_snapshot::manifest::{Consistency, Snapshot, SnapshotManifest, staging_of};

fn checksum(text: &str) -> FileChecksum {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    FileChecksum {
        sha256: hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        bytes: text.len() as u64,
    }
}

fn manifest(files: Inventory) -> SnapshotManifest {
    let total_bytes = files.values().map(|f| f.bytes).sum();
    SnapshotManifest {
        id: uuid::Uuid::now_v7().to_string(),
        tenant: "tenant-a".to_owned(),
        provider: "spot-snapshot".to_owned(),
        provider_version: "0.1.0".to_owned(),
        provider_kind: "pgdump".to_owned(),
        pg_major: 18,
        catalogue_version: "1".to_owned(),
        feed_schema: "1".to_owned(),
        outbox_generation: 3,
        outbox_last_published_seq: 42,
        started_at: "2026-10-09T10:00:00Z".to_owned(),
        finished_at: "2026-10-09T10:00:05Z".to_owned(),
        consistency: Consistency::OnlineConsistent {
            xmin: 700,
            epoch: 0,
        },
        total_bytes,
        file_count: files.len() as u64,
        files,
        node_local: BTreeMap::new(),
        platform_extra: BTreeMap::new(),
    }
}

fn one_file_inventory() -> Inventory {
    let mut files = Inventory::new();
    files.insert("dump/2187.dat.gz".to_owned(), checksum("dumped data"));
    files
}

/// The manifest round trips through its own TOML.
#[test]
fn manifest_round_trips_through_toml() {
    let original = manifest(one_file_inventory());
    let text = original.to_toml().unwrap();
    let parsed: SnapshotManifest = toml::from_str(&text).unwrap();
    assert_eq!(parsed.id, original.id);
    assert_eq!(parsed.tenant, "tenant-a");
    assert_eq!(parsed.outbox_generation, 3);
    assert_eq!(parsed.outbox_last_published_seq, 42);
    assert_eq!(
        parsed.consistency,
        Consistency::OnlineConsistent {
            xmin: 700,
            epoch: 0
        }
    );
    assert_eq!(parsed.files.len(), 1);
}

/// The canonical checks refuse a non-UUIDv7 id.
#[test]
fn refuses_a_non_uuidv7_id() {
    let mut bad = manifest(one_file_inventory());
    bad.id = "not-a-uuid".to_owned();
    assert!(bad.canonical_checks().is_err());
    let mut v4 = bad.clone();
    v4.id = uuid::Uuid::now_v7().simple().to_string();
    // A UUIDv1-style string also fails the v7 check; simplest non-v7: all zeros.
    v4.id = "00000000-0000-0000-0000-000000000000".to_owned();
    assert!(v4.canonical_checks().is_err(), "a nil uuid is refused");
}

/// The canonical checks refuse a major version other than 18 — with the coded error.
#[test]
fn refuses_a_foreign_major_version() {
    let mut bad = manifest(one_file_inventory());
    bad.pg_major = 16;
    let error = bad.canonical_checks().unwrap_err();
    assert_eq!(error.code(), "snapshot.version_mismatch");
    let SnapshotError::VersionMismatch {
        snapshot_major,
        target_major: _,
    } = error
    else {
        panic!("wrong variant");
    };
    assert_eq!(snapshot_major, 16);
    // The coded remedy (catalogue/snapshot.toml) names the logical-restore
    // possibility; the catalogue test proves its shape, this test the code path.
}

/// The canonical checks refuse unsafe paths and sum mismatches.
#[test]
fn refuses_unsafe_paths_and_sum_mismatches() {
    let mut unsafe_paths = manifest(one_file_inventory());
    unsafe_paths
        .files
        .insert("../escape".to_owned(), checksum("escaped"));
    let error = unsafe_paths.canonical_checks().unwrap_err();
    assert_eq!(error.code(), "snapshot.verify_failed");
    assert!(error.to_string().contains("../escape"));

    let mut wrong_total = manifest(one_file_inventory());
    wrong_total.total_bytes += 1;
    assert!(wrong_total.canonical_checks().is_err());

    let mut wrong_count = manifest(one_file_inventory());
    wrong_count.file_count += 1;
    assert!(wrong_count.canonical_checks().is_err());
}

/// `Snapshot::open` refuses a directory with the `.partial` suffix.
#[tokio::test]
async fn open_refuses_a_partial_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let staged = tmp.path().join("snap.partial");
    std::fs::create_dir_all(&staged).unwrap();
    std::fs::write(staged.join("snapshot.toml"), "garbage").unwrap();
    let error = Snapshot::open(&staged).await.unwrap_err();
    assert!(error.to_string().contains("staged"), "{error}");
}

/// `Snapshot::open` succeeds after publish: the staging suffix is gone, the manifest
/// parses, and `verify` checks the recorded file.
#[tokio::test]
async fn publish_then_open_then_verify() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("snap");
    let staged = staging_of(&dir);
    std::fs::create_dir_all(staged.join("dump")).unwrap();
    std::fs::write(staged.join("dump").join("2187.dat.gz"), "dumped data").unwrap();
    manifest(one_file_inventory()).publish(&dir).await.unwrap();
    // The staging dir was renamed into place.
    assert!(!staged_of(&dir).exists(), "the staging neighbour is gone");
    let opened = Snapshot::open(&dir).await.unwrap();
    assert_eq!(opened.manifest().tenant, "tenant-a");
    let report = opened.verify().await.unwrap();
    assert_eq!(report.files_checked, 1);
    assert_eq!(report.bytes_checked, 11);
}

fn staged_of(dir: &Path) -> std::path::PathBuf {
    let mut name = dir.file_name().unwrap().to_string_lossy().into_owned();
    name.push_str(".partial");
    dir.parent().unwrap().join(name)
}

/// A flipped byte fails verify naming the file.
#[tokio::test]
async fn flipped_byte_fails_verify_naming_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("snap");
    let staged = staging_of(&dir);
    std::fs::create_dir_all(staged.join("dump")).unwrap();
    std::fs::write(staged.join("dump").join("2187.dat.gz"), "original bytes").unwrap();
    let mut files = one_file_inventory();
    files.insert("dump/2187.dat.gz".to_owned(), checksum("original bytes"));
    manifest(files).publish(&dir).await.unwrap();
    let opened = Snapshot::open(&dir).await.unwrap();
    std::fs::write(dir.join("dump").join("2187.dat.gz"), "original bytez").unwrap();
    let error = opened.verify().await.unwrap_err();
    assert_eq!(error.code(), "snapshot.verify_failed");
    assert!(error.to_string().contains("dump/2187.dat.gz"), "{error}");
}

/// An extra file the manifest does not name is refused.
#[tokio::test]
async fn extra_file_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("snap");
    let staged = staging_of(&dir);
    std::fs::create_dir_all(staged.join("dump")).unwrap();
    std::fs::write(staged.join("dump").join("2187.dat.gz"), "dumped data").unwrap();
    manifest(one_file_inventory()).publish(&dir).await.unwrap();
    std::fs::write(dir.join("dump").join("extra.bin"), "sneaked in").unwrap();
    let error = Snapshot::open(&dir)
        .await
        .unwrap()
        .verify()
        .await
        .unwrap_err();
    assert!(error.to_string().contains("extra.bin"), "{error}");
}

/// Instants round trip and a started-after-finished manifest is refused.
#[test]
fn instants_round_trip_and_order_is_checked() {
    let parsed =
        spot_snapshot::manifest::SnapshotManifest::parse_instant("2026-10-09T10:00:00Z").unwrap();
    assert_eq!(
        SnapshotManifest::format_instant(parsed),
        "2026-10-09T10:00:00Z"
    );
    let mut bad = manifest(one_file_inventory());
    bad.started_at = "2026-10-09T10:00:06Z".to_owned();
    bad.finished_at = "2026-10-09T10:00:05Z".to_owned();
    assert!(bad.canonical_checks().is_err());
}

/// A destination that already holds files is refused; a fresh one is accepted.
#[tokio::test]
async fn destination_must_be_empty() {
    let tmp = tempfile::tempdir().unwrap();
    let empty = tmp.path().join("empty");
    spot_snapshot::manifest::check_destination(&spot_snapshot::manifest::DestinationPath(
        empty.clone(),
    ))
    .await
    .unwrap();
    std::fs::write(&empty, "occupied").unwrap();
    let error = spot_snapshot::manifest::check_destination(
        &spot_snapshot::manifest::DestinationPath(empty.clone()),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("already holds")
            || error.to_string().contains("not a directory"),
        "{error}"
    );
}

/// The `.partial` staging neighbour blocks a second take.
#[tokio::test]
async fn partial_neighbour_blocks_a_take() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("snap");
    std::fs::create_dir_all(dir.with_extension("partial")).unwrap();
    let error =
        spot_snapshot::manifest::check_destination(&spot_snapshot::manifest::DestinationPath(dir))
            .await
            .unwrap_err();
    assert!(error.to_string().contains("staged"), "{error}");
}

/// The file-checksum helper hashes deterministically.
#[test]
fn checksum_helper_is_deterministic() {
    assert_eq!(checksum("abc").sha256, checksum("abc").sha256);
    assert_ne!(checksum("abc").sha256, checksum("abd").sha256);
    let mut files = Inventory::new();
    files.insert("dump/x".to_owned(), checksum("abc"));
    let built = manifest(files);
    assert_eq!(built.total_bytes, 3);
}
