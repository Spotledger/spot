//! `spot-idle-connections` (WP-PLT-01.T2): does one `spot` node hold 10,000 idle connections
//! without degrading the requests of the busy ones?
//!
//! The harness starts the real `spot-http` server (router, site resolution, health endpoints,
//! accept loop) with the realtime WebSocket route and a fake service route in this process, opens
//! N idle connections of three kinds (HTTP/1.1 keep-alive, HTTP/2, authenticated WebSocket in a
//! room) in configurable proportions, and measures:
//!
//! - resident memory before and after, and open descriptors;
//! - the time to establish the last connection, and the accept errors the server logged;
//! - the latency (p50, p95, p99) of a steady request rate from busy clients on separate
//!   connections, on the empty server and with the idle connections held;
//! - shutdown: how long until every connection is closed, and whether any task leaked.
//!
//! The ceilings are in `budgets.toml` (see `docs/process/idle-connections.md`). The run is judged by
//! [`verdict::evaluate`], a pure function with its own tests; [`run::run`] does the work and
//! returns a [`report::Report`] that serialises to JSON.
//!
//! This is test code: no production crate depends on it. It measures with monotonic clocks
//! (`Instant`) because latency is what it measures; it reads no wall clock.

#![forbid(unsafe_code)]

pub mod client;
pub mod config;
pub mod error;
pub mod events;
pub mod measure;
pub mod report;
pub mod run;
pub mod server;
pub mod stats;
pub mod verdict;

pub use config::{Budgets, Mix, RunConfig};
pub use error::HarnessError;
pub use report::{Outcome, Report};
pub use run::run;
