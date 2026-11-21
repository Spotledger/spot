//! End-to-end journal fixtures (WP-JRN-07.T4, ADR-061).
//!
//! The journal is a dozen small crates, each tested against doubles of its neighbours. This
//! crate holds the **shared fixtures** that wire the real crates together over the in-memory
//! store; the scenarios are the tests under `tests/`:
//!
//! * `chain.rs`: commit, group-commit relay, durability ack, snapshot, verify, rebuild, audit.
//! * `fork.rs`: a fork at 400, roll-forward and point-in-time rebuilds, pinned verification.
//! * `erasure.rs`: personal fields protected on the way in, a subject erased, no cleartext.
//! * `crash.rs`: crashes between the steps of a commit, a seal and a snapshot.
//!
//! The fixtures are test support: they use `unwrap` and `expect` on purpose.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

pub mod audit;
pub mod events;
pub mod keys;
pub mod rig;
pub mod store;
