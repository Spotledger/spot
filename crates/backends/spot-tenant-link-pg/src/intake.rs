//! The host intake over the PostgreSQL link store (WP-CTL-10.T7 and a recorded security finding).
//!
//! [`PgLinkIntake`] implements the [`LinkIntake`] port that `spot-link-transport` mounts at
//! `POST /link/v1/handshake` and `POST /link/v1/{link}/revoke`:
//!
//! * a handshake message goes through [`PgLinkStore::claim_inbound`] **before** it reaches the
//!   host's [`HandshakeSink`] (which drives `Handshake::receive` or `from_proposal`). The
//!   `keys` directory and the `Trust` are the host's handshake policy, given once at
//!   construction; nothing is taken from the message;
//! * a revoke goes through [`LinkAdmin::receive_revoke`](crate::LinkAdmin::receive_revoke);
//! * [`prune_nonces`](LinkIntake::prune_nonces) is [`PgLinkStore::prune_nonces`], called by the
//!   host's scheduler through `spot_link_transport::NonceSweep`.
//!
//! Every store error refuses (fail closed). The refusal is coarse (see [`IntakeRefusal`]); no key,
//! signature, nonce or body is in an error, a log line or an audit row of this module.
//!
//! The methods block on the store's runtime, so the port is called from a blocking thread, as the
//! transport's routes do; do not call it from a current-thread runtime.

use std::sync::Arc;

use spot_link_transport::{IntakeRefusal, LinkIntake};
use spot_permissions::Principal;
use spot_tenant_link::{
    AuditSink, ConsentPolicy, Error, HandshakeMessage, LinkId, NonceClaim, Party, PeerKeyDirectory,
    RevokeMessage, StepUpGate, StepUpProof, Trust,
};

use crate::admin::LinkAdmin;
use crate::permissions::{LinkPermissions, Operation};
use crate::store::PgLinkStore;

/// Where a genuine, newly claimed handshake message goes: the host's handshake driver
/// (`Handshake::from_proposal` for a proposal, `Handshake::receive` for the rest).
///
/// The message's nonce is **already claimed** in the store when `deliver` runs, and `claim` is the
/// proof. The driver passes it on (`Handshake::receive_claimed(message, at, Some(claim))` or
/// `from_proposal_claimed`), and a handshake over the store's `PgNonceLog` then records the nonce
/// through `record_claimed`: accepted once, for this nonce and link only. Without the claim the
/// handshake would find its own caller's nonce seen and refuse the message as a replay.
pub trait HandshakeSink: Send + Sync + 'static {
    /// Hands `message`, sent by the authenticated transport `peer`, to the handshake.
    ///
    /// Returning [`IntakeRefusal::Unavailable`] means the message was not processed: the intake
    /// gives its nonce back and kills `claim`, so the peer's resend is not refused as a replay.
    ///
    /// # Errors
    /// [`IntakeRefusal`].
    fn deliver(
        &self,
        peer: &Party,
        message: &HandshakeMessage,
        claim: &NonceClaim,
        at: u64,
    ) -> Result<(), IntakeRefusal>;
}

/// The intake of one tenant's link store.
pub struct PgLinkIntake {
    store: Arc<PgLinkStore>,
    chain: Arc<dyn AuditSink>,
    keys: Arc<dyn PeerKeyDirectory>,
    trust: Trust,
    sink: Arc<dyn HandshakeSink>,
}

impl std::fmt::Debug for PgLinkIntake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PgLinkIntake").finish_non_exhaustive()
    }
}

impl PgLinkIntake {
    /// An intake over `store`, writing refusals of remote revokes to `chain`, screening handshake
    /// messages against `keys` and `trust` (the same objects the handshake policy holds) and
    /// handing the genuine ones to `sink`.
    #[must_use]
    pub fn new(
        store: Arc<PgLinkStore>,
        chain: Arc<dyn AuditSink>,
        keys: Arc<dyn PeerKeyDirectory>,
        trust: Trust,
        sink: Arc<dyn HandshakeSink>,
    ) -> Self {
        Self {
            store,
            chain,
            keys,
            trust,
            sink,
        }
    }
}

/// The coarse answer for a link-layer error. Unknown or unexpected errors are `Forged`: refused,
/// and told nothing more.
fn refusal(error: &Error) -> IntakeRefusal {
    match error {
        Error::Replayed { .. } => IntakeRefusal::Replayed,
        Error::OutOfOrder { .. }
        | Error::NotActive { .. }
        | Error::Revoked { .. }
        | Error::LinkExpired { .. }
        | Error::InvalidTransition { .. } => IntakeRefusal::Stale,
        Error::StoreFailed { .. } | Error::AuditFailed { .. } | Error::VersionConflict { .. } => {
            IntakeRefusal::Unavailable
        }
        _ => IntakeRefusal::Forged,
    }
}

impl LinkIntake for PgLinkIntake {
    fn handshake(
        &self,
        peer: &Party,
        message: &HandshakeMessage,
        at: u64,
    ) -> Result<(), IntakeRefusal> {
        let claim = self
            .store
            .run(
                self.store
                    .claim_inbound(message, self.keys.as_ref(), &self.trust, at),
            )
            .map_err(|error| refusal(&error))?;
        match self.sink.deliver(peer, message, &claim, at) {
            Ok(()) => Ok(()),
            Err(IntakeRefusal::Unavailable) => {
                // Not processed: kill the claim, then give the nonce back so the resend can still
                // be taken. A failure to release does not hide the refusal.
                claim.release();
                if let Some(link) = message.body.link() {
                    let _ = self
                        .store
                        .run(self.store.release_nonce(link, &message.nonce));
                }
                Err(IntakeRefusal::Unavailable)
            }
            Err(other) => Err(other),
        }
    }

    fn revoke(&self, _peer: &Party, message: &RevokeMessage, at: u64) -> Result<(), IntakeRefusal> {
        let admin = LinkAdmin::new(
            self.store.as_ref(),
            self.chain.as_ref(),
            &NoStepUp,
            &NoPermissions,
        );
        admin
            .receive_revoke(message, at)
            .map(|_| ())
            .map_err(|error| match error {
                crate::OpError::Link(error) => refusal(&error),
                crate::OpError::Denied { .. } => IntakeRefusal::Forged,
            })
    }

    fn prune_nonces(&self, now: u64, policy: ConsentPolicy) -> Result<u64, IntakeRefusal> {
        self.store
            .run(self.store.prune_nonces(now, policy))
            .map_err(|error| refusal(&error))
    }
}

/// A step-up gate that confirms nothing: `receive_revoke` needs no consent, and the intake must
/// not be usable for anything that does.
struct NoStepUp;

impl StepUpGate for NoStepUp {
    fn confirm(&self, actor: &str, _proof: &StepUpProof, _at: u64) -> Result<(), Error> {
        Err(Error::StepUpRequired {
            actor: actor.to_owned(),
        })
    }
}

/// Permissions that allow nothing, for the same reason.
struct NoPermissions;

impl LinkPermissions for NoPermissions {
    fn may(&self, _: &Principal, _: Operation, _: Option<&LinkId>) -> bool {
        false
    }
}
