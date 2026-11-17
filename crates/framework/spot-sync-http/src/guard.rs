//! The per-heartbeat re-check of a waiting pull (WP-RTB-02.T13).
//!
//! A pull is a long poll: the hub holds the request open, writing a heartbeat line every few
//! seconds, until records arrive or the long-poll cap passes. The directory resolves the client
//! certificate **once**, when the request starts. A node revoked or suspended while its pull is
//! open would therefore keep receiving heartbeats (and records) until the cap. The heartbeat is the
//! natural place to ask again: [`HeartbeatCheck`] resolves the same certificate against the live
//! directory and the pull ends, closing the stream, the first time it is no longer admitted. A
//! revoked node's open pull is thus over within one heartbeat (default 5 s), not within the
//! long-poll cap.
//!
//! The check is a cheap in-memory lookup (the durable directory answers from its snapshot), so it
//! costs nothing next to the heartbeat's own write. It fails closed: any resolution error ends the
//! pull.
//!
//! # Wiring
//!
//! [`SyncServer::with_pull_gate`](crate::server::SyncServer) (a one-line setter) stores a
//! [`PullGate`]; `pump_pull` builds one [`HeartbeatCheck`] from the certificate its handler
//! resolved and calls [`HeartbeatCheck::still_admitted`] in its heartbeat branch, returning when it
//! says `false`. `DirectoryGate` over the same directory the server resolves with is the gate the
//! hub host installs.

use std::sync::Arc;

use crate::identity::{ClientCertificate, NodeDirectory};

/// Decides, at each heartbeat of an open pull, whether the connection's certificate is still
/// admitted.
pub trait PullGate: Send + Sync {
    /// Whether `certificate` is still admitted now.
    fn admitted(&self, certificate: &ClientCertificate) -> bool;
}

/// A gate that always admits: the behaviour before this check existed (the pull ends at the cap).
#[derive(Clone, Copy, Debug, Default)]
pub struct OpenGate;

impl PullGate for OpenGate {
    fn admitted(&self, _certificate: &ClientCertificate) -> bool {
        true
    }
}

/// The gate over the live node directory: admitted while the directory still resolves the
/// certificate (enrolled, not revoked, not suspended, directory not fail-closed).
#[derive(Clone)]
pub struct DirectoryGate {
    directory: Arc<dyn NodeDirectory>,
}

impl DirectoryGate {
    /// A gate over `directory`.
    #[must_use]
    pub fn new(directory: Arc<dyn NodeDirectory>) -> Self {
        Self { directory }
    }
}

impl std::fmt::Debug for DirectoryGate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectoryGate").finish_non_exhaustive()
    }
}

impl PullGate for DirectoryGate {
    fn admitted(&self, certificate: &ClientCertificate) -> bool {
        self.directory.resolve(certificate).is_ok()
    }
}

/// One open pull's re-check: the gate and the certificate the pull started with.
#[derive(Clone)]
pub struct HeartbeatCheck {
    gate: Arc<dyn PullGate>,
    certificate: ClientCertificate,
}

impl HeartbeatCheck {
    /// A check for the pull of `certificate`.
    #[must_use]
    pub fn new(gate: Arc<dyn PullGate>, certificate: ClientCertificate) -> Self {
        Self { gate, certificate }
    }

    /// Whether the pull may go on. Called once per heartbeat.
    #[must_use]
    pub fn still_admitted(&self) -> bool {
        self.gate.admitted(&self.certificate)
    }
}

impl std::fmt::Debug for HeartbeatCheck {
    /// Shows no certificate.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeartbeatCheck").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// The TLS side: ask for a client certificate, do not chain it.
// ---------------------------------------------------------------------------------------------

/// A client-certificate verifier that **requests** a certificate and accepts any that proves it
/// holds its key, without chaining it to a CA.
///
/// An enrolled node's certificate is self-signed (it generates its own key pair, ADR-062 section
/// 3) and is trusted by its **pinned fingerprint** in the live node directory, checked by the
/// guard on every request. A verifier that chained client certificates to a CA would refuse every
/// self-signed node at the handshake, including the first enrolment request. This one keeps the
/// handshake's proof of possession (the signature over the transcript is verified with the
/// certificate's key) and leaves authorisation to the directory:
///
/// * a connection with no certificate is allowed at the handshake (the enrolment endpoint is open),
///   and every other path answers `sync_http.identity_required`;
/// * a certificate the directory does not hold is `sync_http.identity_unknown`, a revoked one
///   `sync_http.identity_revoked`.
#[derive(Debug)]
pub struct PinnedClientAuth {
    algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
}

impl PinnedClientAuth {
    /// A verifier over the `ring` provider's signature algorithms.
    #[must_use]
    pub fn new() -> Self {
        Self {
            algorithms: rustls::crypto::ring::default_provider().signature_verification_algorithms,
        }
    }
}

impl Default for PinnedClientAuth {
    fn default() -> Self {
        Self::new()
    }
}

impl rustls::server::danger::ClientCertVerifier for PinnedClientAuth {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        // Authorisation is the directory's pin, per request; nothing chains here.
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// The hub's TLS server configuration for self-signed, pinned nodes: the hub's own chain and key,
/// and [`PinnedClientAuth`] for client certificates.
///
/// # Errors
///
/// `sync_http.unavailable` when the chain and key do not form a certificate.
pub fn pinning_server_config(
    chain: Vec<rustls::pki_types::CertificateDer<'static>>,
    key: rustls::pki_types::PrivateKeyDer<'static>,
) -> crate::error::Result<rustls::ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|error| crate::error::Error::unavailable(format!("TLS versions: {error}")))?
        .with_client_cert_verifier(Arc::new(PinnedClientAuth::new()))
        .with_single_cert(chain, key)
        .map_err(|error| {
            crate::error::Error::unavailable(format!(
                "the server certificate did not load: {error}"
            ))
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use super::*;
    use crate::error::{Error, Result};
    use crate::identity::AuthenticatedNode;
    use spot_sync_protocol::NodeId;

    /// A directory whose answer a test flips: admitted, then revoked.
    struct Flip {
        revoked: Mutex<bool>,
    }

    impl NodeDirectory for Flip {
        fn resolve(&self, _certificate: &ClientCertificate) -> Result<AuthenticatedNode> {
            if *self.revoked.lock().unwrap() {
                return Err(Error::identity_revoked("revoked"));
            }
            Ok(AuthenticatedNode::new(
                NodeId::new("edge-khi#branch_01").unwrap(),
                ["branch_01".to_owned()],
            ))
        }
    }

    fn certificate() -> ClientCertificate {
        ClientCertificate::from_der(vec![1, 2, 3, 4])
    }

    #[test]
    fn a_revoked_certificate_is_not_admitted_at_the_next_heartbeat() {
        let directory = Arc::new(Flip {
            revoked: Mutex::new(false),
        });
        let check = HeartbeatCheck::new(
            Arc::new(DirectoryGate::new(directory.clone())),
            certificate(),
        );
        assert!(check.still_admitted());
        assert!(check.still_admitted());
        *directory.revoked.lock().unwrap() = true;
        assert!(!check.still_admitted());
    }

    #[test]
    fn the_open_gate_admits_everything() {
        let check = HeartbeatCheck::new(Arc::new(OpenGate), certificate());
        assert!(check.still_admitted());
    }
}
