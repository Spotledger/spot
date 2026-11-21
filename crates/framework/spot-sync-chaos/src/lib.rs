//! `spot-sync-chaos`: the deterministic chaos and scale suite for sync (WP-SYN-11, ADR-062
//! section 4, ADR-067 section 7).
//!
//! The suite builds a [`World`]: one hub relay ([`spot_sync_hub::HubRelay`]) over an in-memory
//! stream, `N` nodes ([`spot_sync_node::NodeIntake`]) over an in-memory store, a virtual clock
//! with a per-node skew, and a wire that can drop, duplicate and reorder frames. Nothing reads a
//! wall clock, a thread RNG, a socket or a database: every run is a pure function of its
//! [`WorldConfig`], so a failing seed replays exactly ([`replay_line`]).
//!
//! * [`World::cut`] arms a connection cut at one protocol step ([`Step`]) on one edge
//!   ([`Edge`]); [`World::heal`] reconnects. `tests/cuts.rs` cuts before and after each of the
//!   five steps.
//! * [`WorldConfig::reorder_window`], [`WorldConfig::duplicate`] and
//!   [`WorldConfig::loss_permille`] misbehave the wire (the fault model of `spot-sync-protocol`'s
//!   simulator, over the hub and node crates instead of its abstract sender and receiver); `tests/duplicates_reorder.rs` uses both.
//! * [`World::restore_node`] and [`World::start_session`] are the restored-node path:
//!   [`spot_sync_bootstrap::decide_start`] answers, and when it says *bootstrap* the node is
//!   bootstrapped from a snapshot instead of continued.
//! * [`World::skew`] and [`World::jump`] move a node's wall clock; `tests/skew.rs` proves the
//!   stale-document check neither misses nor invents a finding.
//! * [`spot_sync_bootstrap::Admission`] bounds how many nodes may return at once;
//!   `tests/scale.rs` brings 100 back.
//! * [`assert_guarantees`] is the one checker every test calls. It names, in its own words, the
//!   six properties the series is judged by and reports the first one that does not hold.
//!
//! Everything here is test support: the crate adds no production code. It is a library rather
//! than a `tests/` directory alone so the five scenario files can share one World, one fault
//! model and one checker without copying them.
//!
//! # Time, randomness and money
//!
//! Time is a virtual millisecond counter ([`Clock`]); randomness is one seeded generator
//! ([`Rng`]); every quantity is an integer. There is no `unsafe`, and no `unwrap`/`expect` outside
//! the tests.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

mod bootstrap;
mod catalog;
mod clock;
mod guarantees;
mod model;
mod rng;
mod seed;
mod store;
mod world;

pub use bootstrap::{ScaleReport, StartReport};
pub use clock::Clock;
pub use guarantees::{Guarantee, Violation, assert_guarantees, check_guarantees};
pub use model::{
    AckRecord, ChaosError, CutHit, Edge, Frame, NackRecord, RoundReport, Step, WorldConfig,
};
pub use rng::Rng;
pub use seed::{replay_line, report_failure, seed_override};
pub use store::{MemImages, MemStore};
pub use world::{ENTITIES, HUB_SOURCE, MAJOR, TOPIC, World};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-chaos";
