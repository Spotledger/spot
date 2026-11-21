//! L8 isolation checker (WP-TST-08.T1).
//!
//! The blueprint's L8 promise is *isolation level as claimed* ([`platform.md`] §3). Every
//! transaction in the storage contract **declares** [`Isolation::ReadCommitted`],
//! [`Isolation::RepeatableRead`] or [`Isolation::Serializable`] and says what each means
//! (ADR-46, `contracts/storage/src/tx.rs`). This crate asks whether the backends actually
//! deliver it: it generates transactional histories on real backends, records what every
//! transaction read and wrote, and decides **from the history alone** whether an anomaly the
//! declared level forbids occurred.
//!
//! [`Isolation::ReadCommitted`]: spot_storage_contract::Isolation::ReadCommitted
//! [`Isolation::RepeatableRead`]: spot_storage_contract::Isolation::RepeatableRead
//! [`Isolation::Serializable`]: spot_storage_contract::Isolation::Serializable
//! [`platform.md`]: docs/blueprint/platform.md
//!
//! # How it works
//!
//! The approach is the one Jepsen's Elle uses: choose workloads whose values make the order of
//! writes recoverable, then search for cycles in the inferred dependency graph.
//!
//! 1. [`workload`] runs a seeded, multi-worker mix of transactions on a backend at a declared
//!    level. Each transaction is a small bundle of micro-operations on a test entity's JSON
//!    list field or integer register field.
//! 2. [`checker::check`] rebuilds, per key, the version order (the order writes to a key were
//!    applied), then the Adya dependency edges between committed transactions: `ww`
//!    (write-write), `wr` (write-read), `rw` (anti-dependency) and `rt` (real time). It names
//!    each anomaly it finds with a minimal, explainable witness.
//! 3. [`levels`] maps a claimed level to the anomaly classes it forbids, so an anomaly a level
//!    *allows* is reported but never as a failure.
//!
//! # Honest limits
//!
//! The checker **finds anomalies in the histories it saw**; it never *proves* isolation. The
//! absence of anomalies is evidence, not proof. Nothing here is a formal verifier, and a run
//! covers one schedule of one workload at one level.
//!
//! The definition of each anomaly and the version-order inference are documented in the
//! modules that implement them ([`anomaly`], [`versions`]) and cite Adya, Liskov and O'Neil,
//! *Generalized Isolation Level Definitions* (ICDE 2000), Bailis et al., *Highly Available
//! Transactions*, and Kingsbury and Alvaro, *Elle: Inferring Isolation Anomalies from
//! Experimental Observations* (VLDB 2021).
//!
//! # The history format
//!
//! [`history`] extends the documented, versioned JSON line format of `spot-l7-invariants`
//! (WP-TST-07.T1) with the operations an isolation workload needs. The header keeps the same
//! fields (`format`, `version`, `seed`, `workers`, `profile`) and adds the workload and key
//! count; every entry keeps the same `worker`, `seq`, `start_us`, `end_us`, `isolation`,
//! `outcome` and `attempts` fields, reusing [`spot_l7_invariants::history::Level`] and
//! [`spot_l7_invariants::history::Outcome`] verbatim, and adds the transaction's `ops`. The
//! seeded generator ([`spot_l7_invariants::schedule::Rng`]) and the retry policy
//! ([`spot_l7_invariants::driver::RetryPolicy`]) are reused as they are.
//!
//! # Backends and weakness proofs
//!
//! [`backends`] runs the matrix (the in-memory fake and PostgreSQL, each at the three levels)
//! and checks it. [`weak`] wraps the fake and deliberately breaks one isolation rule at a
//! time; a checker that could not see those anomalies would prove nothing.
//!
//! # Profiles
//!
//! `SPOT_L8_PROFILE=smoke|standard|nightly` selects the run's size, and the `spot-l8` binary
//! runs any profile or checks an existing history file:
//!
//! ```text
//! spot-l8 run --backend fake|pg --level rc|rr|ser --workload list-append|rw-register \
//!            --seed N --txns N --workers W --report out.json
//! spot-l8 check history.jsonl --claimed LEVEL
//! ```

pub mod anomaly;
pub mod backends;
pub mod checker;
pub mod deps;
pub mod graph;
pub mod history;
pub mod levels;
pub mod model;
pub mod report;
pub mod versions;
pub mod weak;
pub mod workload;

pub use anomaly::{Anomaly, AnomalyClass, Witness};
pub use checker::check;
pub use history::{Observed, TxnEntry, TxnHistory, TxnOp};
pub use levels::anomalies_forbidden;
pub use report::{IsolationReport, Verdict};
