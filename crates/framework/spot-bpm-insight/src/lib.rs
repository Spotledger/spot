//! `spot-bpm-insight` — process insight from Case events (WS8, WP-BPM-11.T1, ADR-49).
//!
//! A **Process** is a map of steps; a **Case** is one journey through it. The engine records
//! every node attempt (`started`, `waiting`, `done`, `failed`, with instants); this crate turns
//! that event feed into the numbers an operator reads: how long each step takes, how much of it
//! is waiting rather than work, which step is the bottleneck, and which runs are stuck.
//!
//! Everything here is a **pure aggregation**: no storage, no clock (every instant is an
//! argument), no float (ADR-12 — durations and percentiles are exact integers) and no `unsafe`.
//! The same crate runs over a backfill of stored events and over a live feed, because the fold
//! is incremental ([`Accumulator::push`]) and a feed pushed in chunks gives exactly the result
//! of the whole feed.
//!
//! # The parts
//!
//! - [`event`]: [`NodeRunEvent`] — the crate's only input, one state transition of one node
//!   attempt, derived by the host from the engine's durable node-run records.
//! - [`accumulator`]: [`Accumulator`] folds a feed into an [`Insight`]; [`aggregate`] is the
//!   one-shot form. [`Insight::steps`] is the per-process-version-and-node [`StepStats`].
//! - [`histogram`] and [`state`]: the fixed-bucket cycle-time histogram that bounds memory, and
//!   the versioned, serialisable [`AccumulatorState`] a host stores as its checkpoint. Memory is
//!   `O(steps x buckets)` plus `O(open attempts)`; see [`accumulator`] for the exact statement.
//! - [`stats`]: [`StepStats`] — count, exact p50/p90/p99, waiting versus work time, retry and
//!   failure rates.
//! - [`bottleneck`]: [`bottlenecks`] ranks steps by their share of cycle time and by queue
//!   growth; the thresholds are [`BottleneckPolicy`] data and the explanation string is stable.
//! - [`stuck`]: [`stuck_cases`] names the runs not progressing past a per-node expected
//!   duration, with the reason and the owner.
//! - [`access`]: [`ProcessAccess`] — aggregates respect the viewer's process-level read
//!   permission; a process the viewer may not read is absent, never a row of zeros.
//! - [`report`]: the [`ReportDef`](spot_reports::ReportDef)s the report engine serves, so
//!   insight shows up as ordinary reports (cycle time by step, stuck queue, throughput by week).
//! - [`labels`]: the run-label pair (ADR-080). No user-visible string this crate produces says
//!   the internal word "Case"; titles and headings come from [`RunLabels`].
//!
//! # Example
//!
//! ```
//! use spot_bpm_engine::{CaseId, NodeKind};
//! use spot_bpm_insight::{AttemptRef, Millis, NodeRunEvent, ProcessKey, aggregate};
//! use spot_dag::NodeId;
//! use spot_app_api::prelude::OffsetDateTime;
//!
//! let at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
//! let later = OffsetDateTime::from_unix_timestamp(1_700_000_030).unwrap();
//! let attempt = AttemptRef::new(
//!     ProcessKey::new("order_flow", 1),
//!     CaseId::new("case1").unwrap(),
//!     NodeId::new("approve").unwrap(),
//!     1,
//!     NodeKind::Decision,
//! );
//! let events = vec![
//!     NodeRunEvent::started(attempt.clone(), at),
//!     NodeRunEvent::done(attempt, later),
//! ];
//! let insight = aggregate(events);
//! assert_eq!(insight.steps().len(), 1);
//! assert_eq!(insight.steps()[0].done, 1);
//! assert_eq!(insight.steps()[0].p50.map(Millis::as_millis), Some(30_000));
//! ```

#![forbid(unsafe_code)]

pub mod access;
pub mod accumulator;
pub mod bottleneck;
pub mod duration;
pub mod error;
pub mod event;
pub mod histogram;
pub mod labels;
pub mod percentile;
pub mod process;
pub mod report;
pub mod state;
pub mod stats;
pub mod stuck;

mod wire;

pub use access::{AllowList, ProcessAccess, ReadAll, ReadNone};
pub use accumulator::{Accumulator, Insight, OpenAttempt, aggregate, aggregate_for};
pub use bottleneck::{Bottleneck, BottleneckPolicy, bottlenecks};
pub use duration::Millis;
pub use error::{InsightError, InsightResult};
pub use event::{AttemptRef, NodeRunEvent, RunState, WaitReason};
pub use histogram::{BUCKETS, Histogram, HistogramState};
pub use labels::RunLabels;
pub use percentile::percentile;
pub use process::{ProcessKey, StepKey};
pub use report::{
    CYCLE_TIME_BY_STEP, NODE_RUN_DOCTYPE, RUN_DOCTYPE, STUCK_QUEUE, THROUGHPUT_BY_WEEK,
    report_definitions,
};
pub use state::{AccumulatorState, EndedState, OpenState, STATE_VERSION, StepState};
pub use stats::{Rate, StepStats};
pub use stuck::{StuckCase, StuckPolicy, StuckReason, stuck_cases};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-bpm-insight";
