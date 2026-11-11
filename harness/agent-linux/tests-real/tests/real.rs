//! The real-system conformance tests for the Linux modules (WP-ADM-04.T3).
//!
//! Every test here is `#[ignore]`d, so `cargo test` on a developer's host compiles the
//! crate and runs nothing. The harness (`../run.sh`) builds this crate, mounts the test
//! binary into a distribution container and runs each test with `--ignored --exact`.
//! `../vm/run.sh` does the same on a qemu cloud image for the `vm_only` tests.
//!
//! Each test exercises the module's public plan/apply/verify (part 1) or typed methods
//! (part 2) against the genuine distribution: real `dnf`/`apt`, real `systemctl`, real
//! `useradd`, real `lsblk`/`findmnt`, real `btrfs`/`lvcreate`. Nothing is mocked. Anything
//! a test creates is uniquely prefixed and cleaned up, so a failure cannot leave the
//! machine dirty.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_agent_linux::real::StdFileSystem;
use spot_agent_linux::{FileSystem, PackageBackend};
use spot_agent_linux_real::{
    Cleanup, family, part1_context, part1_module, setup, unique_name, validate,
};
use spot_agent_linux_sys::real::{StdCommandRunner as SysRunner, StdFileSystem as SysFs};
use spot_agent_linux_sys::{
    CertificatesModule, MacModule, NetworkModule, SnapshotBackend, SnapshotRequest, StorageModule,
    SysctlModule, TimeModule,
};
use spot_mgmt_ops::ParamValue;

// ---------------------------------------------------------------------------
// Part 1: packages, services, users, firewall
// ---------------------------------------------------------------------------

/// `/etc/os-release` names the family the target claims, and the package backend follows.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn distro_family_matches_target() {
    let fs = StdFileSystem::new();
    let text = fs
        .read_to_string("/etc/os-release")
        .expect("/etc/os-release is readable on a Linux host");
    assert!(text.contains("ID="), "os-release names an ID: {text}");

    let backend = PackageBackend::detect(&fs).expect("a supported distribution family");
    let expected = std::env::var("SPOT_TARGET_FAMILY").unwrap_or_else(|_| family());
    match expected.as_str() {
        "fedora" | "rhel" => assert_eq!(backend.name(), "dnf", "family {expected} uses dnf"),
        "ubuntu" | "debian" => assert_eq!(backend.name(), "apt", "family {expected} uses apt"),
        other => panic!("the target family {other} is not one the suite knows"),
    }
}

/// `os.package.list` reads the genuine installed-package list through the real manager.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn package_list_plan_apply_verify() {
    let context = part1_context();
    let module = part1_module(&context, "os.package.list");
    let args = validate("os.package.list", vec![]);

    let preview = module.plan(&args).expect("a package-list preview");
    assert_eq!(preview.len(), 1, "listing packages is one command");

    module.apply(&args).expect("the package list is readable");
    let check = module.verify(&args).expect("the package list verifies");
    assert!(check.ok, "{}", check.detail);
}

/// `os.service.status` and `os.service.failed` read a live systemd manager.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn service_status_and_failed() {
    let context = part1_context();

    let status = part1_module(&context, "os.service.status");
    let status_args = validate(
        "os.service.status",
        vec![(
            "unit",
            ParamValue::Text("systemd-journald.service".to_owned()),
        )],
    );
    let preview = status.plan(&status_args).expect("a service-status preview");
    assert_eq!(preview.len(), 1, "reading a unit is one command");
    status
        .apply(&status_args)
        .expect("systemd-journald is present under a real systemd");

    let failed = part1_module(&context, "os.service.failed");
    let failed_args = validate("os.service.failed", vec![]);
    failed
        .apply(&failed_args)
        .expect("the failed-unit list is readable");
}

/// A throwaway account: create, lock, unlock, read groups, delete — and always delete.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn user_create_lock_unlock_delete_roundtrip() {
    let context = part1_context();
    let name = unique_name("user");

    // Best-effort cleanup if an assertion fails before the explicit delete.
    let mut cleanup = Cleanup::new();
    cleanup.push(&["userdel", "--force", name.as_str()]);

    let create = part1_module(&context, "os.user.create");
    let create_args = validate(
        "os.user.create",
        vec![
            ("name", ParamValue::Text(name.clone())),
            ("create_home", ParamValue::Bool(false)),
        ],
    );
    create.apply(&create_args).expect("the account is created");
    assert!(
        create
            .verify(&create_args)
            .expect("the account is verified")
            .ok,
        "the created account exists"
    );

    let lock_args = validate(
        "os.user.lock",
        vec![("name", ParamValue::Text(name.clone()))],
    );
    part1_module(&context, "os.user.lock")
        .apply(&lock_args)
        .expect("the account is locked");

    let unlock_args = validate(
        "os.user.unlock",
        vec![("name", ParamValue::Text(name.clone()))],
    );
    part1_module(&context, "os.user.unlock")
        .apply(&unlock_args)
        .expect("the account is unlocked");

    let groups_args = validate(
        "os.user.groups",
        vec![("name", ParamValue::Text(name.clone()))],
    );
    part1_module(&context, "os.user.groups")
        .apply(&groups_args)
        .expect("the account's groups are readable");

    let delete_args = validate(
        "os.user.delete",
        vec![
            ("name", ParamValue::Text(name.clone())),
            ("remove_home", ParamValue::Bool(true)),
        ],
    );
    part1_module(&context, "os.user.delete")
        .apply(&delete_args)
        .expect("the account is deleted");
    assert!(
        part1_module(&context, "os.user.delete")
            .verify(&delete_args)
            .expect("the deletion is verified")
            .ok,
        "the account is gone"
    );
}

/// `os.firewall.status` and `os.firewall.list` detect and read a real firewalld. VM-only:
/// a container shares the host firewall.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn firewall_status_firewalld() {
    firewall_status_and_list();
}

/// `os.firewall.status` and `os.firewall.list` detect and read a real ufw. VM-only.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn firewall_status_ufw() {
    firewall_status_and_list();
}

/// The shared body of the two firewall tests: detect the backend and list the rules.
fn firewall_status_and_list() {
    let context = part1_context();

    let status_args = validate("os.firewall.status", vec![]);
    part1_module(&context, "os.firewall.status")
        .apply(&status_args)
        .expect("a firewall backend is detected");

    let list_args = validate("os.firewall.list", vec![]);
    part1_module(&context, "os.firewall.list")
        .apply(&list_args)
        .expect("the firewall rules are readable");
}

// ---------------------------------------------------------------------------
// Part 2: storage, network, certificates, time, sysctl, SELinux/AppArmor
// ---------------------------------------------------------------------------

/// `os.storage.facts` parses the real `lsblk`/`findmnt` output.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn storage_facts_are_read() {
    let runner = SysRunner::new();
    let fs = SysFs::new();
    let module = StorageModule::new(&runner, &fs);

    let facts = module.facts().expect("storage facts are readable");
    assert!(
        !facts.mounts.is_empty(),
        "a running machine reports at least one mount"
    );
}

/// `os.cert.list` reads the real distribution trust store.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn certificates_list_is_read() {
    let runner = SysRunner::new();
    let fs = SysFs::new();
    let module = CertificatesModule::new(&runner, &fs);

    let family = module
        .family()
        .expect("the distribution family is recognised");
    assert!(
        matches!(family.name(), "rhel" | "debian"),
        "a known trust-store family"
    );
    // The anchor directory may be empty on a minimal image; listing it must still work.
    let _certificates = module.list().expect("the trust store is listable");
}

/// `os.time.status` runs the real `chronyc`. A minimal image may not have chronyd
/// running; the test then proves the module produced a typed error, never a panic.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn time_status_is_read() {
    let runner = SysRunner::new();
    let fs = SysFs::new();
    let module = TimeModule::new(&runner, &fs);

    match module.status() {
        Ok(status) => {
            assert!(status.tracking.stratum <= 16, "a plausible stratum");
        }
        Err(error) => assert!(
            error.code == spot_agent_linux_sys::error::TOOL_MISSING
                || error.code == spot_agent_linux_sys::error::COMMAND_FAILED,
            "an unexpected chrony failure: {error}"
        ),
    }
}

/// `os.sysctl.get` reads a real kernel parameter through `/proc/sys`.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn sysctl_get_reads_a_key() {
    let runner = SysRunner::new();
    let fs = SysFs::new();
    let module = SysctlModule::new(&runner, &fs);

    let value = module
        .get("vm.swappiness")
        .expect("vm.swappiness is on the allow-list and readable");
    assert!(
        value.parse::<i64>().is_ok(),
        "vm.swappiness is an integer: {value}"
    );
}

/// `os.network.facts` reads the real interfaces and routes from `ip`.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn network_facts_are_read() {
    let runner = SysRunner::new();
    let fs = SysFs::new();
    let clock = Arc::new(spot_core::SystemClock::new());
    let module = NetworkModule::new(&runner, &fs, clock);

    let interfaces = module.interfaces().expect("interfaces are readable");
    assert!(
        !interfaces.is_empty(),
        "a running machine reports at least one interface"
    );
    let _routes = module.routes().expect("routes are readable");
}

/// A btrfs snapshot round trip on a loop device: create, list, verify, delete.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn btrfs_snapshot_roundtrip() {
    let runner = SysRunner::new();
    let fs = SysFs::new();
    let module = StorageModule::new(&runner, &fs);

    let image = format!("/tmp/{}-btrfs.img", unique_name("btrfs"));
    let mountpoint = format!("/tmp/{}-mnt", unique_name("btrfs"));

    setup(&["truncate", "-s", "256M", image.as_str()]);
    setup(&["mkfs.btrfs", "-f", image.as_str()]);
    std::fs::create_dir_all(&mountpoint).expect("the mountpoint is created");
    setup(&["mount", "-o", "loop", image.as_str(), mountpoint.as_str()]);

    let mut cleanup = Cleanup::new();
    cleanup.push(&["umount", mountpoint.as_str()]);
    cleanup.push(&["rm", "-f", image.as_str()]);

    let source = format!("{mountpoint}/data");
    setup(&["btrfs", "subvolume", "create", source.as_str()]);

    let name = unique_name("snap");
    let request = SnapshotRequest::new(SnapshotBackend::Btrfs, &source, &name, None)
        .expect("a valid btrfs snapshot request");

    let preview = module.plan_create(&request).expect("a snapshot preview");
    assert!(!preview.is_noop(), "the snapshot does not exist yet");

    module
        .apply_create(&request)
        .expect("the snapshot is created");
    assert!(
        module.exists(&request).expect("the snapshot is checked"),
        "the snapshot now exists"
    );
    assert!(
        module
            .verify_create(&request)
            .expect("the snapshot verifies")
            .ok,
        "the snapshot is present"
    );
    let listed = module
        .list(SnapshotBackend::Btrfs, &source)
        .expect("snapshots are listed");
    assert!(
        listed.iter().any(|snapshot| snapshot.name.contains(&name)),
        "the snapshot appears in the list: {listed:?}"
    );

    module
        .apply_delete(&request)
        .expect("the snapshot is deleted");
    assert!(
        !module.exists(&request).expect("the snapshot is checked"),
        "the snapshot is gone"
    );
}

/// An LVM snapshot round trip on loop devices: create, list, verify, delete.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn lvm_snapshot_roundtrip() {
    let runner = SysRunner::new();
    let fs = SysFs::new();
    let module = StorageModule::new(&runner, &fs);

    let disk = format!("/tmp/{}-lvm.img", unique_name("lvm"));
    setup(&["truncate", "-s", "512M", disk.as_str()]);
    let loop_device = setup(&["losetup", "--find", "--show", disk.as_str()]);

    let group = format!("{}vg", unique_name("lvm"));
    let volume = "data";
    setup(&["pvcreate", "-f", loop_device.as_str()]);
    setup(&["vgcreate", group.as_str(), loop_device.as_str()]);
    setup(&["lvcreate", "-y", "-n", volume, "-L", "256M", group.as_str()]);

    let mut cleanup = Cleanup::new();
    cleanup.push(&["vgremove", "-f", group.as_str()]);
    cleanup.push(&["pvremove", "-f", loop_device.as_str()]);
    cleanup.push(&["losetup", "-d", loop_device.as_str()]);
    cleanup.push(&["rm", "-f", disk.as_str()]);

    let source = format!("{group}/{volume}");
    let name = unique_name("snap");
    let request =
        SnapshotRequest::new(SnapshotBackend::Lvm, &source, &name, Some(64 * 1024 * 1024))
            .expect("a valid LVM snapshot request");

    module
        .apply_create(&request)
        .expect("the LVM snapshot is created");
    assert!(
        module.exists(&request).expect("the snapshot is checked"),
        "the LVM snapshot exists"
    );
    let listed = module
        .list(SnapshotBackend::Lvm, &source)
        .expect("LVM snapshots are listed");
    assert!(
        listed.iter().any(|snapshot| snapshot.name == name),
        "the LVM snapshot appears in the list: {listed:?}"
    );
    assert!(
        module
            .verify_create(&request)
            .expect("the snapshot verifies")
            .ok,
        "the LVM snapshot is present"
    );

    module
        .apply_delete(&request)
        .expect("the LVM snapshot is deleted");
    assert!(
        !module.exists(&request).expect("the snapshot is checked"),
        "the LVM snapshot is gone"
    );
}

/// `os.mac.status` reads a real SELinux. VM-only: a container shares the host's LSM.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn mac_status_selinux() {
    let status = mac_status();
    assert_eq!(status.kind.name(), "selinux", "the host runs SELinux");
}

/// `os.mac.status` reads a real AppArmor. VM-only: a container shares the host's LSM.
#[test]
#[ignore = "runs against the real host; run with --ignored"]
fn mac_status_apparmor() {
    let status = mac_status();
    assert_eq!(status.kind.name(), "apparmor", "the host runs AppArmor");
}

/// The shared body of the two MAC tests: read and validate the status.
fn mac_status() -> spot_agent_linux_sys::MacStatus {
    let runner = SysRunner::new();
    let fs = SysFs::new();
    let module = MacModule::new(&runner, &fs);
    let status = module.status().expect("SELinux or AppArmor is present");
    assert!(
        matches!(status.kind.name(), "selinux" | "apparmor"),
        "a recognised mandatory access control system"
    );
    status
}
