//! Who is on the other end of a connection.
//!
//! The link's peer is identified by the transport, never by anything inside a message:
//!
//! * **Server side:** the client certificate of the mutual-TLS session (the TLS layer puts it in
//!   the request, exactly as `spot-sync-http` does) is mapped to a [`Party`] by a
//!   [`PeerCertificates`] directory that pins certificates by SHA-256 fingerprint. A certificate
//!   that is not pinned, or was revoked, maps to nothing.
//! * **Client side:** the connection is dialled to an address whose server certificate chains to
//!   a CA the host pinned for that peer, so the [`PeerTransport`] *is* the peer. The host builds
//!   it with the party it pinned; a response never names who is answering.
//!
//! The party then goes into `Inbox::receive` and `Outbox::on_receipt` as `transport_peer`.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use spot_sync_http::{ClientCertificate, Transport};
use spot_tenant_link::Party;

/// Maps a presented client certificate to the party it is pinned to.
pub trait PeerCertificates: Send + Sync + 'static {
    /// The party `certificate` is pinned to, or `None` when it is not pinned or was revoked.
    /// There is no third answer: nothing here falls back to an anonymous identity.
    fn party(&self, certificate: &ClientCertificate) -> Option<Party>;
}

struct Pin {
    party: Party,
    revoked: bool,
}

/// An in-process [`PeerCertificates`] keyed by certificate fingerprint. The control plane's
/// directory replaces it; it is the reference for tests and single-node hosts.
#[derive(Default)]
pub struct PinnedCertificates {
    pins: RwLock<BTreeMap<String, Pin>>,
}

impl PinnedCertificates {
    /// An empty directory: every certificate is unknown.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Pins a certificate fingerprint (lower-case hex SHA-256) to `party`.
    pub fn pin(&self, fingerprint: impl Into<String>, party: Party) {
        let mut pins = self
            .pins
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pins.insert(
            fingerprint.into(),
            Pin {
                party,
                revoked: false,
            },
        );
    }

    /// Revokes a pinned fingerprint; later requests with it are refused.
    pub fn revoke(&self, fingerprint: &str) {
        let mut pins = self
            .pins
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(pin) = pins.get_mut(fingerprint) {
            pin.revoked = true;
        }
    }
}

impl PeerCertificates for PinnedCertificates {
    fn party(&self, certificate: &ClientCertificate) -> Option<Party> {
        let pins = self
            .pins
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        pins.get(certificate.fingerprint())
            .filter(|pin| !pin.revoked)
            .map(|pin| pin.party.clone())
    }
}

/// A transport to one peer together with the party that peer is, as the host pinned it.
#[derive(Clone)]
pub struct PeerTransport {
    transport: Arc<dyn Transport>,
    peer: Party,
}

impl PeerTransport {
    /// Wraps `transport`, which must reach only `peer`: its TLS configuration pins the CA (or the
    /// certificate) of that peer and its address is that peer's endpoint. `peer` comes from the
    /// host's own configuration of the link, never from a response.
    #[must_use]
    pub fn new(transport: Arc<dyn Transport>, peer: Party) -> Self {
        Self { transport, peer }
    }

    /// The pinned peer.
    #[must_use]
    pub fn peer(&self) -> &Party {
        &self.peer
    }

    pub(crate) fn transport(&self) -> &Arc<dyn Transport> {
        &self.transport
    }
}

impl std::fmt::Debug for PeerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerTransport")
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}
