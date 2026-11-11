//! Transcript-driven tests for Fedora, a RHEL-compatible distribution and Ubuntu LTS:
//! each distribution's recorded command output drives every module, and the assertions are
//! the differences the three distributions actually have (SELinux against AppArmor, LVM or
//! btrfs against ext4, NetworkManager against systemd-networkd, RHEL against Debian
//! trust-store paths).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::Path;
use std::sync::Arc;

use spot_agent_linux_sys::certs::{CertificatesModule, DistroFamily};
use spot_agent_linux_sys::chrony::TimeModule;
use spot_agent_linux_sys::mac::{MacKind, MacMode, MacModule};
use spot_agent_linux_sys::network::{NetworkBackend, NetworkModule};
use spot_agent_linux_sys::storage::{SnapshotBackend, StorageModule};
use spot_agent_linux_sys::sysctl::SysctlModule;
use spot_core::{Clock, ManualClock};

fn clock() -> Arc<dyn Clock> {
    Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH))
}

#[test]
fn fedora_lvm_xfs_selinux_networkmanager() {
    let runner = common::runner("fedora");
    let fs = common::fs("fedora");

    let storage = StorageModule::new(&runner, &fs);
    let facts = storage.facts().unwrap();
    assert_eq!(facts.backend, Some(SnapshotBackend::Lvm));
    assert_eq!(
        storage
            .filesystem_type(Path::new("/var/lib/pgsql"))
            .unwrap()
            .as_deref(),
        Some("xfs")
    );
    assert_eq!(
        storage.free_space(Path::new("/var/lib/pgsql")).unwrap(),
        27_000_000_000
    );

    let sysctl = SysctlModule::new(&runner, &fs);
    assert_eq!(sysctl.get("vm.swappiness").unwrap(), "60");
    assert!(
        sysctl
            .list()
            .unwrap()
            .iter()
            .any(|(key, _)| key == "vm.swappiness")
    );

    let time = TimeModule::new(&runner, &fs);
    let status = time.status().unwrap();
    assert!(status.synced);
    assert_eq!(status.tracking.stratum, 3);
    assert_eq!(status.tracking.system_time_nanos, 12_345);

    let mac = MacModule::new(&runner, &fs);
    let mac_status = mac.status().unwrap();
    assert_eq!(mac_status.kind, MacKind::Selinux);
    assert_eq!(mac_status.mode, MacMode::Enforcing);

    let certs = CertificatesModule::new(&runner, &fs);
    assert_eq!(certs.family().unwrap(), DistroFamily::Rhel);
    assert!(certs.exists("spot-root").unwrap());

    let network = NetworkModule::new(&runner, &fs, clock());
    let facts = network.facts().unwrap();
    assert_eq!(facts.backend, Some(NetworkBackend::NetworkManager));
    assert_eq!(facts.interfaces[0].name, "eth0");
    assert_eq!(facts.routes[0].gateway.as_deref(), Some("192.0.2.1"));
    assert_eq!(facts.dns, vec!["192.0.2.1".to_owned()]);
}

#[test]
fn rhel_btrfs_selinux_networkmanager() {
    let runner = common::runner("rhel");
    let fs = common::fs("rhel");

    let storage = StorageModule::new(&runner, &fs);
    assert_eq!(
        storage.facts().unwrap().backend,
        Some(SnapshotBackend::Btrfs)
    );
    assert_eq!(
        storage
            .filesystem_type(Path::new("/var/lib/pgsql"))
            .unwrap()
            .as_deref(),
        Some("btrfs")
    );

    let sysctl = SysctlModule::new(&runner, &fs);
    assert_eq!(sysctl.get("vm.swappiness").unwrap(), "30");

    let time = TimeModule::new(&runner, &fs);
    let status = time.status().unwrap();
    assert_eq!(status.sources.len(), 2);
    assert_eq!(status.tracking.system_time_nanos, -45);

    let mac = MacModule::new(&runner, &fs);
    assert_eq!(mac.status().unwrap().kind, MacKind::Selinux);

    let certs = CertificatesModule::new(&runner, &fs);
    assert_eq!(certs.family().unwrap(), DistroFamily::Rhel);

    let network = NetworkModule::new(&runner, &fs, clock());
    let facts = network.facts().unwrap();
    assert_eq!(facts.interfaces[0].name, "ens3");
    assert_eq!(facts.dns.len(), 2);
}

#[test]
fn ubuntu_ext4_apparmor_systemd_networkd() {
    let runner = common::runner("ubuntu");
    let fs = common::fs("ubuntu");

    let storage = StorageModule::new(&runner, &fs);
    assert_eq!(storage.facts().unwrap().backend, None);
    assert_eq!(
        storage
            .filesystem_type(Path::new("/var/lib/pgsql"))
            .unwrap()
            .as_deref(),
        Some("ext4")
    );

    let sysctl = SysctlModule::new(&runner, &fs);
    assert_eq!(sysctl.get("vm.swappiness").unwrap(), "60");

    let time = TimeModule::new(&runner, &fs);
    assert!(time.status().unwrap().synced);

    let mac = MacModule::new(&runner, &fs);
    let mac_status = mac.status().unwrap();
    assert_eq!(mac_status.kind, MacKind::Apparmor);
    assert_eq!(mac_status.profiles, Some(2));

    let certs = CertificatesModule::new(&runner, &fs);
    assert_eq!(certs.family().unwrap(), DistroFamily::Debian);
    assert!(certs.exists("spot-root").unwrap());

    let network = NetworkModule::new(&runner, &fs, clock());
    let facts = network.facts().unwrap();
    assert_eq!(facts.backend, Some(NetworkBackend::SystemdNetworkd));
    assert_eq!(facts.interfaces[0].name, "enp1s0");
    assert_eq!(facts.dns, vec!["203.0.113.1".to_owned()]);
}

#[test]
fn every_distribution_transcript_covers_every_module() {
    for distro in common::DISTROS {
        let runner = common::runner(distro);
        let fs = common::fs(distro);
        // Storage, sysctl, time, mac, certificates and network all read the one transcript
        // without an unrecorded command.
        StorageModule::new(&runner, &fs).facts().unwrap();
        SysctlModule::new(&runner, &fs).list().unwrap();
        TimeModule::new(&runner, &fs).status().unwrap();
        MacModule::new(&runner, &fs).status().unwrap();
        CertificatesModule::new(&runner, &fs).list().unwrap();
        NetworkModule::new(&runner, &fs, clock()).facts().unwrap();
    }
}
