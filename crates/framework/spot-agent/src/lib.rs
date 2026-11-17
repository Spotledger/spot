//! The Spotledger host agent executable (WP-ADM-02.T2, blueprint WS22, ADR-44, ADR-099).
//!
//! The library holds everything testable: configuration ([`config`]), the state directory
//! ([`state`]), the key store ([`keystore`]), enrolment ([`enrol`]), the trust anchor
//! ([`trust`]), the local hash-chained command audit log ([`audit`]), structured redacted
//! logging ([`logging`]) and the run loop entry ([`run`]).
//!
//! The agent is outbound-only: it never listens on a port. The channel client
//! ([`channel`], WP-ADM-02.T3), the handler registry ([`handlers`], WP-ADM-03.T2), the
//! privilege helper ([`privilege`], WP-ADM-03.T2) and the Windows service host
//! ([`winsvc`], WP-ADM-05.T6) plug into seams declared here.

#![forbid(unsafe_code)]

pub mod audit;
pub mod channel;
pub mod cli;
pub mod config;
pub mod enrol;
pub mod error;
pub mod handlers;
pub mod keystore;
pub mod logging;
pub mod privilege;
pub mod run;
pub mod state;
pub mod trust;
pub mod update;
pub mod winsvc;

/// The agent version.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
