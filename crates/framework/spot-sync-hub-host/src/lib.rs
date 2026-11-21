//! The hub half of sync, joined (WP-RTB-02.T4; ADR-062 section 3, ADR-063 section 5).
//!
//! `spot-sync-http` has the listener, the TLS and the three routes over two traits,
//! [`HubSource`](spot_sync_http::HubSource) and [`PushSink`](spot_sync_http::PushSink);
//! `spot-sync-hub` has the relay that stages the tenant stream into one durable queue per
//! destination node. This crate is the join, in four pieces:
//!
//! - [`cache`]: the **serve cache**. The relay and its PostgreSQL queue are synchronous and may
//!   not be called from an async thread (the bridge refuses it), so a plain thread ([`driver`])
//!   runs the relay and publishes, per destination, the window of queued items as wire records
//!   numbered by **serve sequence**. [`ServeCache`](cache::ServeCache) answers a pull from that
//!   memory, never from the database, and a pull for sequence `n` is the node's
//!   acknowledgement of everything below `n`: it is turned into an
//!   [`AckNote`](cache::AckNote) that trims the queue by serve sequence (the rule of
//!   WP-SYN-11.T2).
//! - [`driver`]: [`HubCore`](driver::HubCore), the narrow port the thread drives (tick, window, ack),
//!   [`RelayCore`](driver::RelayCore), its implementation over a real `HubRelay`, and the driver
//!   loop.
//! - [`sinks`]: [`TenantSinks`](sinks::TenantSinks), the push sink that routes a batch to its
//!   tenant's sink by the batch's source site and refuses a tenant it does not serve.
//! - [`guard`]: the layer in front of the sync router: it re-resolves the node against the live
//!   directory on every request, applies the per-node and global connection limits and the push
//!   rate, lets a node pull only **its own** queue (`<hub site>#<partition>`, the relay's destination id), refuses a hello that
//!   names another site, and audits refusals at most once per node per minute.
//! - [`limits`]: the pure parts of the limits (token bucket, admission, audit throttle).
//!
//! # Wire mapping
//!
//! One queued bundle is one wire record: name `bundle-<serve_seq>`, version 1, sequence the
//! bundle's serve sequence, payload the bundle's JSON. The queue numbers items contiguously per
//! destination, so a batch is always a contiguous run; a gap ends the run.
//!
//! No `unsafe`, no floats, no clock reads (time is an argument or an injected clock), no payload
//! in an error or a log line.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod cache;
pub mod driver;
pub mod enrol;
pub mod grants;
pub mod guard;
pub mod limits;
pub mod sinks;

pub use cache::{AckNote, ServeCache, dest_of};
pub use driver::{HubCore, RelayCore, drive};
pub use guard::{Guard, GuardSettings, RefusalAudit, SeenSink, guard_layer};
pub use limits::{Admission, AuditThrottle, Limits, Permit, Refusal, TokenBucket};
pub use sinks::TenantSinks;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-sync-hub-host";
