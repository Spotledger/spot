//! The handshake messages and the one byte string each signature covers.
//!
//! Five messages carry a link from proposal to activation: `propose`, `accept`, `reject`,
//! `key_exchange` and `activate`. Every message is signed by the sender's tenant identity key
//! (Ed25519, the primitive `spot-agent-enroll` and `spot-bundle-signing` already use).
//!
//! # The signed transcript
//!
//! ```text
//! b"spotledger-link-v1\0" ++ revision ++ optional(previous digest) ++ nonce
//!   ++ signer public key ++ body
//! ```
//!
//! The domain prefix keeps a link signature from verifying over any other protocol. `revision`
//! and `previous` chain each message to the one before it, so an old message cannot be replayed
//! into a later step; the nonce rules out a second, differently packed copy of the same step.
//! The body encoding is hand-written and length-prefixed: a hash never depends on a serde
//! implementation, and changing one field changes the signed bytes.
//!
//! These are plain `serde` types; a transport (WP-CTL-10) carries them and this crate never
//! opens a socket.

use serde::{Deserialize, Serialize};

use crate::consent::Consent;
use crate::error::{Error, Result};
use crate::grant::Offer;
use crate::ids::{Digest, LinkId, Nonce};
use crate::keys::{IdentitySigner, PeerKey, SignatureBytes};
use crate::model::{Carrier, LinkDraft, RevokeReason};
use crate::util::{put_opt_field, put_u32, put_u64};

/// The reason a peer rejects a proposal. A fixed vocabulary, never peer-supplied free text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    /// The terms were declined.
    TermsDeclined,
    /// The grants or fields did not match what this side can offer.
    GrantsMismatch,
    /// The peer identity key is not acceptable.
    KeysUnacceptable,
}

impl RejectReason {
    /// The reason as text.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TermsDeclined => "terms_declined",
            Self::GrantsMismatch => "grants_mismatch",
            Self::KeysUnacceptable => "keys_unacceptable",
        }
    }

    fn tag(self) -> u8 {
        match self {
            Self::TermsDeclined => 0,
            Self::GrantsMismatch => 1,
            Self::KeysUnacceptable => 2,
        }
    }
}

/// Message 1, proposer to peer: the draft link, the proposer's offer and its consent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalBody {
    /// The draft link.
    pub draft: LinkDraft,
    /// The proposer's consent, with its step-up proof.
    pub consent: Consent,
}

/// Message 2, peer to proposer: the peer's offer and its consent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptBody {
    /// The link.
    pub link: LinkId,
    /// The accepting side's offer.
    pub offer: Offer,
    /// The accepting side's consent.
    pub consent: Consent,
}

/// A rejection, at the peer's first turn or at any later turn of the rejecting side.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectBody {
    /// The link.
    pub link: LinkId,
    /// Why.
    pub reason: RejectReason,
}

/// A side publishing the public half of its identity key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyExchangeBody {
    /// The link.
    pub link: LinkId,
    /// The key; it must equal the message signer's key.
    pub key: PeerKey,
}

/// The proposer's final message: the agreed grants digest, so both sides end on the same terms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivateBody {
    /// The link.
    pub link: LinkId,
    /// The `grants_digest` of the agreed grants.
    pub grants: Digest,
}

/// The body of one handshake message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandshakeBody {
    /// `propose`.
    Propose(ProposalBody),
    /// `accept`.
    Accept(AcceptBody),
    /// `reject`.
    Reject(RejectBody),
    /// `key_exchange`.
    KeyExchange(KeyExchangeBody),
    /// `activate`.
    Activate(ActivateBody),
}

impl HandshakeBody {
    /// The message's name.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Propose(_) => "propose",
            Self::Accept(_) => "accept",
            Self::Reject(_) => "reject",
            Self::KeyExchange(_) => "key_exchange",
            Self::Activate(_) => "activate",
        }
    }

    /// The link the message names (`None` only for a malformed body).
    #[must_use]
    pub fn link(&self) -> Option<&LinkId> {
        match self {
            Self::Propose(body) => Some(&body.draft.id),
            Self::Accept(body) => Some(&body.link),
            Self::Reject(body) => Some(&body.link),
            Self::KeyExchange(body) => Some(&body.link),
            Self::Activate(body) => Some(&body.link),
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        match self {
            Self::Propose(body) => {
                out.push(0);
                body.draft.id.encode(out);
                body.draft.local.encode(out);
                body.draft.peer.encode(out);
                out.push(carrier_tag(body.draft.carrier));
                put_u64(out, body.draft.created);
                match body.draft.expires {
                    Some(expires) => {
                        out.push(1);
                        put_u64(out, expires);
                    }
                    None => out.push(0),
                }
                encode_offer(&body.draft.offer, out);
                body.consent.encode(out);
            }
            Self::Accept(body) => {
                out.push(1);
                body.link.encode(out);
                encode_offer(&body.offer, out);
                body.consent.encode(out);
            }
            Self::Reject(body) => {
                out.push(2);
                body.link.encode(out);
                out.push(body.reason.tag());
            }
            Self::KeyExchange(body) => {
                out.push(3);
                body.link.encode(out);
                out.extend_from_slice(body.key.as_bytes());
            }
            Self::Activate(body) => {
                out.push(4);
                body.link.encode(out);
                out.extend_from_slice(body.grants.as_bytes());
            }
        }
    }
}

fn carrier_tag(carrier: Carrier) -> u8 {
    match carrier {
        Carrier::Direct => 0,
        Carrier::Pull => 1,
        Carrier::External => 2,
    }
}

fn encode_offer(offer: &Offer, out: &mut Vec<u8>) {
    offer.side.encode(out);
    put_u32(out, u32::try_from(offer.grants.len()).unwrap_or(u32::MAX));
    for grant in &offer.grants {
        grant.encode(out);
    }
}

/// One signed handshake message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HandshakeMessage {
    /// The position in the exchange: 0 `propose`, 1 `accept`/`reject`, 2 and 3
    /// `key_exchange` (proposer then peer), 4 `activate`.
    pub revision: u64,
    /// The digest of the message before this one; `None` only for revision 0.
    pub previous: Option<Digest>,
    /// The fresh nonce the sender chose for this message.
    pub nonce: Nonce,
    /// The sender's public key.
    pub signer: PeerKey,
    /// The body.
    pub body: HandshakeBody,
    /// The sender's Ed25519 signature over [`HandshakeMessage::signing_bytes`].
    pub signature: SignatureBytes,
}

impl HandshakeMessage {
    /// The exact bytes the sender signed.
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"spotledger-link-v1\0");
        put_u64(&mut out, self.revision);
        put_opt_field(
            &mut out,
            self.previous
                .as_ref()
                .map(|digest| digest.as_bytes().as_slice()),
        );
        out.extend_from_slice(self.nonce.as_bytes());
        out.extend_from_slice(self.signer.as_bytes());
        self.body.encode(&mut out);
        out
    }

    /// The digest of this message, which the next message names as its `previous`.
    #[must_use]
    pub fn digest(&self) -> Digest {
        let mut bytes = self.signing_bytes();
        bytes.extend_from_slice(self.signature.as_bytes());
        Digest::of(&bytes)
    }

    /// Checks the signature against the signer's key.
    ///
    /// # Errors
    /// [`crate::Error::BadSignature`] when it does not verify.
    pub fn verify(&self) -> Result<()> {
        self.signer.verify(&self.signing_bytes(), &self.signature)
    }

    /// Builds and signs the next message.
    pub(crate) fn signed(
        signer: &dyn IdentitySigner,
        revision: u64,
        previous: Option<Digest>,
        nonce: Nonce,
        body: HandshakeBody,
    ) -> Self {
        let mut message = Self {
            revision,
            previous,
            nonce,
            signer: signer.public_key(),
            body,
            signature: SignatureBytes::from_bytes([0u8; 64]),
        };
        message.signature = signer.sign(&message.signing_bytes());
        message
    }
}

/// The grants of an offer in wire order, for tests that compare two sides.
#[doc(hidden)]
#[must_use]
pub fn encoded_offer(offer: &Offer) -> Vec<u8> {
    let mut out = Vec::new();
    encode_offer(offer, &mut out);
    out
}

/// The version of the [`RevokeMessage`] encoding this crate signs and accepts.
pub const REVOKE_VERSION: u8 = 1;

/// The `revision` a [`RevokeMessage`] carries for a link that finished its handshake: the
/// revision of the `activate` message that made it active.
///
/// A revoke is signed for the state of the link it ends. The only state a remote revoke may end
/// is an active link, which both sides reached at this revision; a message naming an earlier
/// revision was signed for a link that was still in handshake and is stale, and one naming a
/// later revision names a state that does not exist. A link still in handshake is ended with the
/// handshake's own `reject`.
pub const REVOKE_ACTIVE_REVISION: u64 = 4;

/// A revocation arriving from the peer: one signed message, not a bare instruction.
///
/// It is signed by the peer's identity key (the key pinned on the link) over
///
/// ```text
/// b"spotledger-link-revoke-v1\0" ++ version ++ link ++ revision ++ reason ++ nonce
///   ++ signer public key
/// ```
///
/// with the same length-prefixed, serde-independent encoding as the handshake messages. The
/// receiver verifies the signature, the link, the pinned key and the revision **before** it
/// claims the nonce or changes any state (see `LinkService::receive_revoke`). The signature
/// proves the revocation came from the holder of the peer's key; who revoked is then the peer
/// party of the link, never a field of the message.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeMessage {
    /// The encoding version; [`REVOKE_VERSION`].
    pub version: u8,
    /// The link being revoked.
    pub link: LinkId,
    /// The link revision this revoke applies to; [`REVOKE_ACTIVE_REVISION`].
    pub revision: u64,
    /// Why, from the fixed vocabulary.
    pub reason: RevokeReason,
    /// The fresh nonce the sender chose; it is claimed once per link.
    pub nonce: Nonce,
    /// The sender's public key.
    pub signer: PeerKey,
    /// The sender's Ed25519 signature over [`RevokeMessage::signing_bytes`].
    pub signature: SignatureBytes,
}

impl RevokeMessage {
    /// Builds and signs a revoke for an active link.
    #[must_use]
    pub fn sign(
        signer: &dyn IdentitySigner,
        link: LinkId,
        reason: RevokeReason,
        nonce: Nonce,
    ) -> Self {
        let mut message = Self {
            version: REVOKE_VERSION,
            link,
            revision: REVOKE_ACTIVE_REVISION,
            reason,
            nonce,
            signer: signer.public_key(),
            signature: SignatureBytes::from_bytes([0u8; 64]),
        };
        message.signature = signer.sign(&message.signing_bytes());
        message
    }

    /// The exact bytes the sender signed.
    #[must_use]
    pub fn signing_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"spotledger-link-revoke-v1\0");
        out.push(self.version);
        self.link.encode(&mut out);
        put_u64(&mut out, self.revision);
        out.push(reason_tag(self.reason));
        out.extend_from_slice(self.nonce.as_bytes());
        out.extend_from_slice(self.signer.as_bytes());
        out
    }

    /// Checks the version and the signature against the signer's key.
    ///
    /// # Errors
    /// [`Error::WrongStep`] for a version this crate does not know,
    /// [`Error::BadSignature`] when the signature does not verify.
    pub fn verify(&self) -> Result<()> {
        if self.version != REVOKE_VERSION {
            return Err(Error::WrongStep {
                expected: "a version 1 revoke",
                found: "a revoke of another version",
            });
        }
        self.signer.verify(&self.signing_bytes(), &self.signature)
    }
}

fn reason_tag(reason: RevokeReason) -> u8 {
    match reason {
        RevokeReason::TermsEnded => 0,
        RevokeReason::SecurityConcern => 1,
        RevokeReason::Superseded => 2,
    }
}
