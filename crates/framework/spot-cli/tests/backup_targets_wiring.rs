//! WP-PRD-04.T13: the removable and cloud targets in the `[backup]` table and in `spot backup`'s
//! target selection. The configuration cases and the selection over a scripted device table need no
//! database and no hardware; a full `spot backup run` to a removable drive is not run here because a
//! test cannot mount one (the library target is exercised by `backup_targets.rs`).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]
#![cfg(unix)]

/// The command registry, as `tests/backup.rs` includes it.
#[path = "../src/commands/mod.rs"]
mod commands;

use std::path::PathBuf;
use std::sync::Arc;

use commands::backup::target::{Refusal, resolve};
use spot_cli::config::{BackupTarget, load};
use spot_cli::services::backup_targets::{DeviceEntry, DeviceSnapshot, FakeDevices, TARGET_ABSENT};

fn load_with(tag: &str, extra: &str) -> Result<spot_cli::config::Loaded, String> {
    let root = std::env::temp_dir().join(format!(
        "spot_cli_t13_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("apps/books/doctypes")).unwrap();
    std::fs::write(
        root.join("common.toml"),
        format!(
            "apps = [{:?}]\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\nadmin_url = 'postgres://u:p@127.0.0.1:15432/postgres'\n{extra}\n",
            root.join("apps/books").display().to_string()
        ),
    )
    .unwrap();
    let out = load(&root, false).map_err(|e| e.to_string());
    std::fs::remove_dir_all(root).unwrap();
    out
}

fn refused(tag: &str, extra: &str) -> String {
    match load_with(tag, extra) {
        Ok(_) => panic!("expected {tag} to be refused"),
        Err(e) => e,
    }
}

#[test]
fn each_kind_parses_and_keeps_its_keys() {
    let extra = "[backup.targets.usb]\nkind = 'removable'\nvolume = 'label:BACKUP'\nsubdir = 'spot'\n\
                 [backup.targets.cloud]\nkind = 'cloud'\nendpoint = 'https://s3.example.test'\nkey_id = 'k1'\n\
                 credential_ref = 'secret:backup-key'\nprefix = 'acme'\nmax_kbps = 512\nquota_bytes = 1000000\n\
                 [backup.targets.main]\nkind = 'folder'\npath = '/srv/backups'\n";
    let loaded = load_with("kinds", extra).unwrap();
    let targets = &loaded.common.backup.targets;
    assert!(matches!(
        targets.get("usb"),
        Some(BackupTarget::Removable { volume, subdir }) if volume == "label:BACKUP" && subdir.as_deref() == Some("spot")
    ));
    assert!(matches!(
        targets.get("cloud"),
        Some(BackupTarget::Cloud { credential_ref: Some(r), max_kbps: Some(512), .. }) if r == "secret:backup-key"
    ));
    assert!(matches!(
        targets.get("main"),
        Some(BackupTarget::Folder { .. })
    ));
}

#[test]
fn unknown_keys_drive_letters_and_an_insecure_option_are_refused() {
    let usb = |body: &str| format!("[backup.targets.usb]\nkind = 'removable'\n{body}\n");
    let unknown = refused("unknown", &usb("volume = 'label:X'\nformat = true"));
    assert!(
        unknown.contains("format") || unknown.contains("backup"),
        "{unknown}"
    );
    for (tag, volume) in [("letter", "E:"), ("path", "/mnt/usb"), ("empty", "")] {
        let message = refused(tag, &usb(&format!("volume = '{volume}'")));
        assert!(message.contains("backup.targets.usb"), "{tag}: {message}");
    }
    let cloud = "[backup.targets.c]\nkind = 'cloud'\nendpoint = 'https://s3.example.test'\nkey_id = 'k'\ninsecure = true\n";
    let message = refused("insecure", cloud);
    assert!(!message.is_empty(), "an insecure option is not accepted");
}

#[test]
fn a_cloud_credential_must_be_a_reference_never_the_secret() {
    let cloud = |credential: &str| {
        format!(
            "[backup.targets.c]\nkind = 'cloud'\nendpoint = 'https://s3.example.test'\nkey_id = 'k'\ncredential_ref = '{credential}'\n"
        )
    };
    let message = refused("secret", &cloud("hunter2-real-secret"));
    assert!(message.contains("credential_ref"), "{message}");
    assert!(
        !message.contains("hunter2"),
        "no secret in a diagnostic: {message}"
    );
    assert!(load_with("ref-ok", &cloud("secret:backup-key")).is_ok());
}

// ---- selection over a scripted device table ---------------------------------------------

fn device(
    label: &str,
    uuid: &str,
    mount: &std::path::Path,
    removable: bool,
    disk: &str,
) -> DeviceEntry {
    DeviceEntry {
        device: format!("/dev/{disk}1"),
        label: Some(label.to_owned()),
        uuid: Some(uuid.to_owned()),
        mount_point: Some(mount.to_path_buf()),
        removable,
        physical_disk: disk.to_owned(),
        free_bytes: 1 << 30,
    }
}

fn table(devices: Vec<DeviceEntry>) -> Arc<FakeDevices> {
    Arc::new(FakeDevices::new(DeviceSnapshot {
        devices,
        system_disks: vec!["sda".to_owned()],
    }))
}

fn removable(volume: &str, subdir: Option<&str>) -> BackupTarget {
    BackupTarget::Removable {
        volume: volume.to_owned(),
        subdir: subdir.map(str::to_owned),
    }
}

#[test]
fn a_plugged_volume_resolves_to_its_mount_plus_subdir_and_names_the_eject_command() {
    let mount = std::env::temp_dir().join(format!("spot_t13_mount_{}", std::process::id()));
    std::fs::create_dir_all(&mount).unwrap();
    let devices = table(vec![device("BACKUP", "abcd-1234", &mount, true, "sdb")]);
    let resolved = resolve(&removable("label:BACKUP", None), devices).unwrap();
    assert_eq!(resolved.dir, mount.join("spot-backups"));
    assert!(resolved.dir.is_dir(), "the folder on the drive was created");
    assert!(resolved.eject_hint.is_some());
    let _ = std::fs::remove_dir_all(&mount);
}

#[test]
fn an_absent_volume_is_target_absent_with_nothing_written() {
    let mount = std::env::temp_dir().join(format!("spot_t13_absent_{}", std::process::id()));
    let devices = table(Vec::new());
    let refusal: Refusal = resolve(&removable("label:BACKUP", None), devices).unwrap_err();
    assert_eq!(refusal.code, TARGET_ABSENT);
    assert!(refusal.retry_next_slot);
    assert!(!mount.exists(), "nothing was created");
}

#[test]
fn a_non_removable_or_system_disk_volume_and_a_duplicate_label_are_refused() {
    let mount = std::env::temp_dir().join(format!("spot_t13_unsafe_{}", std::process::id()));
    std::fs::create_dir_all(&mount).unwrap();
    let fixed = table(vec![device("DATA", "u-1", &mount, false, "sdc")]);
    assert_eq!(
        resolve(&removable("label:DATA", None), fixed)
            .unwrap_err()
            .code,
        "backup.target_unsafe"
    );
    let system = table(vec![device("USBISH", "u-2", &mount, true, "sda")]);
    assert_eq!(
        resolve(&removable("label:USBISH", None), system)
            .unwrap_err()
            .code,
        "backup.target_unsafe"
    );
    let twice = table(vec![
        device("BACKUP", "u-3", &mount, true, "sdb"),
        device("BACKUP", "u-4", &mount, true, "sdd"),
    ]);
    assert_eq!(
        resolve(&removable("label:BACKUP", None), twice)
            .unwrap_err()
            .code,
        "backup.target_ambiguous"
    );
    let _ = std::fs::remove_dir_all(&mount);
}

#[test]
fn cloud_without_a_connector_is_refused_without_naming_the_endpoint_or_a_credential() {
    let cloud = BackupTarget::Cloud {
        endpoint: "https://secret-host.example.test".to_owned(),
        key_id: "AKIA-SECRET".to_owned(),
        credential_ref: Some("secret:backup-key".to_owned()),
        prefix: None,
        max_kbps: None,
        quota_bytes: None,
    };
    let refusal = resolve(&cloud, table(Vec::new())).unwrap_err();
    assert_eq!(refusal.code, "backup.target_unavailable");
    let text = format!("{} {}", refusal.code, refusal.reason);
    for secret in ["secret-host", "AKIA-SECRET", "backup-key"] {
        assert!(!text.contains(secret), "{secret} leaked into: {text}");
    }
}
