//! Break-glass access for platform support (WP-ADM-11.T1, blueprint WS22, ADR-44, ADR-070
//! section 9).
//!
//! Break-glass is the vendor's access to the customer: just-in-time, two-person approved,
//! recorded, visible to the customer and expiring on its own. It is the one place raw
//! shell exists — the exception to "typed operations only" — so every session is recorded
//! and the customer can read who, why, when and what happened from their own audit chain.
//!
//! # Shape
//!
//! - [`session`]: the pure state machine `Request -> Approved -> Active -> Ended |
//!   Expired | Revoked`, composing [`spot_deep_access::DeepCredential`] for the
//!   customer-signed, hardware-bound grant and [`spot_support_session::SessionGrant`] for
//!   the scoped, time-boxed one. [`session::SessionKiller`] stops the terminal at the
//!   exact expiry instant, enforced by the monotonic agent clock.
//! - [`policy`]: the resolved tenant policy: opt-in, two distinct vendor-side approvers,
//!   customer consent when required, and a duration cap that a tenant setting may narrow
//!   but never widen.
//! - [`request`]: the request and the composition helpers that turn it into the two grant
//!   models, plus the approver and consent principals.
//! - [`recorder`]: the [`recorder::Recorder`] trait, [`recorder::MemoryRecorder`] and
//!   [`recorder::FileRecorder`]: a hash-chained, append-only frame log (input, output,
//!   window-size changes), with a manifest, tamper detection, a size cap with an explicit
//!   truncation marker, redaction of no-echo input, and [`recorder::replay`] for the
//!   customer's viewer.
//! - [`audit`]: one customer-visible event per transition, written to the tenant's
//!   tamper-evident chain through the same executor boundary deep-access uses.
//! - [`customer`]: [`customer::CustomerView`] (sessions from the audit chain) and
//!   [`customer::RecordingArchive`] (the append-only recordings; the vendor cannot delete
//!   them).
//! - [`pty`]: the [`pty::Pty`] trait and [`pty::FakePty`]; the real terminal is behind the
//!   `real-pty` feature ([`real_pty`]) and is not part of acceptance.
//!
//! # Purity
//!
//! No clock is read (every instant is an argument), no float is used, no `unsafe`, and no
//! `unwrap`/`expect` outside tests. The state machine and the audit/event logic are pure;
//! the file recorder is the one component that touches the file system, because the brief
//! requires an append-only recording on disk.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![deny(clippy::float_arithmetic)]

pub mod audit;
pub mod customer;
pub mod error;
pub mod policy;
pub mod pty;
pub mod recorder;
pub mod request;
pub mod session;

#[cfg(feature = "real-pty")]
pub mod real_pty;

pub use audit::{BreakGlassEvent, ChainLog, Executor, MemoryLog, SessionLog, audit};
pub use customer::{CustomerView, RecordingArchive, SessionSummary};
pub use error::{ALLOW, BreakGlassError};
pub use policy::Policy;
pub use pty::{FakePty, Pty};
pub use recorder::{
    FileRecorder, Frame, FrameKind, Hash, Manifest, MemoryRecorder, REDACTED, Recorder, Replay,
    genesis_hash, hex, read_manifest, replay, replay_bytes, verify_recording,
};
pub use request::{Approval, CustomerConsent, Principal, PrincipalKind, Request};
pub use session::{BreakGlassSession, ExpiryOutcome, Grant, RecordingKiller, SessionKiller, State};
