//! The host agent's execution core (WP-ADM-03.T1, blueprint WS22, ADR-44).
//!
//! This is the part of the host agent that decides **whether a command may run**. The
//! acceptance is blunt: an unsigned or single-signed dangerous command is refused even
//! from a compromised console. Everything is decided locally from keys the agent already
//! holds; the console and the network are never trusted.
//!
//! # The decision
//!
//! [`Agent::handle`] takes a wire [`CommandEnvelope`] and the current instant and returns
//! a [`Decision`]: either [`Decision::Execute`] carrying an [`ExecutionPlan`], or
//! [`Decision::Refuse`] carrying a [`Refusal`]. Time is always an argument: this crate
//! never reads a clock, so a test and an offline agent drive the same code.
//!
//! # The verification chain
//!
//! [`verify::Evaluator`] runs the chain in a fixed order: the envelope decodes into a
//! canonical [`spot_signed_command::SignedCommand`]; the operation is resolved in the
//! [`spot_mgmt_ops::Catalogue`] so its risk level (and therefore the number of required
//! administrator signatures) comes **from the catalogue, never from the envelope**; the
//! signature count is enforced by the signed-command verifier against the agent's
//! [`TrustStore`] of administrator keys; the audience, freshness window and replay nonce
//! are checked; the arguments are validated against the catalogue schema; the two-person,
//! hardware-key and critical-operation rules are applied; and only then is a plan built.
//!
//! # Privilege separation
//!
//! The two roles are modelled in code. [`Broker`] is unprivileged: it owns the network
//! session, decodes messages and runs verification, and produces a [`PlanEnvelope`] — the
//! original signed command plus a plan it signs with its own key. [`Executor`] is
//! privileged: it receives only a plan over a [`LocalChannel`], independently re-derives
//! the plan from the carried command and refuses anything that is not the deterministic
//! plan of a validly signed command. A compromised broker can therefore ask only for what
//! a valid signed command already authorised.
//!
//! # The result log
//!
//! Every decision and every execution result is appended to a local hash-chained log
//! ([`ResultLog`]) built from the [`spot_audit_chain`] API, carrying the command digest,
//! the signers and the outcome. The log rides back with the result and is verifiable
//! customer-side ([`MemoryResultLog::verify`], or the chain's own verifier).
//!
//! # Purity
//!
//! Sans-I/O: time, the nonce store, the administrator key store, the critical-operation
//! context and the log are all injected traits. No shell is ever spawned (a source-scan
//! test enforces that), no `unsafe` (the project coding rules), no floats, and no secret is
//! ever logged or returned in a `Debug`.

#![forbid(unsafe_code)]

pub mod agent;
pub mod attestation;
pub mod channel;
pub mod envelope;
pub mod error;
pub mod executor;
pub mod handler;
pub mod log;
pub mod plan;
pub mod policy;
pub mod trust;
pub mod verify;

pub use agent::{Agent, Decision};
pub use attestation::{AdminAttestation, KeyAttestation, VendorAttestation};
pub use channel::{InProcessChannel, LocalChannel};
pub use envelope::{CommandEnvelope, EnvelopeSignature};
pub use error::{CoreError, Refusal};
pub use executor::{
    Broker, ExecutionOutcome, ExecutionStatus, Executor, ExecutorBuilder, PlanEnvelope,
};
pub use handler::{HandlerOutcome, HandlerRegistry, OperationHandler};
pub use log::{AuditExecutor, ChainResultLog, DecisionRecord, MemoryResultLog, ResultLog};
pub use plan::{Digest, ExecutionPlan, PlanParts};
pub use policy::{Clearance, CriticalContext, CriticalRequest, DelayOnly, NoCritical, Policy};
pub use trust::{AdminKey, TrustStore};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-agent-core";
