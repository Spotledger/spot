//! `spot-bpm-engine` — the durable Business Process engine (WS8, WP-BPM-02.T1, ADR-49).
//!
//! A **Process** is a map of steps across documents and systems; a **Case** is one journey
//! through it. This crate is the engine that runs a Case: it is pure orchestration over traits,
//! with the reference in-memory implementations and a deterministic simulation harness. The
//! PostgreSQL store and the scheduler-backed timer port are the host's to provide.
//!
//! # The step loop
//!
//! [`Engine::step`] loads a Case and its pinned [`ProcessDefinition`], fires due timers, awaits
//! child Cases, resumes nodes a crash left running, then claims and runs the ready nodes. A node
//! runs through its [`NodeHandler`], which returns a [`NodeOutcome`]: success, a wait, a retry
//! or a failure.
//!
//! # Idempotency
//!
//! Each attempt has an [`IdempotencyKey`] of `(case, node, attempt)`. The engine stores the
//! attempt's outcome **before** acknowledging the DAG transition, so a replay after a crash
//! returns the stored outcome without calling the handler again. Only a `started, no outcome`
//! attempt is re-run, and then [`NodeCtx::is_retry`] is `true` so the handler knows to dedupe.
//! A handler that performs a side effect must key it by [`NodeCtx::key`].
//!
//! # Layers
//!
//! The engine compiles a Process onto `spot-dag` ([`ProcessDefinition::compile`]) and stores
//! every run in `spot-dag-exec`; it never re-implements the DAG layer. Retries use
//! `spot-jobs`' deterministic [`Backoff`](spot_jobs::Backoff); waits register with a
//! [`TimerPort`] the host wires to `spot-scheduler`; and a host runs a step as a `spot-jobs` job
//! through the [`jobs`] adapter. Case state lives only in the [`CaseStore`].
//!
//! # Example
//!
//! ```
//! use spot_bpm_engine::{CaseId, HandlerRegistry, Harness, NodeKind, RecordingHandler};
//!
//! let ledger = std::sync::Arc::new(spot_bpm_engine::EffectLedger::new());
//! let mut handlers = HandlerRegistry::new();
//! handlers.register(RecordingHandler::new(NodeKind::Document, ledger.clone()));
//! let harness = Harness::new(handlers);
//! assert_eq!(harness.clock().now(), spot_bpm_engine::harness::default_now());
//! let _ = CaseId::new("case1").unwrap();
//! ```

#![forbid(unsafe_code)]

pub mod case;
pub mod engine;
pub mod error;
pub mod expr;
pub mod handler;
pub mod harness;
pub mod jobs;
pub mod logic;
pub mod store;
pub mod timer;

/// The node execution internals of [`engine`]; it adds inherent methods to [`Engine`], so it is
/// private and re-exported through the engine module.
mod exec;

pub use case::{Attempt, AttemptState, CaseRecord, CaseUpdate, ChildLink, TimerRecord};
pub use engine::{
    EffectPoint, Engine, Faults, InMemoryEngine, NoFaults, ProcessDefinition, ProcessRegistry,
    StepReport, in_memory_engine,
};
pub use error::{EngineError, EngineResult};
pub use handler::{
    CaseId, ERROR_CONDITION, HandlerRegistry, IdempotencyKey, LogicKind, NodeCtx, NodeHandler,
    NodeKind, NodeOutcome, OutgoingEdge, TriggerHandler, WaitSpec,
};
pub use harness::{
    EffectLedger, FlakyHandler, Harness, RecordingHandler, ScriptedFaults, VirtualClock,
};
pub use jobs::{CASE_JOB_NAME, CaseJob, CaseJobQueue, RecordingQueue, to_new_job};
pub use store::{
    CaseFuture, CaseStore, CaseStoreFactory, InMemoryCaseStore, InMemoryCaseStoreFactory,
};
pub use timer::{InMemoryTimerPort, NoTimers, TimerPort, TimerRegistration};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-bpm-engine";
