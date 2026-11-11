//! Real-system check for `WindowsCertStore`'s certificate validity parsing, run only on the
//! Windows runner's weekly build (`#[ignore]`). This is the honest counterpart to the
//! fixture-based `tests/cert_dates.rs`: it proves the locale-tolerant parser agrees with
//! whatever `certutil`/PowerShell actually prints on a real machine, instead of only on
//! recorded text.
//!
//! No Windows runner is available to this agent/orchestrator (the same gap WP-ADM-05.T1,
//! T2 and T3 reported); this test is checked in and gated so it compiles and is picked up
//! by `cargo test --all-targets` everywhere, but it cannot be run here. It must be run by
//! the weekly Windows build before this PR's acceptance claim is trusted.

#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{SystemTime, UNIX_EPOCH};

use spot_agent_windows_sys::windows::WindowsCertStore;
use spot_agent_windows_sys::{CertStore, CertificateStore, Expiry};

/// The LocalMachine Root store always holds trusted roots whose validity window spans
/// "now" on a machine with a sane clock; reading it must parse rather than error, and the
/// result must not be `NotYetValid` (roots are issued well in the past).
#[test]
#[ignore = "requires a real Windows host with a LocalMachine certificate store (weekly Windows build only)"]
fn check_expiry_reads_a_real_root_certificates_validity_window() {
    let store = WindowsCertStore::new();
    let certificates = store
        .list(CertStore::Root)
        .expect("the LocalMachine Root store must be readable");
    let first = certificates
        .first()
        .expect("a stock Windows machine has trusted root certificates");

    #[allow(clippy::cast_possible_wrap)]
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("the system clock is after the Unix epoch")
        .as_secs() as i64;

    let expiry = store
        .check_expiry(CertStore::Root, &first.thumbprint, now_unix)
        .expect(
            "the validity window must parse from certutil/PowerShell output on a real \
             machine, in whatever locale this host is running",
        );
    assert_ne!(
        expiry,
        Expiry::NotYetValid,
        "a trusted root installed on a stock machine is not from the future"
    );
}
