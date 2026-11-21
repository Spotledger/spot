//! `spot-agent-windows-real` — the real-system conformance harness for the WP-ADM-05
//! Windows modules (WP-ADM-05.T3).
//!
//! The crate under test (`spot-agent-windows`, `spot-agent-windows-sys`) is written so it
//! builds and unit-tests on Linux against an in-memory fake. This crate is the other half:
//! it drives the *real* `#[cfg(windows)]` implementations against a real Windows machine,
//! on Windows Server 2022/2025, Windows 10 22H2 and Windows 11, as `matrix.toml` lists.
//!
//! It is run by `harness/agent-windows/run.ps1`, which:
//!
//! 1. refuses unless the machine is a disposable VM (`SPOT_HARNESS_DISPOSABLE=1` and a
//!    `Win32_ComputerSystem.Model` that looks virtual);
//! 2. generates a unique `SPOT_HARNESS_PREFIX` (`spot-h-` plus eight hex digits) and
//!    passes it to every test;
//! 3. builds and runs this crate with `--test-threads=1` and turns the output into JUnit
//!    XML.
//!
//! # Throw-away objects only
//!
//! Every object a test creates — a service, a local account, a firewall rule, a shadow
//! copy — is named with the run's prefix, and the test removes it through a
//! [`Cleanup`] guard whose `Drop` is the Rust equivalent of a `finally`: it runs on the
//! success path and on a panic. A test enumerates the machine and asserts that the only
//! objects carrying the prefix are its own, so it can never touch anything it did not
//! create. `run.ps1` sweeps any prefixed object a crashed test left behind, and only
//! prefixed ones.
//!
//! # Not built on Linux
//!
//! The crate fails to compile off Windows with a clear message: a "green" run that built
//! nothing would be a false claim, so the harness refuses to be a no-op. The Linux CI host
//! tests the orchestration around this crate (`harness/agent-windows/tests/`) instead.

#![forbid(unsafe_code)]

#[cfg(not(windows))]
compile_error!(
    "spot-agent-windows-real is the real-system conformance harness for WP-ADM-05: \
     build and run it on Windows through harness/agent-windows/run.ps1, never on Linux. \
     The Linux CI host runs harness/agent-windows/tests/ instead."
);

#[cfg(windows)]
mod support;

#[cfg(windows)]
pub use support::*;
