//! L8 fault injection, framework cut (WP-TST-08.T2).
//!
//! L8 promises *recovery after crash, partition, power loss*. The platform's central safety
//! claims all depend on it: a committed transaction survives, an uncommitted one leaves nothing,
//! a number is never skipped or reused, no business event is lost or duplicated, and a client
//! that did not hear back from a commit can find out what happened. This crate attacks those
//! claims on a real PostgreSQL that the harness starts, kills and restarts itself, using the L7
//! invariants ([`spot_l7_invariants`], [`spot_l7_engine`]) and the isolation checker
//! ([`spot_l8_isolation`]) as the judges.
//!
//! # What is here
//!
//! - [`cluster::ScratchCluster`]: a disposable PostgreSQL cluster in a temp directory on a free
//!   port, started through `spot-pg-supervisor` with crash-relevant settings (`fsync = on`,
//!   `synchronous_commit = on`, small `shared_buffers`, `max_connections = 300`). It can be
//!   stopped gracefully, killed with `SIGKILL` (postmaster **and** its children) and restarted
//!   with the recovery time recorded. It refuses to run as root and cleans up on drop.
//! - [`proxy::FaultProxy`]: an in-process TCP proxy between the backend and PostgreSQL with
//!   scriptable faults per connection and per direction — `pass`, `delay(ms)`,
//!   `drop_after_bytes(n)`, `reset_now`, `blackhole`, `half_close` and
//!   `reset_when_message(type)` (cut exactly when the commit message goes out, or just after the
//!   server's reply to it). It reads only PostgreSQL message type bytes and lengths
//!   ([`wire`]), never query text.
//! - [`scenarios`]: the fault scenarios (crash during load, repeated crashes, killed
//!   connections, commit-ack loss, partition, slow storage, the job engine). Each runs a
//!   workload, applies a fault, recovers, and then checks **all L7 invariants** plus the
//!   [`audit::DurabilityAudit`].
//! - [`audit::DurabilityAudit`]: partitions the operations into acknowledged, refused,
//!   indeterminate-resolved-committed and indeterminate-resolved-absent, asserts the durability
//!   rules and emits a table.
//! - [`honesty`]: seeded failures of the harness itself, so a green run means something.
//!
//! # Honest limits
//!
//! **Power loss cannot be simulated on a CI machine.** This crate covers process kill (SIGKILL),
//! connection loss, network faults and restart with crash recovery. It does **not** cover power
//! loss, disk faults or torn pages: those belong to WP-PRD-05 and a physical rig. A test started
//! with `fsync = off` and `synchronous_commit = off` is `#[ignore]`d and exists only to show that
//! the audit *can* fail; it is never part of the green suite.
//!
//! The scenarios need the bundled PostgreSQL 18 binaries. Without them every scenario prints
//! `SKIPPED` and passes; with `SPOT_REQUIRE_PG=1` a missing server is a failure instead. The
//! binaries are found through `SPOT_PG_BIN` or `/usr/lib/postgresql/18/bin`.
//!
//! # Reproducing a failure
//!
//! Every fault plan is a pure function of its seed, and the fault timeline is recorded with
//! times. To replay a failure, run the same scenario with the same seed:
//!
//! ```text
//! SPOT_L8_FAULTS_PROFILE=smoke SPOT_PG_BIN=/usr/lib/postgresql/18/bin \
//!   cargo test -p spot-l8-faults -- --nocapture
//! ```
//!
//! [`spot_l7_invariants`]: https://example.invalid/spot-l7-invariants
//! [`spot_l7_engine`]: https://example.invalid/spot-l7-engine
//! [`spot_l8_isolation`]: https://example.invalid/spot-l8-isolation

pub mod audit;
pub mod cluster;
pub mod faults;
pub mod honesty;
pub mod jobs;
pub mod profile;
pub mod proxy;
pub mod report;
pub mod scenarios;
pub mod wire;

pub use audit::{AuditReport, CommitClass, DurabilityAudit};
pub use cluster::{ClusterConfig, ClusterError, ScratchCluster};
pub use faults::{
    ConnSelector, Direction, Fault, FaultEvent, FaultTimeline, MessageTrigger, ScheduledFault,
    Trigger,
};
pub use profile::{Profile, profile_from_env, skip_or_fail};
pub use proxy::{FaultProxy, ProxyConfig};
pub use report::FaultReport;
pub use scenarios::{ScenarioConfig, ScenarioError, ScenarioResult};
pub use wire::{Frame, FrameKind, FrameScanner, PgMessage, WireError};
