//! The tenant link core (WP-CTL-09.T1, ADR-058 rules 3 and 6 to 14, ADR-080 section 5).
//!
//! A link is a relationship between exactly two parties — two tenants, or a tenant and an
//! external peer. It is a **platform** object: the platform carries agreed topics and fields and
//! never interprets them. The control plane records that a link exists; it never carries link
//! data (T2 stores the record in each tenant's own schema).
//!
//! # Pieces
//!
//! - [`model`]: [`Link`], [`Party`], [`Carrier`], [`LinkState`] and [`LinkDraft`].
//! - [`grant`]: [`Grant`] (topic, kind, direction, schema range, [`FieldList`], expiry,
//!   peer-region requirement, notice), [`grants_match`] (the intersection both sides listed) and
//!   [`grant_allowed`] (the "never wider than either offer" check).
//! - [`handshake`]: the signed `propose` → `accept` → `key_exchange` → `activate` state machine
//!   with both-sides consent, pinned peer keys, re-checked consents, durable nonces, replay and
//!   substitution refusals.
//! - [`consent`]: one party administrator's consent, its step-up proof through the tenant's
//!   `spot-iam-access` policy and [`ConsentPolicy`], the maximum age a consent stays usable.
//! - [`peers`]: [`PeerKeyDirectory`], the keys this side already trusts, and [`Trust`], how a
//!   first-use key may be confirmed by an administrator.
//! - [`nonce`]: [`NonceLog`], the durable record of used handshake nonces; the handshake's own
//!   set is only a cache.
//! - [`wire`]: the five messages and the canonical transcript each signature covers.
//! - [`audit`]: every state change and every use writes a `spot-audit-chain` record locally; both
//!   sides reconstruct the milestones from their own trail.
//! - [`store`]: [`LinkStore`] and [`MemoryLinkStore`]; the PostgreSQL store is T2.
//! - [`service`]: revocation, expiry and the single use check ([`LinkService::authorize`]).
//! - [`delivery`]: what revocation does to messages already in flight.
//! - [`region`]: the [`RegionOracle`] port for a grant's peer-region requirement.
//! - [`error`]: the stable `tenant_link.*` codes.
//!
//! # Rules the crate enforces
//!
//! - **Two parties, pairwise.** A link joins exactly two parties; a three-party arrangement is
//!   a set of pairwise links (rule 10).
//! - **Both sides consent, with step-up.** Activation needs two [`Consent`] records from
//!   distinct parties and actors (rules 3 and the 17 Oct 2026 amendment). Every use of a consent
//!   re-checks its shape, its age against [`ConsentPolicy`] and its proof through the gate.
//! - **Keys are pinned, not taken from the message.** The first key a party sends must match the
//!   [`PeerKeyDirectory`] entry, or a [`Trust::FirstUseConfirmed`] fingerprint an administrator
//!   confirmed; later messages use the key the link recorded.
//! - **Nonces are durable.** Every send and receive records its nonce through [`NonceLog`]
//!   before acting, so a rebuilt handshake refuses a captured message.
//! - **Grants never exceed an offer.** The agreed grants are the intersection of both offers;
//!   a wider direction, schema range, field list or expiry is refused with the dimension named
//!   (rules 6 and 12).
//! - **Default deny for fields.** Only listed fields leave; personal and sensitive fields must be
//!   listed by name, not swept in by a namespace rule ([`FieldList::lint`]).
//! - **Revocation is immediate for new exchanges.** One check, [`Link::allows`], is what every
//!   sender and receiver calls; in-flight messages already acknowledged stay (rule 3).
//! - **Nothing is assumed about residency.** A peer-region requirement is answered by the
//!   [`RegionOracle`]; unknown is refused, and the refusal names the policy clause.
//! - **External peers carry messages only**, never record mirrors (rule 12).
//!
//! # Purity
//!
//! No clock (time is a `u64` of Unix milliseconds passed in), no randomness (nonces and link
//! ids come from the caller), no sockets, no database, no `unsafe`, no `unwrap`/`expect` outside
//! tests, no floats. Private key material never enters the crate: signing is a trait method.

#![forbid(unsafe_code)]

pub mod audit;
pub mod consent;
pub mod delivery;
pub mod error;
pub mod grant;
pub mod handshake;
pub mod ids;
pub mod keys;
pub mod model;
pub mod nonce;
pub mod peers;
pub mod region;
pub mod service;
pub mod store;
pub mod util;
pub mod wire;

pub use audit::{
    AuditSink, LinkEvent, LinkEventKind, MemoryAuditLog, reconstruct, record as record_event,
};
pub use consent::{
    CONSENT_OPERATION, Consent, ConsentPolicy, PolicyStepUp, StepUpGate, StepUpProof,
};
pub use delivery::{DeliveryState, RevocationOutcome, after_revocation};
pub use error::{Error, Result, codes};
pub use grant::{
    Direction, ExchangePayload, FieldCatalog, FieldList, FieldName, FieldValue, Filtered, Grant,
    Offer, SchemaRange, TopicKind, grant_allowed, grants_digest, grants_match,
};
pub use handshake::{Handshake, HandshakePolicy, Received, Resume};
pub use ids::{Digest, ExternalPeerId, LinkId, Nonce, Topic};
pub use keys::{IdentitySigner, PeerKey, SIGNATURE_BYTES, SeedSigner, SignatureBytes};
pub use model::{Carrier, Link, LinkDraft, LinkState, Party, Revocation, RevokeReason};
pub use nonce::{
    AsyncNonceClaimer, MemoryNonceLog, NonceClaim, NonceClaimer, NonceLog, claim_with,
    claim_with_async,
};
pub use peers::{MemoryPeerKeyDirectory, PeerKeyDirectory, Trust, check_peer_key};
pub use region::{MemoryRegionOracle, RegionOracle};
pub use service::{LinkService, SendRequest};
pub use store::{LinkStore, MemoryLinkStore};
pub use wire::{
    AcceptBody, ActivateBody, HandshakeBody, HandshakeMessage, KeyExchangeBody, ProposalBody,
    REVOKE_ACTIVE_REVISION, REVOKE_VERSION, RejectBody, RejectReason, RevokeMessage,
};

/// The name of this crate, for diagnostics.
pub const CRATE_NAME: &str = "spot-tenant-link";
