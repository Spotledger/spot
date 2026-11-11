//! `spot-dag-exec` — the DAG execution layer of the Spotledger Platform (WS8, WP-DAG-02,
//! ADR-13).
//!
//! [`spot_dag`] defines and compiles a DAG; this crate records *where a run of it is*. It is
//! the pure half and the storage half of that:
//!
//! * [`plan`] is a pure state machine over a compiled graph: [`start`] pins a published version
//!   and readies the entry nodes, [`claim`] takes a ready node for one attempt, [`complete`]
//!   applies an outcome (honouring the join rules and resolving dead branches into
//!   [`NodeStatus::Skipped`]), and [`cancel`] stops a run. Each returns a [`Transition`], never
//!   touching storage.
//! * [`ExecStore`] is where runs live: [`InMemoryExecStore`] is the reference, and
//!   [`contract::run`] is the backend-independent suite the PostgreSQL store of WP-DAG-02.T2
//!   must also pass. [`ExecStore::apply`] is idempotent per `(run, node, attempt)` and
//!   version-checked.
//! * [`rules`] is the single definition of a transition: [`rules::apply_step`] and
//!   [`rules::already_applied`], which every store calls instead of keeping its own copy.
//! * [`Unit::commit_with`] applies a transition in the **same storage transaction** as the
//!   caller's document change, so state and document commit or roll back together.
//!
//! The crate reads no clock and holds no random source: run ids and instants are arguments. The
//! business process engine (WP-BPM-02) sits on this.
//!
//! # Example
//!
//! ```
//! use spot_dag::{CompiledDag, DagDefinition, Limits, Node, NodeId, JoinRule, Published, Edge};
//! use spot_dag_exec::{start, RunId};
//! use spot_app_api::prelude::OffsetDateTime;
//!
//! let def = DagDefinition::new(
//!     "approval",
//!     1,
//!     vec![
//!         Node::new(NodeId::new("draft").unwrap(), "step", JoinRule::All, serde_json::Value::Null).unwrap(),
//!         Node::new(NodeId::new("posted").unwrap(), "step", JoinRule::All, serde_json::Value::Null).unwrap(),
//!     ],
//!     vec![Edge::new(NodeId::new("draft").unwrap(), NodeId::new("posted").unwrap(), None)],
//! );
//! let dag = CompiledDag::compile(&def, &Limits::new()).unwrap();
//! let pin = Published { version: 1, hash: dag.hash().clone() };
//! let now = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
//! let plan = start(&dag, &pin, RunId::new("run1").unwrap(), "order-1", now).unwrap();
//! assert_eq!(plan.ready, vec![NodeId::new("draft").unwrap()]);
//! ```

pub mod contract;
pub mod error;
pub mod instant;
pub mod memory;
pub mod plan;
pub mod rules;
pub mod state;
pub mod store;
pub mod unit;

#[cfg(test)]
mod testutil;

pub use error::{ExecError, ExecResult};
pub use instant::Instant;
pub use memory::InMemoryExecStore;
pub use plan::{cancel, claim, complete, start};
pub use state::{
    Completion, NodeOutcome, NodeState, NodeStatus, Run, RunId, RunPlan, RunStatus, Step,
    TakenEdge, Transition,
};
pub use store::{ExecFuture, ExecStore, ExecStoreFactory, ReadyRun};
pub use unit::Unit;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-dag-exec";
