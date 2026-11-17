//! The durability gate (WP-RTB-01.T4, ADR-061 section 4).
//!
//! Zero loss is a statement about failure domains: a commit is not acknowledged until its event
//! exists in a second failure domain. The storage contract's `commit()` returns no position and is
//! frozen, so the gate sits at the HTTP boundary: [`gate`] is a middleware that, after the handler
//! returned success, waits for the journal to be durable up to the outbox head read after that
//! commit. See [`gate`] for what each policy does, [`PolicyCell`] for changing the policy at
//! runtime and [`Stats`] for what the readiness text reports.
//!
//! The crate depends on `axum` types only (not on `spot-http`): the host service installs it with
//! `ctx.router_layer(|router| router.route_layer(from_fn_with_state(state, gate)))`.
#![forbid(unsafe_code)]

mod error;
mod gate;
mod policy;
mod stats;

pub use error::GateError;
pub use gate::{GateConfig, GateFuture, GateState, HeadSource, Journal, Standby, gate};
pub use policy::{Kind, OnUnreachable, Policy, PolicyCell, PolicyChange};
pub use stats::Stats;
