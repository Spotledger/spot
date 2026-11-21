//! The enrolment exchange messages: plain `serde` types, base64 text for bytes.
//!
//! A later transport task carries these over mutual TLS; this crate serialises them
//! only for its own exchange tests and property tests. Byte fields hold **base64
//! text**, not the decoded values, so:
//!
//! - a tampered message is caught at the point the value is used, with one typed error
//!   ([`EnrolError::CryptoMalformed`]) and no echo of the bad value;
//! - `Debug` and `serde` never print secret bytes — there are none to print.
//!
//! `EnrolRequest.token` is the printable one-time token from [`crate::token`]; it is a
//! secret and lives in the messages only for the one flight it is needed.

use serde::{Deserialize, Serialize};

use crate::error::EnrolError;

/// An internal helper type: the public key of a side, as the base64 text the wire
/// carries. Kept separate from `ed25519_dalek::VerifyingKey` so messages stay plain
/// data a transport can copy without this crate's crypto dependency graph.
///
/// Not secret material: a public key is public, and its `Debug` shows the base64 text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublicKeyB64(
    /// Standard base64 of the 32-byte compressed Ristretto point (`VerifyingKey`).
    pub String,
);

impl PublicKeyB64 {
    /// The base64 text as the wire carries it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Decodes to a `VerifyingKey`; a wrong length or alphabet is a malformed message.
    ///
    /// # Errors
    /// [`EnrolError::CryptoMalformed`] when the text is not exactly a public key.
    pub fn verifying_key(&self) -> Result<ed25519_dalek::VerifyingKey, EnrolError> {
        let bytes: [u8; 32] = crate::codec::dec64(&self.0)?;
        ed25519_dalek::VerifyingKey::from_bytes(&bytes).map_err(|_| EnrolError::CryptoMalformed)
    }

    /// The raw 32 bytes (decoded); a wrong shape is a malformed message.
    ///
    /// # Errors
    /// [`EnrolError::CryptoMalformed`] when the text is not exactly 32 base64 bytes.
    pub fn bytes(&self) -> Result<[u8; 32], EnrolError> {
        crate::codec::dec64(&self.0)
    }

    /// A key from raw 32-byte material (`VerifyingKey::to_bytes` form).
    #[must_use]
    pub fn from_bytes_unchecked(bytes: &[u8; 32]) -> Self {
        Self(crate::codec::enc64(bytes))
    }
}

impl std::fmt::Debug for PublicKeyB64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKeyB64({})", self.0)
    }
}

/// An internal helper type: an Ed25519 signature, as the base64 text the wire carries.
///
/// Not secret material; `Debug` shows the base64 text.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignatureB64(
    /// Standard base64 of the 64-byte compact `Signature`.
    pub String,
);

impl SignatureB64 {
    /// The base64 text as the wire carries it.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Decodes to a `Signature`; a wrong length or alphabet is a malformed message.
    ///
    /// # Errors
    /// [`EnrolError::CryptoMalformed`] when the text is not exactly a signature.
    pub fn signature(&self) -> Result<ed25519_dalek::Signature, EnrolError> {
        let bytes: [u8; 64] = crate::codec::dec64(&self.0)?;
        Ok(ed25519_dalek::Signature::from_bytes(&bytes))
    }

    /// A signature from raw 64-byte material (`Signature::to_bytes` form).
    #[must_use]
    pub fn from_bytes_unchecked(bytes: &[u8; 64]) -> Self {
        Self(crate::codec::enc64(bytes))
    }
}

impl std::fmt::Debug for SignatureB64 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SignatureB64({})", self.0)
    }
}

/// Message 1, agent → server: who wants to join, with the one-time token.
///
/// Serialised as JSON by the (later) transport; the server runs the token check and the
/// pinning, then answers with an [`EnrolChallenge`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrolRequest {
    /// The printable one-time token, exactly as the install command showed it.
    pub token: String,
    /// The agent's own public key: the identity it asks to be enrolled under.
    pub agent_public_key: PublicKeyB64,
    /// The hostname the agent runs on, shown in the console after enrolment. Free text.
    pub hostname: String,
    /// The platform the agent runs on (for example `linux-x86_64`, `windows-x86_64`).
    pub platform: String,
    /// The agent's own version string.
    pub agent_version: String,
    /// The agent side's fresh nonce for this exchange, base64 text.
    pub nonce_a: String,
}

impl EnrolRequest {
    /// The shape checks the server can make before anything secret is touched: the
    /// non-empty, bounded text fields. The token's own shape is checked in
    /// [`crate::token::printable_shape_ok`] so the malformed-token refusal and the
    /// refused-token refusal stay one and the same outcome.
    ///
    /// # Errors
    /// [`EnrolError::FieldInvalid`] when a field is empty or over its bound.
    pub fn shape_ok(&self) -> Result<(), EnrolError> {
        let ok = !self.hostname.is_empty()
            && self.hostname.len() <= 255
            && self.platform.len() <= 64
            && self.agent_version.len() <= 64
            && self.nonce_a.len() == 44;
        if ok {
            Ok(())
        } else {
            Err(EnrolError::FieldInvalid)
        }
    }
}

/// Message 2, server → agent: the server's identity and its proof of the exchange.
///
/// The agent checks `server_public_key` against the **pinned** expectation first
/// ([`crate::keys::ExpectedServer`]) and only then verifies `signature` over the
/// [`crate::transcript`].
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrolChallenge {
    /// The server's public key, for the agent to verify the signature and to pin.
    pub server_public_key: PublicKeyB64,
    /// The server side's fresh nonce for this exchange, base64 text.
    pub nonce_s: String,
    /// The server's Ed25519 signature over the transcript built from the request's and
    /// this message's values.
    pub signature: SignatureB64,
}

impl std::fmt::Debug for EnrolChallenge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrolChallenge")
            .field("server_public_key", &self.server_public_key)
            .field("nonce_s", &self.nonce_s)
            .field("signature", &self.signature)
            .finish()
    }
}

impl EnrolChallenge {
    /// The shape checks the agent can make before any decoding: a 44-character nonce.
    ///
    /// # Errors
    /// [`EnrolError::FieldInvalid`] when the nonce has an unexpected length.
    pub fn shape_ok(&self) -> Result<(), EnrolError> {
        if self.nonce_s.len() == 44 {
            Ok(())
        } else {
            Err(EnrolError::FieldInvalid)
        }
    }
}

/// Message 3, agent → server: the agent's proof over the same transcript, signed with
/// the enrolled agent key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrolProof {
    /// The agent's Ed25519 signature over the transcript.
    pub signature: SignatureB64,
}

impl std::fmt::Debug for EnrolProof {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrolProof")
            .field("signature", &self.signature)
            .finish()
    }
}

/// The server's final answer: the agent is enrolled.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnrolAccepted {
    /// The durable agent identity the server assigned.
    pub agent_id: String,
    /// The server public key again, so the agent records exactly what it proved
    /// against — and can re-check its pin after the exchange.
    pub server_public_key: PublicKeyB64,
}

/// The pinned server identity the install command hands to the agent: the fingerprint
/// over the public key it must see in the challenge, or the exchange aborts.
///
/// The fingerprint is the SHA-256 over the raw public key bytes, hex-encoded, and is
/// what an administrator compares out of band when installing the agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectedServer {
    /// The hex SHA-256 fingerprint of the expected server public key (64 hex chars).
    pub public_key_fingerprint: String,
}

impl ExpectedServer {
    /// Wraps the expectation from a fingerprint the install command carried.
    ///
    /// # Errors
    /// [`EnrolError::FieldInvalid`] when the text is not 64 lowercase hex characters.
    pub fn parse(fingerprint: &str) -> Result<Self, EnrolError> {
        let shaped = fingerprint.len() == 64
            && fingerprint
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if shaped {
            Ok(Self {
                public_key_fingerprint: fingerprint.to_owned(),
            })
        } else {
            Err(EnrolError::FieldInvalid)
        }
    }

    /// The fingerprint of a server public key: hex SHA-256 over the 32 raw bytes.
    ///
    /// # Errors
    /// [`EnrolError::CryptoMalformed`] when the key text does not decode.
    pub fn of_key(server_public_key: &PublicKeyB64) -> Result<Self, EnrolError> {
        let bytes = server_public_key.bytes()?;
        Self::parse(&crate::codec::hex_lower(&crate::util::sha256(&bytes)))
    }

    /// The agent's pin check: the challenge's key matches the pinned fingerprint.
    ///
    /// # Errors
    /// [`EnrolError::ServerKeyMismatch`] on any difference, including a key whose text
    /// does not decode (a tampered challenge is not the expected server either).
    pub fn matches(&self, server_public_key: &PublicKeyB64) -> Result<(), EnrolError> {
        let actual = Self::of_key(server_public_key)?;
        if actual.public_key_fingerprint == self.public_key_fingerprint {
            Ok(())
        } else {
            Err(EnrolError::ServerKeyMismatch)
        }
    }
}

/// Every message of one exchange in one place, for tests.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub(crate) struct Exchange {
    pub(crate) request: EnrolRequest,
    pub(crate) challenge: EnrolChallenge,
    pub(crate) proof: EnrolProof,
    pub(crate) accepted: EnrolAccepted,
}

impl Exchange {
    /// Builds the transcript over an exchange from its message values; the common
    /// code of the two state machines.
    #[allow(dead_code)]
    pub(crate) fn transcript(&self) -> Result<Vec<u8>, EnrolError> {
        let agent_key = &self.request.agent_public_key;
        let server_key = &self.challenge.server_public_key;
        let digest = crate::transcript::token_digest_of(&self.request.token);
        crate::transcript::build(
            &self.request.nonce_a,
            &self.challenge.nonce_s,
            agent_key.as_str(),
            server_key.as_str(),
            &digest,
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{
        EnrolAccepted, EnrolChallenge, EnrolProof, EnrolRequest, ExpectedServer, PublicKeyB64,
        SignatureB64,
    };
    use crate::error::EnrolError;

    fn sample_request(token: &str) -> EnrolRequest {
        EnrolRequest {
            token: token.to_owned(),
            agent_public_key: PublicKeyB64::from_bytes_unchecked(&[1u8; 32]),
            hostname: "node-1".to_owned(),
            platform: "linux-x86_64".to_owned(),
            agent_version: "0.1.0".to_owned(),
            nonce_a: crate::transcript::nonce_b64(&[5u8; 32]),
        }
    }

    #[test]
    fn messages_round_trip_through_json() {
        let request = sample_request("token");
        let text = serde_json::to_string(&request).unwrap();
        let back: EnrolRequest = serde_json::from_str(&text).unwrap();
        assert_eq!(back, request);
        let accepted = EnrolAccepted {
            agent_id: "agent-1".to_owned(),
            server_public_key: PublicKeyB64::from_bytes_unchecked(&[2u8; 32]),
        };
        let text = serde_json::to_string(&accepted).unwrap();
        assert_eq!(
            serde_json::from_str::<EnrolAccepted>(&text).unwrap(),
            accepted
        );
        let _ = (
            EnrolProof {
                signature: SignatureB64::from_bytes_unchecked(&[0u8; 64]),
            },
            EnrolChallenge {
                server_public_key: PublicKeyB64::from_bytes_unchecked(&[2u8; 32]),
                nonce_s: crate::transcript::nonce_b64(&[6u8; 32]),
                signature: SignatureB64::from_bytes_unchecked(&[0u8; 64]),
            },
        );
    }

    #[test]
    fn fingerprints_match_keys_and_refuse_other_shapes() {
        let key = PublicKeyB64::from_bytes_unchecked(&[2u8; 32]);
        let expected = ExpectedServer::of_key(&key).unwrap();
        assert!(expected.matches(&key).is_ok());
        let other = PublicKeyB64::from_bytes_unchecked(&[3u8; 32]);
        assert_eq!(expected.matches(&other), Err(EnrolError::ServerKeyMismatch));
        assert!(matches!(
            ExpectedServer::parse(&"0".repeat(63)),
            Err(EnrolError::FieldInvalid)
        ));
        // Upper-case hex is refused so fingerprints compare by exact text.
        assert!(matches!(
            ExpectedServer::parse(&"A".repeat(64)),
            Err(EnrolError::FieldInvalid)
        ));
    }

    #[test]
    fn shape_checks_refuse_empty_and_wrong_nonce_length() {
        let mut request = sample_request("token");
        assert!(request.shape_ok().is_ok());
        request.hostname = String::new();
        assert_eq!(request.shape_ok(), Err(EnrolError::FieldInvalid));
        request.hostname = "node-1".to_owned();
        request.nonce_a = "short".to_owned();
        assert_eq!(request.shape_ok(), Err(EnrolError::FieldInvalid));
        let challenge = EnrolChallenge {
            server_public_key: PublicKeyB64::from_bytes_unchecked(&[2u8; 32]),
            nonce_s: "short".to_owned(),
            signature: SignatureB64::from_bytes_unchecked(&[0u8; 64]),
        };
        assert_eq!(challenge.shape_ok(), Err(EnrolError::FieldInvalid));
    }
}
