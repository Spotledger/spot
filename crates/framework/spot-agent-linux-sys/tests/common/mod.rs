//! Shared builders for the integration tests: the per-distribution transcripts and the
//! fake filesystem each distribution's `/etc/os-release` and trust store need.

#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_agent_linux_sys::testing::{FakeFs, FakeRunner, runner_from_transcript};

/// The three distributions the suite covers.
pub const DISTROS: [&str; 3] = ["fedora", "rhel", "ubuntu"];

/// A PEM certificate body the tests install.
pub const PEM: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";

/// The recorded command transcript for a distribution.
#[must_use]
pub fn runner(distro: &str) -> FakeRunner {
    let text = match distro {
        "fedora" => include_str!("../transcripts/fedora/suite.txt"),
        "rhel" => include_str!("../transcripts/rhel/suite.txt"),
        "ubuntu" => include_str!("../transcripts/ubuntu/suite.txt"),
        other => panic!("no transcript for {other}"),
    };
    runner_from_transcript(text)
}

/// The fake filesystem for a distribution: its `/etc/os-release` and one installed
/// certificate in the family's anchor directory.
#[must_use]
pub fn fs(distro: &str) -> FakeFs {
    match distro {
        "fedora" => FakeFs::new()
            .with_file("/etc/os-release", "ID=fedora\nID_LIKE=\"rhel fedora\"\n")
            .with_file("/etc/pki/ca-trust/source/anchors/spot-root.crt", PEM),
        "rhel" => FakeFs::new()
            .with_file("/etc/os-release", "ID=\"rhel\"\nID_LIKE=\"fedora\"\n")
            .with_file("/etc/pki/ca-trust/source/anchors/spot-root.crt", PEM),
        "ubuntu" => FakeFs::new()
            .with_file("/etc/os-release", "ID=ubuntu\nID_LIKE=debian\n")
            .with_file("/usr/local/share/ca-certificates/spot-root.crt", PEM),
        other => panic!("no filesystem for {other}"),
    }
}
