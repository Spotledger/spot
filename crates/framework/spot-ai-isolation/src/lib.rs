//! AI workload isolation (WP-AI-05.T1; blueprint WS13, ADR-26, ADR-072).
//!
//! Twenty parallel agents must not degrade interactive users. Isolation here is by
//! construction, then proven by a load test with a simulated database:
//!
//! - **[`AiAdmission`]** sits in front of `spot-mcp` (and any other agent call path). It
//!   enforces per-agent and per-tenant concurrency limits, per-agent and per-tenant request
//!   rates (the in-process [`LocalRateGate`], or the cluster-wide coordinator in
//!   [`AiAdmission::admit_cluster`]), per-call and per-session deadlines, and per-call row and
//!   byte limits.
//! - **Human traffic first.** A [`LoadSignal`] reports interactive queue depth, pool wait and
//!   p95 (from `spot-telemetry` in a real deployment); an [`AimdController`] shrinks AI
//!   admission when that load crosses the high thresholds, down to zero, and recovers it
//!   gradually.
//! - **A dedicated pool.** [`AiPoolSettings`] is the `ai` role's connection-pool budget,
//!   declared as four `spot-settings` keys. [`role_creation_sql`] and [`grants_for_schema`]
//!   are the idempotent migration helpers for `spot-pg-migrate` consumers. AI traffic never
//!   shares a pool with human traffic.
//! - **Circuit breakers** per agent identity and per tenant ([`KeyedBreakers`]), with
//!   ADR-072 semantics, visible through [`AiHealthReport`].
//! - **Fairness.** [`WeightedFairQueue`] gives every agent a share, so one agent's burst
//!   cannot starve the others.
//! - **Cancellation.** A cancelled or timed-out call cancels its database statement through a
//!   [`StatementCanceller`] and releases its permit, so no slot leaks.
//!
//! Time is always an argument (a monotonic [`std::time::Duration`]); this crate never reads a
//! clock, never does floating-point arithmetic and never talks to a database. It is the seam
//! `spot-mcp`, the host and the storage layer build on.
//!
//! [`AiAdmission::admit_cluster`]: admission::AiAdmission::admit_cluster

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::float_arithmetic)]

pub mod admission;
pub mod breaker;
pub mod cancel;
pub mod config;
pub mod error;
pub mod fairness;
pub mod health;
pub mod identity;
pub mod load;
pub mod pool;
pub mod rate;

pub use admission::{Admission, AiAdmission, AiRequest};
pub use breaker::{BreakerHealth, BreakerPolicy, BreakerState, KeyedBreakers, Outcome};
pub use cancel::{RecordingCanceller, StatementCanceller, StatementId};
pub use config::{AiLimits, AiPoolSettings, pool_settings_registry, pool_settings_specs};
pub use error::{AiError, Result};
pub use fairness::WeightedFairQueue;
pub use health::AiHealthReport;
pub use identity::{AgentId, MAX_IDENTITY_BYTES, TenantId};
pub use load::{AimdController, LoadLevel, LoadSample, LoadSignal, LoadThresholds, StaticLoad};
pub use pool::{
    AI_ROLE, GRANT_LIST, grants_for_schema, grants_for_schemas, pool_timeouts, role_creation_sql,
};
pub use rate::{LocalRateGate, RateGate, TokenBucket};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-ai-isolation";
