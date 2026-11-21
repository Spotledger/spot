//! Agent self-update (WP-ADM-03.T3, contract `agent-channel` 0.1.0, ADR-099).
//!
//! The agent updates itself through `spot-updater`: the signed manifest, the trust anchor
//! with validity windows, the revocation list, the archive guards, staging, the pointer
//! switch, rollback and crash resume all come from that crate. This module is the agent's
//! half: where the offer comes from ([`source`]), how it is read from the wire ([`wire`]),
//! the update trust anchor and the persisted revocation list ([`trust`]), how the agent's
//! own process is quiesced, probed and restarted ([`service`]), and the run that joins them
//! ([`runner`]).
//!
//! # What is refused, and where
//!
//! - **Unsigned**, a signature by a key outside its window, a revoked key, a bad signature,
//!   a stale revocation list: `spot_updater::verify_update`, run twice (before the download,
//!   so nothing is fetched for an unsigned offer, and again inside `UpdateEngine::apply`,
//!   which no caller can skip).
//! - **Older than installed**: `spot_updater::decide` refuses every downgrade. The contract
//!   allows "a signed rollback"; the updater supports no signed downgrade (its module docs
//!   say rollback is the pointer switch back to the last good version), so the agent never
//!   installs an older version at all. A fleet-wide rollback is a newer version number that
//!   carries the old code.
//! - **Archive**: digest and size, entry paths (`..`, absolute, symlink, case duplicates),
//!   compression bombs, unlisted and missing entries: `spot_updater::verify_archive`.
//! - **Another product, a channel the policy does not allow, another operating system, a
//!   schema epoch other than 0**: the agent has no database, so a manifest that asks for a
//!   migration is not an agent update.
//!
//! # Trust anchor
//!
//! The update trust anchor is a file the package installs next to the binary ([`trust`]); it
//! is never fetched from the gateway and no setting turns the signature check off. The
//! revocation list may travel with the offer (an additive `revocations` field) or sit in the
//! state directory; the newest list whose issuer is trusted and whose signature verifies
//! wins, and the generation the agent has held only ever goes up.
//!
//! # Self-update without killing the updater
//!
//! The running agent is the service, so the engine's `stop`, `start` and `wait_ready` are
//! realised by [`service::AgentService`]: `stop` only pauses new commands, `start(new)`
//! launches the new binary in probe mode (a short run that sends a heartbeat carrying its
//! own version over the pinned channel and exits 0), and `wait_ready` waits for that probe
//! until the engine's deadline. The old process keeps running until the new version has
//! proven itself; on success the engine commits and [`runner::SelfUpdater::restart`] asks
//! the service manager to restart into the new `current`. If the probe fails or the deadline
//! passes, the engine rolls back, the agent resumes commands and the result is reported as
//! [`CODE_ROLLED_BACK`].
//!
//! # The schedule
//!
//! [`schedule`] runs [`UpdateAction::check`] on an interval (`update_check_hours`, default 6,
//! with jitter) from the channel's `serve`, logging and auditing each result; a failed,
//! refused or panicking check never stops the agent. [`host::HostUpdate`] is the production
//! action. After an applied update the loop waits for a safe point (no accepted but
//! unreported command) and asks the service manager to restart into the new version.
//!
//! # Crash safety
//!
//! The engine journals every step before and after it runs. At start-up the agent calls
//! [`runner::SelfUpdater::boot`], which lets the engine finish or undo an interrupted update.
//!
//! Nothing here reads the wall clock for a decision (`now` is an argument), uses floats,
//! `unsafe`, `unwrap` or `expect`, or puts a key, token or payload in an error.

pub mod error;
pub mod host;
pub mod probe;
pub mod runner;
pub mod schedule;
pub mod service;
pub mod source;
pub mod trust;
pub mod wire;

pub use error::UpdateError;
pub use host::{HostParts, HostUpdate};
pub use runner::{SelfUpdater, UpdateConfig, UpdateReport, UpdateStatus};
pub use schedule::{UpdateAction, UpdateLoop, UpdateLoopEnd, run_update_loop};
pub use service::{
    AgentService, Probe, ProbeState, ProcessProbe, Quiesce, QuiesceFlag, Restarter, SystemRestarter,
};
pub use source::{ChannelSource, DirSource, UpdateSource};
pub use trust::{FileRevocationStore, MemoryRevocationStore, RevocationStore, load_anchor};
pub use wire::{Offer, parse_offer};

/// Result code: the new version did not come up healthy and the previous version is serving.
pub const CODE_ROLLED_BACK: &str = "agent.update_rolled_back";
/// Result code: no update is published for this agent and platform (contract code).
pub const CODE_UNAVAILABLE: &str = "agent.update_unavailable";
/// Result code: the offer was refused before anything on disk changed.
pub const CODE_REFUSED: &str = "agent.update_refused";
/// Result code: the update could not run (network, layout, trust files).
pub const CODE_FAILED: &str = "agent.update_failed";
/// Result code: neither the update nor its rollback completed; an operator must look.
pub const CODE_NEEDS_ATTENTION: &str = "agent.update_attention";
