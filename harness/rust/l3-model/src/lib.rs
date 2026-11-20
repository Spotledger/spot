//! L3 model-based lifecycle test (WP-TST-10.T5).
//!
//! The example-based L3 suites run scripts someone thought of. This crate runs **sequences nobody
//! scripted**: a seeded generator ([`generator`]) produces 20 to 200 lifecycle calls over a few
//! documents, a pure reference state machine ([`model::Model`]) says before each call what must
//! happen, a [`runner::Runner`] makes the call through the real document engine and compares the
//! result and a full read-back of every document with the model, and a [`shrink`] pass reduces a
//! failing sequence to a minimal one that replays from its seed.
//!
//! The model is written from the ADRs (ADR-42 states and amendments, ADR-51 phases) and from the
//! existing L3 case expectations, in plain terms. It never calls engine code: a model that did
//! would prove nothing. Every rule is listed with its source in `README.md`.
//!
//! The world is the L7 engine's (`spot_l7_engine::world::World`): the sample DocTypes, the six
//! principals, no app controllers. So there is no roll-up on post or cancel, and the only
//! lifecycle effects are the engine's own.

pub mod generator;
pub mod model;
pub mod runner;
pub mod shrink;

pub use generator::{GenConfig, generate};
pub use model::{Bug, Call, Expectation, Model, Refusal, Who};
pub use runner::{Divergence, RunReport, Runner};
pub use shrink::{shrink, to_rust};
