//! The external carrier of the tenant link (WP-CTL-11.T1; ADR-058 rule 12; ADR-080).
//!
//! A peer that is not a platform tenant exchanges **message topics** with a tenant under a
//! grant, over the connector framework and signed webhooks that already exist:
//!
//! * [`ExternalPeer`] says who the peer is, which credential it uses (by reference), how
//!   messages reach it ([`Outbound`]), where and how it signs what it sends
//!   ([`WebhookSpec`]) and what its messages look like ([`PeerFormat`]);
//! * [`ExternalSender`] hands what the link's outbox releases to a [`PeerChannel`]
//!   ([`ConnectorChannel`] or [`WebhookChannel`], both through the egress policy) and maps the
//!   answer onto the delivery states of ADR-058 rule 13;
//! * [`ExternalReceiver`] is the handler behind the peer's webhook: it refuses a body past the
//!   peer's cap before hashing anything, verifies the signature, then feeds
//!   [`spot_link_exchange::Inbox::receive`], so grant limits, the field list, schema range,
//!   deduplication, ordering, audit and conditions are the tenant link's own. Refusals are
//!   counted per peer per window ([`RefusalCounter`]), never one audit row per request.
//!
//! The exchange (outbox, inbox, notices, retention, [`spot_link_exchange::stuck_messages`]) is
//! reused unchanged. A record topic never crosses an external link: [`ExternalPeer::validate_for`]
//! refuses a link that grants one.
//!
//! Nothing here reads a clock (every instant is an argument), draws randomness, moves a float,
//! or holds a secret: a credential is a [`spot_connectors::CredentialRef`], resolved from the
//! vault for one call and dropped, and no error, audit record, receipt, `Debug` output or
//! response body carries it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod channel;
pub mod error;
pub mod peer;
pub mod receiver;
pub mod refusals;
pub mod sender;
pub mod signature;
pub mod wire;

pub use channel::{
    ChannelFailure, ConnectorChannel, Delivery, PeerAnswer, PeerChannel, WebhookChannel,
};
pub use error::{Error, Result, codes};
pub use peer::{ExternalPeer, Outbound, PeerFormat, SignatureScheme, WebhookSpec};
pub use receiver::{ExternalReceiver, InboundRequest, InboundResponse, ReceiverPorts};
pub use refusals::{Recorded, RefusalCounter};
pub use sender::{ExternalSender, PumpReport, Verdict, apply_ack, classify, reject_code_for};
pub use signature::verify_inbound;
pub use wire::{
    Inbound, InboundAck, InboundMessage, decode_inbound, encode_outbound, external_message_id,
    outbound_envelope,
};

/// This crate's name, for diagnostics.
pub const CRATE_NAME: &str = "spot-link-external";
