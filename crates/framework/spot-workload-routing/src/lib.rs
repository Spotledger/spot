//! Workload routing (WP-PLT-20.T1, ADR-24).
//!
//! ADR-24 scales the platform by **workload and data partition** — roles, queue affinity, report
//! routing, replicas — never by splitting a transactional module into services. This crate is the
//! policy engine that decides *where* work runs; the actual proxying uses the existing HTTP layer
//! and the job workers.
//!
//! # The decision
//!
//! One pure function, [`route::route`], answers three questions for a request:
//!
//! - **Which class is it?** [`WorkloadClass`] — interactive, report, bulk, AI or background. The
//!   "heavy lane" ([`WorkloadClass::is_heavy`]) is the definition `spot-ai-isolation` shares.
//! - **Which node runs it?** A [`PathRule`](policy::PathRule) maps a path prefix, exact path or
//!   route name to a node selector, producing [`RouteDecision::Local`] or
//!   [`RouteDecision::Forward`].
//! - **Where do its reads go?** A [`ReplicaReadRule`](policy::ReplicaReadRule) allows a
//!   staleness-tolerant read to use a replica, producing [`RouteDecision::Replica`] and the
//!   [`ReadPreference`] the request context carries for `spot-postgres`.
//!
//! # The policy
//!
//! [`RoutingPolicy`](policy::RoutingPolicy) is settings-driven data (TOML or JSON), held live in a
//! [`PolicyStore`](policy::PolicyStore) that hot-reloads: readers take one [`std::sync::Arc`]
//! snapshot and finish on the version they started with, while a settings change publishes a new
//! one. Its three parts are [`QueueAffinityRule`](policy::QueueAffinityRule) (which queues a
//! node's workers consume, [`RoutingPolicy::queues_for`](policy::RoutingPolicy::queues_for)),
//! path rules and replica-read rules.
//!
//! # The safety rules
//!
//! - A request that carries a write **never** reads a replica ([`read::choose_read`]).
//! - Read-your-writes wins over staleness: within the policy's window, a replica must have applied
//!   the session's high-water [`SessionToken`](read::SessionToken).
//! - A replica lagging past the rule's `max_lag` falls back to the primary.
//! - Forwarding cannot loop: every hop is counted ([`forward::HOP_HEADER`]) and the policy's
//!   `max_hops` ends it; the deadline is propagated ([`forward::DEADLINE_HEADER`]).
//! - An empty target pool falls back to local with a metric ([`metrics::RoutingEvent`]).
//!
//! # The proof
//!
//! [`sim`] is a discrete-event load simulator over virtual time. `tests/sim.rs` runs the brief's
//! scenario — two interactive nodes, one report node, a heavy report burst and steady interactive
//! traffic — and asserts interactive p95 stays within [`RoutingPolicy::interactive_budget`] with
//! routing on, and is exceeded with routing off.
//!
//! # Not here
//!
//! The crate defines the [`ReadPreference`] hint but does not adapt `spot-postgres` to honour it,
//! and it defines [`queues_for`](policy::RoutingPolicy::queues_for) but does not change
//! `spot-jobs`' worker startup to call it; both are follow-ups in the task's open questions. It
//! reads no clock, uses no `unsafe`, and never uses floating-point arithmetic.

#![warn(missing_docs)]
#![forbid(unsafe_code)]
#![deny(clippy::float_arithmetic)]

pub mod class;
pub mod codes;
pub mod error;
pub mod forward;
pub mod metrics;
pub mod middleware;
pub mod node;
pub mod policy;
pub mod queue;
pub mod read;
pub mod route;
pub mod sim;

pub use class::WorkloadClass;
pub use error::{Error, Result};
pub use forward::{
    BoxFuture, DEADLINE_HEADER, Deadline, ForwardMeta, ForwardingClient, HOP_HEADER,
    RecordedForward, RecordingTransport, TARGET_HEADER, Transport,
};
pub use metrics::{CountingMetrics, DecisionKind, Metrics, NoMetrics, RoutingEvent};
pub use middleware::{
    FnSessions, MiddlewareError, NoSessions, RoutingLayer, RoutingService, SessionSource,
    meta_from_parts,
};
pub use node::{Capacity, NodeId, NodeProfile, NodeRole, NodeSelector};
pub use policy::{
    PathMatcher, PathRule, PolicyStore, QueueAffinityRule, ReplicaReadRule, RoutingPolicy,
};
pub use queue::QueueName;
pub use read::{
    ReadDecision, ReadPreference, ReplicaStatus, SessionReadState, SessionToken, choose_read,
};
pub use route::{RequestMeta, RouteDecision, RouteOutcome, RoutingEngine, route};
pub use sim::{SimCompletion, SimConfig, SimOutcome, SimRequest, p95, percentile, simulate};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-workload-routing";
