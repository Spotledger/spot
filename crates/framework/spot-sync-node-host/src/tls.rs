//! The edge's TLS client configuration: the hub is authenticated by **pinned fingerprint**, with
//! no system trust store (ADR-062 section 3).
//!
//! The hub's certificate is whatever was enrolled: the fingerprint comes from the enrolment file.
//! A different certificate is refused (`sync_node_host.hub_untrusted`), **even a valid
//! CA-signed one**: the verifier compares the SHA-256 of the end-entity certificate with the pin
//! and nothing else decides trust. The handshake signatures are still verified with the crypto
//! provider's algorithms, so a peer that presents the pinned certificate must hold its key.
//!
//! The node presents its own certificate (self-signed, generated on first start by
//! `spot_sync_enrol::NodeIdentity`) as the client certificate.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use spot_sync_enrol::{NodeIdentity, fingerprint_of};

use crate::error::{Error, Result};

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts exactly one end-entity certificate: the one whose SHA-256 fingerprint is the pin.
#[derive(Debug)]
pub struct PinnedHubVerifier {
    pin: String,
    algorithms: WebPkiSupportedAlgorithms,
}

impl PinnedHubVerifier {
    /// A verifier for the hub whose certificate fingerprint is `pin` (lower-case hex SHA-256,
    /// the form `spot_sync_enrol::fingerprint_of` writes; case and `:` separators are tolerated).
    pub fn new(pin: &str) -> Self {
        Self {
            pin: normalise(pin),
            algorithms: provider().signature_verification_algorithms,
        }
    }
}

/// Lower-case hex without separators, so `AB:CD` and `abcd` are the same pin.
fn normalise(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Constant-time-ish equality of two hex strings (lengths are public).
fn same(left: &str, right: &str) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.bytes()
        .zip(right.bytes())
        .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

impl ServerCertVerifier for PinnedHubVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, TlsError> {
        if same(&fingerprint_of(end_entity.as_ref()), &self.pin) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General(
                "the hub certificate is not the pinned one".to_owned(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// The client configuration for dialling the hub: the pinned hub certificate, and `identity` as
/// the client certificate.
///
/// # Errors
///
/// `sync_node_host.not_enrolled` when `hub_fingerprint` is empty or the identity does not load
/// into TLS.
pub fn pinned_client_config(
    hub_fingerprint: &str,
    identity: &NodeIdentity,
) -> Result<rustls::ClientConfig> {
    if normalise(hub_fingerprint).is_empty() {
        return Err(Error::not_enrolled("no hub certificate is pinned"));
    }
    let chain = vec![CertificateDer::from(identity.certificate_der().to_vec())];
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        identity.private_key_pkcs8().to_vec(),
    ));
    rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(|_| Error::not_enrolled("the TLS protocol versions did not load"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedHubVerifier::new(hub_fingerprint)))
        .with_client_auth_cert(chain, key)
        .map_err(|_| Error::not_enrolled("the node certificate did not load into TLS"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn cert() -> CertificateDer<'static> {
        let key = rcgen::KeyPair::generate().unwrap();
        let params = rcgen::CertificateParams::new(vec!["hub".to_owned()]).unwrap();
        CertificateDer::from(params.self_signed(&key).unwrap().der().to_vec())
    }

    fn name() -> ServerName<'static> {
        ServerName::try_from("hub").unwrap()
    }

    #[test]
    fn only_the_pinned_certificate_is_accepted() {
        let pinned = cert();
        let other = cert();
        let verifier = PinnedHubVerifier::new(&fingerprint_of(pinned.as_ref()));
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000));
        verifier
            .verify_server_cert(&pinned, &[], &name(), &[], now)
            .unwrap();
        assert!(
            verifier
                .verify_server_cert(&other, &[], &name(), &[], now)
                .is_err()
        );
    }

    #[test]
    fn a_pin_in_upper_case_with_colons_is_the_same_pin() {
        let pinned = cert();
        let fingerprint = fingerprint_of(pinned.as_ref());
        let spaced: String = fingerprint
            .as_bytes()
            .chunks(2)
            .map(|pair| String::from_utf8_lossy(pair).to_uppercase())
            .collect::<Vec<_>>()
            .join(":");
        let verifier = PinnedHubVerifier::new(&spaced);
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000));
        verifier
            .verify_server_cert(&pinned, &[], &name(), &[], now)
            .unwrap();
    }

    #[test]
    fn no_pin_is_not_enrolled() {
        let dir = std::env::temp_dir().join(format!("spot-node-host-tls-{}", std::process::id()));
        let node = spot_sync_protocol::NodeId::new("edge-khi#branch_01").unwrap();
        let identity = NodeIdentity::load_or_create(&dir, &node, 1_800_000_000).unwrap();
        let error = pinned_client_config("  ", &identity).unwrap_err();
        assert_eq!(error.code, crate::error::NOT_ENROLLED);
        assert!(pinned_client_config(&fingerprint_of(cert().as_ref()), &identity).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
