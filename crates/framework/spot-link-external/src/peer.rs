//! The external peer: who it is, how it is reached, how it proves who it is and what its
//! messages look like on the wire (ADR-058 rule 12).

use spot_connectors::{ConnectorId, CredentialRef, MappingSpec};
use spot_tenant_link::{Carrier, ExternalPeerId, Link, Party, TopicKind};

use crate::error::{Error, Result};

/// How an outbound message reaches the peer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outbound {
    /// One operation of an installed connector (WP-INT-05). The connector holds the peer's
    /// protocol; the message body is the operation's input.
    ConnectorOp {
        /// The installed connector.
        connector: ConnectorId,
        /// The operation to invoke.
        operation: String,
    },
    /// A signed `POST` to this URL through the egress policy (the `spot-webhooks` signing).
    WebhookUrl(String),
}

/// How the peer signs what it sends us.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SignatureScheme {
    /// The platform's own scheme: `X-Spot-Signature: t=<unix>,v1=<hex hmac-sha256(secret,
    /// t + "." + body)>`.
    SpotHmacSha256,
    /// The same HMAC-SHA256 over `<unix seconds> + "." + body`, but the signature and the
    /// timestamp travel in headers the peer chooses (hex signature, decimal seconds).
    HeaderHmacSha256 {
        /// The header holding the hex signature.
        signature_header: String,
        /// The header holding the unix timestamp in seconds.
        timestamp_header: String,
    },
}

/// The peer's inbound webhook: where it posts and how its signature is checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookSpec {
    /// The path the peer posts to (starts with `/`).
    pub path: String,
    /// How the peer signs.
    pub signature_scheme: SignatureScheme,
    /// How far the signed timestamp may be from our clock, in seconds.
    pub tolerance_secs: i64,
}

/// The wire shape of a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeerFormat {
    /// The platform's JSON envelope as it is.
    JsonEnvelope,
    /// The peer's own shape: rules map the envelope to the peer's body going out, and the
    /// peer's body to the envelope coming in.
    Mapped(MappingSpec),
}

/// An external peer of one link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalPeer {
    /// The peer's id (the link party `external:<id>`).
    pub id: ExternalPeerId,
    /// A name for people.
    pub name: String,
    /// The credential, by reference only: the shared secret of the HMAC signature in both
    /// directions. The value is resolved at call time and never stored here.
    pub auth: CredentialRef,
    /// How outbound messages reach the peer.
    pub outbound: Outbound,
    /// The peer's inbound webhook.
    pub inbound: WebhookSpec,
    /// The wire shape.
    pub format: PeerFormat,
    /// The largest inbound body the receiver accepts, in bytes, checked before the signature
    /// is computed. Required and greater than zero; [`ExternalPeer::new`] sets
    /// [`ExternalPeer::DEFAULT_MAX_BODY_BYTES`].
    pub max_body_bytes: u64,
    /// How long one counting window of inbound refusals lasts, in seconds. Required and
    /// greater than zero; [`ExternalPeer::new`] sets
    /// [`ExternalPeer::DEFAULT_REFUSAL_WINDOW_SECS`].
    pub refusal_window_secs: u64,
}

impl ExternalPeer {
    /// The documented body cap a peer gets from [`ExternalPeer::new`]: 1 MiB. A body larger
    /// than this is refused before its signature is looked at, so an unauthenticated sender
    /// cannot buy an HMAC computation with bytes.
    pub const DEFAULT_MAX_BODY_BYTES: u64 = 1024 * 1024;

    /// The documented refusal window a peer gets from [`ExternalPeer::new`]: 60 seconds.
    /// Within one window the first refusal is audited with its code; the rest are counted and
    /// written once, by the first request of the next window.
    pub const DEFAULT_REFUSAL_WINDOW_SECS: u64 = 60;

    /// The widest accepted inbound timestamp tolerance, in seconds. A wider window is a
    /// validation error: it would let a captured request be replayed long after it was sent.
    pub const MAX_TOLERANCE_SECS: i64 = 900;

    /// A peer with the documented limits ([`ExternalPeer::DEFAULT_MAX_BODY_BYTES`] and
    /// [`ExternalPeer::DEFAULT_REFUSAL_WINDOW_SECS`]). Change the fields to use other limits.
    #[must_use]
    pub fn new(
        id: ExternalPeerId,
        name: String,
        auth: CredentialRef,
        outbound: Outbound,
        inbound: WebhookSpec,
        format: PeerFormat,
    ) -> Self {
        Self {
            id,
            name,
            auth,
            outbound,
            inbound,
            format,
            max_body_bytes: Self::DEFAULT_MAX_BODY_BYTES,
            refusal_window_secs: Self::DEFAULT_REFUSAL_WINDOW_SECS,
        }
    }

    /// The link party this peer is.
    #[must_use]
    pub fn party(&self) -> Party {
        Party::External(self.id.clone())
    }

    /// Checks the definition on its own.
    ///
    /// # Errors
    /// [`Error::InvalidPeer`] for an empty name, a path that does not start with `/`, a
    /// webhook URL that is not `http` or `https`, an empty operation, a non-positive tolerance,
    /// an empty custom header name, a zero body cap or a zero refusal window, and
    /// [`Error::ToleranceTooWide`] for a tolerance above
    /// [`ExternalPeer::MAX_TOLERANCE_SECS`].
    pub fn validate(&self) -> Result<()> {
        let bad = |reason| Err(Error::InvalidPeer { reason });
        if self.name.trim().is_empty() {
            return bad("the peer has no name");
        }
        if !self.inbound.path.starts_with('/') || self.inbound.path.contains(char::is_whitespace) {
            return bad("the inbound path starts with / and has no spaces");
        }
        if self.inbound.tolerance_secs <= 0 {
            return bad("the timestamp tolerance is positive");
        }
        if self.inbound.tolerance_secs > Self::MAX_TOLERANCE_SECS {
            return Err(Error::ToleranceTooWide);
        }
        if self.max_body_bytes == 0 {
            return bad("the inbound body cap is at least one byte");
        }
        if self.refusal_window_secs == 0 {
            return bad("the refusal window is at least one second");
        }
        if let SignatureScheme::HeaderHmacSha256 {
            signature_header,
            timestamp_header,
        } = &self.inbound.signature_scheme
            && (signature_header.trim().is_empty() || timestamp_header.trim().is_empty())
        {
            return bad("a header scheme names both headers");
        }
        match &self.outbound {
            Outbound::ConnectorOp { operation, .. } if operation.trim().is_empty() => {
                bad("the connector operation is empty")
            }
            Outbound::WebhookUrl(url) => match url::Url::parse(url) {
                Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => Ok(()),
                _ => bad("the webhook url is an http or https url"),
            },
            Outbound::ConnectorOp { .. } => Ok(()),
        }
    }

    /// Checks the definition against `link`: an external-carrier link whose other party is this
    /// peer, and whose grants are all message topics.
    ///
    /// # Errors
    /// [`Error::WrongCarrier`], [`Error::PeerMismatch`], [`Error::RecordTopic`] for the first
    /// record topic granted, or any error of [`ExternalPeer::validate`].
    pub fn validate_for(&self, link: &Link) -> Result<()> {
        self.validate()?;
        if link.carrier != Carrier::External {
            return Err(Error::WrongCarrier);
        }
        if link.peer() != Some(&self.party()) {
            return Err(Error::PeerMismatch);
        }
        if let Some(grant) = link.grants.iter().find(|g| g.kind == TopicKind::Record) {
            return Err(Error::RecordTopic {
                topic: grant.topic.as_str().to_owned(),
            });
        }
        Ok(())
    }
}
