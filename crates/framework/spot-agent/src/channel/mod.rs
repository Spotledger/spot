//! The outbound channel (WP-ADM-02.T3): the client side of contract `agent-channel` 0.1.0.
//! The agent opens every connection and never listens on a port.
//!
//! - [`transport`]: TLS 1.3, pinned to the gateway trust anchor when a CA file is given and
//!   using the system trust store otherwise (no `insecure` option);
//! - [`sysroots`]: the system trust store, read through an injectable list of locations;
//! - [`client`]: signed `poll`, `report`, `heartbeat`, `fetch_update`, and enrolment;
//! - [`intake`]: the fixed-order pipeline (verify, persist before execute, dispatch,
//!   persist the result, report) with defined crash semantics;
//! - [`store`]: durable command records and the file-backed replay window;
//! - [`backoff`]: jittered exponential reconnect backoff (1 s to 5 min);
//! - [`serve`]: the long-poll loop, revocation and shutdown handling.

pub mod backoff;
pub mod client;
pub mod intake;
pub mod serve;
pub mod store;
pub mod sysroots;
pub mod timefmt;
pub mod transport;

pub use serve::{
    ChannelStatus, LoopCtx, LoopEnd, build_transport, enrol_gateway, run_loop, serve, status_lines,
};
