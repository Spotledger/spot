//! L3 HTTP behaviour suite (WP-TST-10.T2).
//!
//! `l3-documents` (WP-TST-10.T1) pins document behaviour on the app API stub. This crate pins
//! the same behaviour through the **real stack**: the product host (`spot serve`'s assembly),
//! real HTTP over a loopback socket, real sessions, the real permission engine and a real
//! PostgreSQL database for a freshly created site with the platform and sample apps installed
//! and seeded.
//!
//! Nothing here is mocked. Every case talks to a running host as a signed-in user, and every
//! invariant is read back over the API (or, for the event outbox, from the database the host
//! itself wrote).
//!
//! * [`fixture`] — a root directory, a database, the installed apps, the users of each sample
//!   role, a running host on an ephemeral port, and a drop guard that leaves nothing behind.
//! * [`client`] — a typed HTTP client with session handling and the contract's error decoding,
//!   so a case reads like a user story.
//! * [`report`] — case outcomes, a JUnit-style document for CI and the failure text a reviewer
//!   needs.
//! * [`cases`] — one behaviour per case, named like the L3 suite: authentication, CRUD, child
//!   rows, the lifecycle with the roll-up, permissions, lists, metadata, and the HTTP edge.
//! * [`index`] — the case index and `CASES.md`, generated from it.
//! * [`parallel`] — the 200-session mixed workload with its end-state invariants.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

pub mod cases;
pub mod client;
pub mod fixture;
pub mod index;
pub mod report;

pub use client::{Client, Reply};
pub use fixture::{Fixture, User};
pub use report::{Filter, Outcome, Report, Status};
