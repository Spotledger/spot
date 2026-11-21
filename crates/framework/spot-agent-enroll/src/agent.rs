//! The agent side of the enrolment: a state machine that never sends a proof before
//! the server is proven to be the pinned one.
//!
//! The states are explicit ([`AgentState`]); every transition happens in one method of
//! [`AgentEnroller`], and a message that does not fit the current state is
//! [`EnrolError::MessageOutOfOrder`]. The trusted path is:
//!
//! 1. [`AgentEnroller::new`] — the agent holds its [`KeyStore`] and the pinned
//!    [`ExpectedServer`] from the install command.
//! 2. [`AgentEnroller::request`] — builds the [`EnrolRequest`] with a fresh nonce; the
//!    token text comes from the install command and is never stored by the enroller.
//! 3. [`AgentEnroller::challenge`] — **pins first**: the challenge's key must match
//!    the expectation, then the signature must verify over the transcript. Only then
//!    does the state move to [`AgentState::Challenged`]. A wrong pin aborts before any
//!    proof exists.
//! 4. [`AgentEnroller::proof`] — signs the same transcript with the agent key and
//!    hands back the [`EnrolProof`].
//! 5. [`AgentEnroller::accepted`] — records the [`AgentIdentity`] values from the
//!    [`EnrolAccepted`], re-checking that the accepted server key is still the pinned
//!    one, and finishes in [`AgentState::Enrolled`].
//!
//! Nothing here opens a connection; the caller moves messages between this and the
//! server with whatever transport exists later. The enroller holds no secret material
//! of its own: signing goes through the [`KeyStore`] trait object, and the token text
//! is zeroized as soon as the challenge has been verified.

use std::sync::Arc;

use ed25519_dalek::Verifier;
use zeroize::Zeroize;

use crate::error::EnrolError;
use crate::identity::AgentIdentity;
use crate::keys::KeyStore;
use crate::message::{
    EnrolAccepted, EnrolChallenge, EnrolProof, EnrolRequest, ExpectedServer, PublicKeyB64,
    SignatureB64,
};
use crate::transcript;

/// The explicit states of the agent-side enrolment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    /// Fresh; no request sent yet.
    AwaitingRequest,
    /// The request was built and (by the caller) sent; the agent waits for the
    /// challenge.
    AwaitingChallenge,
    /// The challenge verified against the pinned server key; the proof may be signed.
    Challenged,
    /// The proof was signed and (by the caller) sent; the agent waits for the
    /// acceptance.
    AwaitingAccepted,
    /// The acceptance was recorded; the enrolment is complete.
    Enrolled,
}

/// The agent side of one enrolment exchange. One exchange per instance; a second
/// exchange needs a new enroller.
pub struct AgentEnroller {
    keystore: Arc<dyn KeyStore>,
    expected_server: ExpectedServer,
    hostname: String,
    platform: String,
    agent_version: String,
    state: AgentState,
    /// The agent nonce of the current exchange, as sent in the request.
    nonce_a: Option<String>,
    /// The server nonce of the current exchange, from the verified challenge.
    nonce_s: Option<String>,
    /// The server key of the current exchange, from the verified (and pinned) challenge.
    server_key: Option<PublicKeyB64>,
    /// The token text of the current exchange; zeroized as soon as the challenge has
    /// verified (the transcript's dependence on the token is carried forward as the
    /// digest below).
    token: Option<String>,
    /// The SHA-256 digest of the token text, kept for the proof's transcript after the
    /// token text itself is wiped.
    token_digest: Option<[u8; 32]>,
    /// The instant the persisted identity is stamped with, set with the acceptance.
    /// Kept for the caller that wants it without re-deriving; unused inside.
    #[allow(dead_code)]
    enrolled_at: i64,
    /// The agent public key of the current exchange, decided at construction.
    agent_public_key: PublicKeyB64,
}

impl AgentEnroller {
    /// A new enroller for one enrolment attempt: the identity key comes from
    /// `keystore`, the pin from the install command.
    #[must_use]
    pub fn new(
        keystore: Arc<dyn KeyStore>,
        expected_server: ExpectedServer,
        hostname: impl Into<String>,
        platform: impl Into<String>,
        agent_version: impl Into<String>,
    ) -> Self {
        let agent_public_key = keystore.public_key().to_b64();
        Self {
            keystore,
            expected_server,
            hostname: hostname.into(),
            platform: platform.into(),
            agent_version: agent_version.into(),
            state: AgentState::AwaitingRequest,
            nonce_a: None,
            nonce_s: None,
            server_key: None,
            token: None,
            token_digest: None,
            enrolled_at: 0,
            agent_public_key,
        }
    }

    /// The current explicit state.
    #[must_use]
    pub fn state(&self) -> AgentState {
        self.state
    }

    /// The transcript bytes of the current exchange, when the state has them; shared
    /// by [`AgentEnroller::challenge`] and [`AgentEnroller::proof`].
    fn transcript_bytes(&self) -> Result<Vec<u8>, EnrolError> {
        let nonce_a = self
            .nonce_a
            .as_deref()
            .ok_or(EnrolError::MessageOutOfOrder)?;
        let nonce_s = self
            .nonce_s
            .as_deref()
            .ok_or(EnrolError::MessageOutOfOrder)?;
        let server_key = self
            .server_key
            .as_ref()
            .ok_or(EnrolError::MessageOutOfOrder)?;
        let digest = self.token_digest.ok_or(EnrolError::MessageOutOfOrder)?;
        crate::transcript::build(
            nonce_a,
            nonce_s,
            self.agent_public_key.as_str(),
            server_key.as_str(),
            &digest,
        )
    }

    /// Builds message 1. The token text is taken here and kept only until the
    /// challenge verified.
    ///
    /// # Errors
    /// [`EnrolError::MessageOutOfOrder`] when a request was already built;
    /// [`EnrolError::FieldInvalid`] when the description fields are empty or over
    /// their bounds (the same checks the server runs on receipt).
    pub fn request(mut self, token: &str) -> Result<(EnrolRequest, Self), EnrolError> {
        if self.state != AgentState::AwaitingRequest {
            return Err(EnrolError::MessageOutOfOrder);
        }
        let request = EnrolRequest {
            token: token.to_owned(),
            agent_public_key: self.agent_public_key.clone(),
            hostname: self.hostname.clone(),
            platform: self.platform.clone(),
            agent_version: self.agent_version.clone(),
            nonce_a: transcript::fresh_nonce(),
        };
        request.shape_ok()?;
        self.nonce_a = Some(request.nonce_a.clone());
        let token = token.to_owned();
        self.token_digest = Some(crate::transcript::token_digest_of(&token));
        self.token = Some(token);
        self.state = AgentState::AwaitingChallenge;
        Ok((request, self))
    }

    /// Verifies message 2 against the pin, then the transcript signature.
    ///
    /// # Errors
    /// [`EnrolError::MessageOutOfOrder`] when no request was built or a challenge was
    /// already processed; [`EnrolError::FieldInvalid`] when the challenge shape is
    /// wrong; [`EnrolError::ServerKeyMismatch`] when the challenge's key is not the
    /// pinned one — the agent **aborts here**, before any proof exists;
    /// [`EnrolError::CryptoMalformed`] or [`EnrolError::SignatureInvalid`] when the
    /// signature does not verify over the transcript.
    pub fn challenge(mut self, challenge: &EnrolChallenge) -> Result<Self, EnrolError> {
        if self.state != AgentState::AwaitingChallenge {
            return Err(EnrolError::MessageOutOfOrder);
        }
        challenge.shape_ok()?;
        // Pin first: a different server never reaches signature verification.
        self.expected_server.matches(&challenge.server_public_key)?;
        self.nonce_s = Some(challenge.nonce_s.clone());
        self.server_key = Some(challenge.server_public_key.clone());
        let transcript_bytes = self.transcript_bytes()?;
        let signature = challenge.signature.signature()?;
        challenge
            .server_public_key
            .verifying_key()?
            .verify(&transcript_bytes, &signature)
            .map_err(|_| EnrolError::SignatureInvalid)?;
        // The transcript no longer needs the token text: wipe it now, keep the digest.
        if let Some(mut token) = self.token.take() {
            token.zeroize();
        }
        self.state = AgentState::Challenged;
        Ok(self)
    }

    /// Builds message 3: the agent's signature over the same transcript, signed
    /// through the [`KeyStore`].
    ///
    /// # Errors
    /// [`EnrolError::MessageOutOfOrder`] when the challenge was not verified first;
    /// [`EnrolError::CryptoMalformed`] when the transcript cannot be built (a bug,
    /// not bad input: the values were verified when stored).
    pub fn proof(mut self) -> Result<(EnrolProof, Self), EnrolError> {
        if self.state != AgentState::Challenged {
            return Err(EnrolError::MessageOutOfOrder);
        }
        // The token text was wiped when the challenge verified; the transcript of the
        // proof covers the same token hash prefix through the kept digest.
        let transcript_bytes = self.transcript_bytes()?;
        let signature = self.keystore.sign(&transcript_bytes);
        self.state = AgentState::AwaitingAccepted;
        Ok((
            EnrolProof {
                signature: SignatureB64::from_bytes_unchecked(&signature.to_bytes()),
            },
            self,
        ))
    }

    /// Records message 4. The accepted server key must still be the pinned one, and
    /// the agent id shape-checked, or the enrolment is not trusted.
    ///
    /// # Errors
    /// [`EnrolError::MessageOutOfOrder`] when no proof was signed;
    /// [`EnrolError::ServerKeyMismatch`] when the accepted key differs from the pin;
    /// [`EnrolError::FieldInvalid`] when the accepted agent id is empty or over 128
    /// bytes.
    pub fn accepted(
        mut self,
        accepted: &EnrolAccepted,
        now: i64,
    ) -> Result<(AgentIdentity, AgentState), EnrolError> {
        if self.state != AgentState::AwaitingAccepted {
            return Err(EnrolError::MessageOutOfOrder);
        }
        self.expected_server.matches(&accepted.server_public_key)?;
        if accepted.agent_id.is_empty() || accepted.agent_id.len() > 128 {
            return Err(EnrolError::FieldInvalid);
        }
        let identity = AgentIdentity {
            agent_id: accepted.agent_id.clone(),
            server_public_key: accepted.server_public_key.clone(),
            enrolled_at: now,
        };
        self.nonce_a.zeroize_option();
        self.nonce_s.zeroize_option();
        self.token_digest = None;
        self.state = AgentState::Enrolled;
        Ok((identity, self.state))
    }
}

impl std::fmt::Debug for AgentEnroller {
    /// Redacted: the enroller holds the token text while a challenge is pending; the
    /// `Debug` names the state and the key protection, nothing else.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentEnroller")
            .field("state", &self.state)
            .field("protection", &self.keystore.protection())
            .finish()
    }
}

/// Small helper so the nonce strings are wiped uniformly.
trait ZeroizeOption {
    fn zeroize_option(&mut self);
}

impl ZeroizeOption for Option<String> {
    fn zeroize_option(&mut self) {
        if let Some(value) = self.as_mut() {
            value.zeroize();
        }
        *self = None;
    }
}
