//! Every handler is idempotent: applying a change the machine already has reports no change,
//! and the second plan is empty. The tests drive each module's `apply` twice with a fake
//! machine whose state is updated between the two calls.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_agent_linux_sys::certs::CertificatesModule;
use spot_agent_linux_sys::chrony::TimeModule;
use spot_agent_linux_sys::storage::{SnapshotBackend, SnapshotRequest, StorageModule};
use spot_agent_linux_sys::sysctl::SysctlModule;
use spot_agent_linux_sys::testing::{FakeFs, FakeRunner, ok_output};

#[test]
fn a_sysctl_set_is_idempotent() {
    let runner = FakeRunner::new()
        .with_response("sysctl -n vm.swappiness", ok_output("60\n"))
        .with_response("sysctl -w vm.swappiness=10", ok_output(""))
        .with_response("sysctl --system", ok_output(""));
    let fs = FakeFs::new();
    let module = SysctlModule::new(&runner, &fs);
    assert!(module.apply_set("vm.swappiness", 10).unwrap().changed);
    // The machine now reports the new value.
    runner.set_response("sysctl -n vm.swappiness", ok_output("10\n"));
    assert!(!module.apply_set("vm.swappiness", 10).unwrap().changed);
    assert!(module.plan_set("vm.swappiness", 10).unwrap().is_noop());
}

#[test]
fn a_btrfs_snapshot_create_is_idempotent() {
    let runner = FakeRunner::new()
        .with_response("btrfs subvolume list -o /data", ok_output(""))
        .with_response("btrfs subvolume snapshot /data /data/snap1", ok_output(""));
    let fs = FakeFs::new().with_dir("/data");
    let module = StorageModule::new(&runner, &fs);
    let request = SnapshotRequest::new(SnapshotBackend::Btrfs, "/data", "snap1", None).unwrap();
    assert!(module.apply_create(&request).unwrap().changed);
    // The snapshot now appears in the listing.
    runner.set_response(
        "btrfs subvolume list -o /data",
        ok_output("ID 257 gen 30 top level 5 path snap1\n"),
    );
    assert!(!module.apply_create(&request).unwrap().changed);
    assert!(module.plan_create(&request).unwrap().is_noop());
}

#[test]
fn a_certificate_install_is_idempotent() {
    let runner = FakeRunner::new().with_response("update-ca-trust extract", ok_output(""));
    let fs = FakeFs::new().with_file("/etc/os-release", "ID=fedora\nID_LIKE=\"rhel fedora\"\n");
    let module = CertificatesModule::new(&runner, &fs);
    assert!(
        module
            .apply_install("spot-root", common::PEM)
            .unwrap()
            .changed
    );
    assert!(
        !module
            .apply_install("spot-root", common::PEM)
            .unwrap()
            .changed
    );
}

#[test]
fn setting_chrony_servers_is_idempotent() {
    let runner = FakeRunner::new().with_response("chronyc reload sources", ok_output(""));
    let fs = FakeFs::new();
    let module = TimeModule::new(&runner, &fs);
    let servers = vec!["time.example".to_owned()];
    assert!(module.apply_servers(&servers).unwrap().changed);
    assert!(!module.apply_servers(&servers).unwrap().changed);
    assert!(module.plan_servers(&servers).unwrap().is_noop());
}
