//! L3 journal/sync cluster test harness (WP-RTB-01.T1).
//!
//! `harness/rust/l3-http` proves one host and one site over the real stack. The journal and
//! sync tasks (WP-RTB-01 and later) need to prove something that crate cannot: two or three
//! in-process hosts (the same `spot_cli::start_with` assembly the `spot` binary's `serve`
//! command runs — not separate OS processes), each with its own PostgreSQL database and
//! journal folder, and a network between them a test can cut and heal. This crate is that
//! harness, separate from `l3-http` so the tasks that add journal and sync cases do not queue
//! behind the 24 briefs that already touch `l3-http` (see `README.md`).
//!
//! * [`Cluster`] and [`Node`] — a group of in-process hosts, each with its own database, root,
//!   journal folder and loopback addresses, with lifecycle controls (`stop`, `kill`, `start`,
//!   `partition_from`, `heal`).
//! * [`NodeSpec`], [`Role`] and [`role_fragment`] — the one place that builds a node's role
//!   fragment, so a later task that gives roles real meaning changes one function.
//! * [`FlakyStore`] — a [`spot_journal_store::JournalStore`] wrapper that fails on command.
//! * [`tamper`] — byte-level corruption helpers over a folder store's files.
//! * [`wait_until`], [`read_conditions`], [`read_audit`], [`fingerprint`] and [`seed::documents`]
//!   — read-only and seeding helpers other tasks' tests need.
//!
//! No journal or sync behaviour lives here (out of scope: see the task brief); `tests/
//! harness_self.rs` proves the harness itself, not any journal or sync case.

#![forbid(unsafe_code)]
#![allow(clippy::multiple_crate_versions)]

mod client;
mod cluster;
mod flaky_store;
mod helpers;
mod node;
mod proxy;
pub mod seed;
pub mod tamper;

pub use client::{Client, Reply};
pub use cluster::Cluster;
pub use flaky_store::FlakyStore;
pub use helpers::{Row, fingerprint, read_audit, read_conditions, wait_until};
pub use node::{Node, NodeSpec, Role, role_fragment};
pub use proxy::Proxy;
