//! The server side of the enrolment: token consumption, transcript signing and the
//! acceptance — one state machine per in-flight exchange.
//!
//! The rules this side enforces (brief §3):
//!
//! - The token is checked and **consumed when the request arrives**, before any
//!   signing: a repeated request with a consumed token is refused, and a proof that
//!   later fails to verify leaves the token consumed (an attacker cannot retry a
//!   guess over a live token) and records the attempt.
//! - The challenge signature covers the domain-separated transcript
//!   ([`crate::transcript`]) with the **server's** key, so the agent can pin it.
//! - The proof must verify over the same transcript with the agent key the request
//!   carried.
//! - Every refusal to the agent is the same [`EnrolError::TokenRefused`]-shaped short
//!   answer; the precise [`RejectReason`] goes to the audit log the caller owns.
//!
//! No sockets, no inbound ports: the caller carries the messages.

use std::sync::Arc;

use ed25519_dalek::Verifier;
use zeroize::Zeroize;

use crate::error::{EnrolError, RejectReason};
use crate::keys::KeyStore;
use crate::message::{EnrolAccepted, EnrolChallenge, EnrolProof, EnrolRequest, SignatureB64};
use crate::token::{TokenIssuer, TokenRecord};
use crate::transcript;

/// The explicit states of the server-side exchange.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    /// Fresh; no request accepted yet.
    AwaitingRequest,
    /// The token was consumed for this exchange and the challenge signed and handed
    /// out; the service waits for the proof.
    AwaitingProof,
    /// The proof verified; the acceptance was handed out. Terminal for this exchange.
    Accepted,
    /// The exchange failed terminally (the token stays consumed). Terminal.
    Failed,
}

/// A record of a refused proof attempt: what the caller logs to the audit trail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditRecord {
    /// The audit code of the failure (stable, machine-readable).
    pub code: String,
    /// The agent public key text as the failed request carried it, if it decoded.
    pub agent_public_key: Option<String>,
    /// The hostname the failed request carried, if shape-acceptable.
    pub hostname: Option<String>,
    /// The instant of the attempt (Unix seconds, as the caller passed in).
    pub at: i64,
}

/// The server side of one enrolment exchange. One exchange per instance: the token is
/// consumed at the request, so a second exchange with the same token is refused at
/// [`EnrolmentService::request`].
pub struct EnrolmentService {
    issuer: Arc<TokenIssuer>,
    keystore: Arc<dyn KeyStore>,
    /// The server's own base URL is not part of this crate; what the acceptance names
    /// the server with (informational, free text, for the agent's pin re-check).
    state: ServiceState,
    /// The consumed record of this exchange (the tag and issuer travel with it).
    record: Option<TokenRecord>,
    /// The request values of this exchange.
    request: Option<EnrolRequest>,
    /// The server nonce handed out with the challenge.
    nonce_s: Option<String>,
    /// Attempts refused since this exchange started (audit).
    attempts: Vec<AuditRecord>,
}

impl EnrolmentService {
    /// A service exchange over `issuer` (token consumption) and `keystore` (the
    /// server's identity key, whose public half the agent pins).
    #[must_use]
    pub fn new(issuer: Arc<TokenIssuer>, keystore: Arc<dyn KeyStore>) -> Self {
        Self {
            issuer,
            keystore,
            state: ServiceState::AwaitingRequest,
            record: None,
            request: None,
            nonce_s: None,
            attempts: Vec::new(),
        }
    }

    /// The current explicit state.
    #[must_use]
    pub fn state(&self) -> ServiceState {
        self.state
    }

    /// The audit records of the refusals this exchange has seen so far.
    #[must_use]
    pub fn audit(&self) -> &[AuditRecord] {
        &self.attempts
    }

    /// Handles message 1: shape-check the request, consume the token (atomically,
    /// once), and sign the challenge over the transcript.
    ///
    /// # Errors
    /// [`EnrolError::MessageOutOfOrder`] when this exchange already accepted a
    /// request; [`EnrolError::FieldInvalid`] when the request shape is wrong.
    /// For a refused token the return is [`EnrolmentService::RequestOutcome::Refused`]
    /// — one [`EnrolError::TokenRefused`] outcome for the agent, with the precise
    /// [`RejectReason`] kept for the audit log.
    pub fn request(
        mut self,
        request: &EnrolRequest,
        now: i64,
    ) -> Result<RequestOutcome, (EnrolError, Box<EnrolmentService>)> {
        if self.state != ServiceState::AwaitingRequest {
            return Err((EnrolError::MessageOutOfOrder, Box::new(self)));
        }
        if request.shape_ok().is_err() {
            self.attempts.push(AuditRecord {
                code: EnrolError::FieldInvalid.code(),
                agent_public_key: None,
                hostname: None,
                at: now,
            });
            return Err((EnrolError::FieldInvalid, Box::new(self)));
        }
        // The token: the one lookup. Everything after this point runs with the token
        // consumed, so a proof that fails still leaves the token spent.
        let record = match self.issuer.consume(&request.token, now) {
            Ok(record) => record,
            Err((error, reason)) => {
                self.attempts.push(AuditRecord {
                    code: reason
                        .map(|reason| reason.code())
                        .unwrap_or_else(|| error.code()),
                    agent_public_key: Some(request.agent_public_key.as_str().to_owned()),
                    hostname: Some(request.hostname.clone()),
                    at: now,
                });
                return Ok(RequestOutcome::Refused { error, reason });
            }
        };
        self.record = Some(record);
        self.request = Some(request.clone());
        self.state = ServiceState::AwaitingProof;
        self.nonce_s = Some(transcript::fresh_nonce());
        let challenge = match self.challenge_message() {
            Ok(challenge) => challenge,
            Err(error) => {
                self.state = ServiceState::Failed;
                return Err((error, Box::new(self)));
            }
        };
        Ok(RequestOutcome::Challenged {
            service: Box::new(self),
            challenge,
        })
    }

    /// Signs the challenge over the transcript of this exchange; separate so `request`
    /// stays flat.
    fn challenge_message(&self) -> Result<EnrolChallenge, EnrolError> {
        let request = self.request.as_ref().ok_or(EnrolError::MessageOutOfOrder)?;
        let nonce_s = self
            .nonce_s
            .as_deref()
            .ok_or(EnrolError::MessageOutOfOrder)?;
        let server_key = self.keystore.public_key().to_b64();
        let transcript_bytes = transcript::build(
            &request.nonce_a,
            nonce_s,
            request.agent_public_key.as_str(),
            server_key.as_str(),
            &transcript::token_digest_of(&request.token),
        )?;
        let signature = self.keystore.sign(&transcript_bytes);
        Ok(EnrolChallenge {
            server_public_key: server_key,
            nonce_s: nonce_s.to_owned(),
            signature: SignatureB64::from_bytes_unchecked(&signature.to_bytes()),
        })
    }

    /// Handles message 3: verify the proof over the same transcript with the agent
    /// key, then accept.
    ///
    /// The proof **does not carry the token or nonces**: the transcript is rebuilt
    /// from the values this exchange stored, so a tampered or replayed proof fails.
    ///
    /// # Errors
    /// [`EnrolError::MessageOutOfOrder`] when no challenge was handed out (or the
    /// exchange already finished); [`EnrolError::CryptoMalformed`] when the proof's
    /// signature text does not decode; [`EnrolError::SignatureInvalid`] when it does
    /// not verify. In both failure cases the token **stays consumed** and an audit
    /// record is appended.
    pub fn proof(
        mut self,
        proof: &EnrolProof,
        agent_id: impl Into<String>,
        now: i64,
    ) -> Result<(EnrolAccepted, ServiceState), (EnrolError, Box<EnrolmentService>)> {
        if self.state != ServiceState::AwaitingProof {
            return Err((EnrolError::MessageOutOfOrder, Box::new(self)));
        }
        let request = match self.request.as_ref() {
            Some(request) => request,
            None => return Err((EnrolError::MessageOutOfOrder, Box::new(self))),
        };
        let server_key = self.keystore.public_key().to_b64();
        let outcome: Result<(), EnrolError> = (|| {
            let nonce_s = self
                .nonce_s
                .as_deref()
                .ok_or(EnrolError::MessageOutOfOrder)?;
            let agent_key = &request.agent_public_key;
            let transcript_bytes = transcript::build(
                &request.nonce_a,
                nonce_s,
                agent_key.as_str(),
                server_key.as_str(),
                &transcript::token_digest_of(&request.token),
            )?;
            let signature = proof.signature.signature()?;
            agent_key
                .verifying_key()?
                .verify(&transcript_bytes, &signature)
                .map_err(|_| EnrolError::SignatureInvalid)?;
            Ok(())
        })();
        if let Err(error) = outcome {
            // The token stays consumed: no retry on a guess. Record the attempt.
            self.state = ServiceState::Failed;
            self.attempts.push(AuditRecord {
                code: error.code(),
                agent_public_key: Some(request.agent_public_key.as_str().to_owned()),
                hostname: Some(request.hostname.clone()),
                at: now,
            });
            return Err((error, Box::new(self)));
        }
        self.state = ServiceState::Accepted;
        Ok((
            EnrolAccepted {
                agent_id: agent_id.into(),
                server_public_key: server_key,
            },
            self.state,
        ))
    }

    /// The consumed token record of this exchange, for the caller's bookkeeping (the
    /// tag the agent will join, the issuer, the expiry).
    #[must_use]
    pub fn consumed_record(&self) -> Option<&TokenRecord> {
        self.record.as_ref()
    }

    /// Zeroizes what this exchange still holds (dropping does most of it: the token
    /// text lives in the stored request, wiped here).
    pub fn wipe(&mut self) {
        if let Some(request) = self.request.as_mut() {
            request.token.zeroize();
        }
    }
}

impl std::fmt::Debug for EnrolmentService {
    /// Redacted: the service holds the consumed token record and the request with the
    /// token text; the `Debug` names the state, nothing else.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EnrolmentService")
            .field("state", &self.state)
            .field("attempts", &self.attempts.len())
            .finish()
    }
}

/// The outcome of [`EnrolmentService::request`]: a signed challenge to hand back, or
/// the one indistinguishable refusal (with the audit reason attached internally).
pub enum RequestOutcome {
    /// The token was consumed and the challenge signed: hand the challenge back.
    Challenged {
        /// The service, advanced to [`ServiceState::AwaitingProof`], boxed to keep the
        /// outcome small.
        service: Box<EnrolmentService>,
        /// Message 2, for the agent.
        challenge: crate::message::EnrolChallenge,
    },
    /// The token was refused. `error` is always [`EnrolError::TokenRefused`]; `reason`
    /// is for the audit log only and never leaves the server process.
    Refused {
        /// The caller-facing error: the one refusal.
        error: EnrolError,
        /// The audit reason, if the refusal reached the store at all.
        reason: Option<RejectReason>,
    },
}

impl std::fmt::Debug for RequestOutcome {
    /// Redacted: the challenge carries no secrets but the outcome is not for logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Challenged { .. } => f.write_str("RequestOutcome::Challenged(***)"),
            Self::Refused { error, reason } => f
                .debug_struct("RequestOutcome::Refused")
                .field("error", error)
                .field("reason", &reason.as_ref().map(|reason| reason.code()))
                .finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{EnrolAccepted, EnrolmentService, RequestOutcome, ServiceState};
    use crate::error::EnrolError;
    use crate::keys::{KeyStore, MemoryKeyStore, Protection};
    use crate::message::{EnrolProof, EnrolRequest, PublicKeyB64};
    use crate::token::TokenIssuer;
    use std::sync::Arc;

    fn server_store() -> Arc<dyn KeyStore> {
        Arc::new(MemoryKeyStore::new(Protection::Software))
    }

    fn sample_request(token: &str, agent_key: &PublicKeyB64) -> EnrolRequest {
        EnrolRequest {
            token: token.to_owned(),
            agent_public_key: agent_key.clone(),
            hostname: "node-1".to_owned(),
            platform: "linux-x86_64".to_owned(),
            agent_version: "0.1.0".to_owned(),
            nonce_a: crate::transcript::fresh_nonce(),
        }
    }

    #[test]
    fn out_of_order_request_after_challenge_is_refused() {
        let issuer = Arc::new(TokenIssuer::new(Arc::new(
            crate::token::MemoryTokenStore::new(),
        )));
        let issued = issuer
            .issue(&crate::token::TokenSpec::default_ttl("op"), 0)
            .unwrap();
        let service = EnrolmentService::new(Arc::clone(&issuer), server_store());
        let agent_key = PublicKeyB64::from_bytes_unchecked(&[2u8; 32]);
        let outcome = match service.request(&sample_request(issued.token.as_str(), &agent_key), 0) {
            Ok(outcome) => outcome,
            Err((error, _service)) => panic!("expected a challenge, got {error:?}"),
        };
        let RequestOutcome::Challenged { service, .. } = outcome else {
            panic!("expected a challenge");
        };
        let (error, _) = service
            .request(&sample_request("second", &agent_key), 0)
            .unwrap_err();
        assert_eq!(error, EnrolError::MessageOutOfOrder);
        let _ = EnrolAccepted {
            agent_id: "a".to_owned(),
            server_public_key: agent_key,
        };
        let _ = (
            EnrolProof {
                signature: crate::message::SignatureB64::from_bytes_unchecked(&[0u8; 64]),
            },
            ServiceState::Accepted,
        );
    }
}
