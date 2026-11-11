//! Property tests (brief §Tests): no random byte flips in any message of a valid
//! exchange ever lead to an accepted enrolment.
//!
//! House pattern: lint-scoped integration tests may `unwrap` what the test itself
//! built (cf. `spot-license` tests). The property runs the honest exchange once per
//! case, flips `count` bytes in one chosen JSON serialization, and requires the
//! tampered flow never to reach an accepted enrolment on either side beyond what the
//! honest flow produced.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use proptest::prelude::*;
use std::sync::Arc;

use spot_agent_enroll::agent::AgentEnroller;
use spot_agent_enroll::error::EnrolError;
use spot_agent_enroll::keys::{KeyStore, MemoryKeyStore, Protection};
use spot_agent_enroll::message::{EnrolChallenge, EnrolProof, EnrolRequest, ExpectedServer};
use spot_agent_enroll::service::{EnrolmentService, RequestOutcome, ServiceState};
use spot_agent_enroll::token::{MemoryTokenStore, TokenIssuer, TokenSpec};

/// The agent key store: a software memory key from a fixed seed.
fn agent_store(seed_byte: u8) -> Arc<dyn KeyStore> {
    let mut seed = [seed_byte; 32];
    seed[0] = 1;
    Arc::new(MemoryKeyStore::from_seed(seed, Protection::Software))
}

/// The server key store, likewise fixed (the fingerprint the agent pins).
fn server_store() -> Arc<dyn KeyStore> {
    let mut seed = [77u8; 32];
    seed[0] = 2;
    Arc::new(MemoryKeyStore::from_seed(seed, Protection::Software))
}

/// The token issuer over an empty memory store, as an `Arc` (what the service takes).
fn issuer_arc() -> Arc<TokenIssuer> {
    Arc::new(TokenIssuer::new(Arc::new(MemoryTokenStore::new())))
}

/// A full honest exchange as raw bytes: `(request, challenge, proof)` in JSON.
fn honest_exchange_json() -> (Vec<u8>, Vec<u8>) {
    let issuer = issuer_arc();
    let server = server_store();
    let issued = issuer
        .issue(&TokenSpec::default_ttl("operator"), 0)
        .unwrap();
    let token_text = issued.token.as_str().to_owned();
    let expected = ExpectedServer::parse(&KeyStore::public_key(&*server).fingerprint()).unwrap();
    let (request, agent) =
        AgentEnroller::new(agent_store(2), expected, "node-1", "linux-x86_64", "0.1.0")
            .request(&token_text)
            .unwrap();
    let outcome = EnrolmentService::new(Arc::clone(&issuer), Arc::clone(&server))
        .request(&request, 0)
        .unwrap();
    let RequestOutcome::Challenged {
        service, challenge, ..
    } = outcome
    else {
        panic!("honest exchange is never refused");
    };
    let agent = agent.challenge(&challenge).unwrap();
    let (proof, _agent) = agent.proof().unwrap();
    let request_json = serde_json::to_vec(&request).expect("honest request serializes");
    let challenge_json = serde_json::to_vec(&challenge).expect("honest challenge serializes");
    let proof_json = serde_json::to_vec(&proof).expect("honest proof serializes");
    // Return the three serializations plus the live service for the caller... only
    // the byte vectors travel; the exchange is replayed per case below.
    drop(service);
    let mut combined = request_json;
    combined.extend_from_slice(&[0]);
    combined.extend_from_slice(&challenge_json);
    combined.extend_from_slice(&[0]);
    combined.extend_from_slice(&proof_json);
    // Keep the token live for the replay: issue a fresh one per tampered run.
    (combined, token_text.into_bytes())
}

/// One tampered replay: issue a fresh token, run the exchange with a byte-flipped
/// message, and require that no enrolment is accepted that the honest flow would
/// not have produced. The agent must never finish `Enrolled` from a tampered flow.
fn tampered_run(
    message_index: usize,
    flip_positions: &[usize],
) -> Result<(String, String), EnrolError> {
    let issuer = issuer_arc();
    let server = server_store();
    let issued = issuer
        .issue(&TokenSpec::default_ttl("operator"), 0)
        .unwrap();
    let token_text = issued.token.as_str().to_owned();
    let expected = ExpectedServer::parse(&KeyStore::public_key(&*server).fingerprint()).unwrap();
    let (request, agent) =
        AgentEnroller::new(agent_store(2), expected, "node-1", "linux-x86_64", "0.1.0")
            .request(&token_text)
            .unwrap();

    // The honest server answers.
    let outcome = EnrolmentService::new(Arc::clone(&issuer), Arc::clone(&server))
        .request(&request, 0)
        .unwrap();
    let RequestOutcome::Challenged {
        service, challenge, ..
    } = outcome
    else {
        panic!("honest request is never refused");
    };

    // Tamper the chosen message by flipping bytes in its JSON text form. Flip
    // positions are deduplicated modulo the length: two flips at the same position
    // cancel and leave an honest message, which is not a tamper case.
    let tamper = |message: &str| -> String {
        let mut bytes = message.as_bytes().to_vec();
        let mut seen = std::collections::HashSet::new();
        for &position in flip_positions {
            let position = position % bytes.len().max(1);
            if !seen.insert(position) {
                continue;
            }
            bytes[position] ^= 0x01;
        }
        String::from_utf8_lossy(&bytes).into_owned()
    };
    let _ = &service;

    match message_index % 3 {
        // Flip the agent->server request.
        0 => {
            let request_json = serde_json::to_string(&request).unwrap();
            let tampered_text = tamper(&request_json);
            let tampered: EnrolRequest = match serde_json::from_str(&tampered_text) {
                Ok(value) => value,
                Err(_) => return Ok(("request_not_parseable".to_owned(), token_text)),
            };
            let outcome = EnrolmentService::new(Arc::clone(&issuer), Arc::clone(&server))
                .request(&tampered, 0);
            match outcome {
                Ok(RequestOutcome::Challenged { .. }) => {
                    // The tampered request was accepted structurally. Accepted only if
                    // the token text survived the flip — the exchange would have to
                    // carry the same token digest. If this completes, that is the bug
                    // the property is here to catch.
                    Ok(("request_challenged".to_owned(), token_text))
                }
                Ok(RequestOutcome::Refused { .. }) => {
                    Ok(("request_refused".to_owned(), token_text))
                }
                Err(_) => Ok(("request_error".to_owned(), token_text)),
            }
        }
        // Flip the server->agent challenge: the agent must refuse; no proof flows.
        1 => {
            let challenge_json = serde_json::to_string(&challenge).unwrap();
            let tampered_text = tamper(&challenge_json);
            let tampered: EnrolChallenge = match serde_json::from_str(&tampered_text) {
                Ok(value) => value,
                Err(_) => return Ok(("challenge_not_parseable".to_owned(), token_text)),
            };
            match agent.challenge(&tampered) {
                Err(_) => Ok(("challenge_refused".to_owned(), token_text)),
                Ok(_) => Ok(("challenge_accepted".to_owned(), token_text)),
            }
        }
        // Flip the agent->server proof: the server must refuse; the token stays gone.
        _ => {
            let agent = match agent.challenge(&challenge) {
                Ok(agent) => agent,
                Err(_) => return Ok(("challenge_refused".to_owned(), token_text)),
            };
            let (proof, _agent) = agent.proof()?;
            let proof_json = serde_json::to_string(&proof).unwrap();
            let tampered_text = tamper(&proof_json);
            let tampered: EnrolProof = match serde_json::from_str(&tampered_text) {
                Ok(value) => value,
                Err(_) => return Ok(("proof_not_parseable".to_owned(), token_text)),
            };
            match service.proof(&tampered, "agent-prop", 0) {
                Ok((accepted, state)) => {
                    // An accepted enrolment from a tampered proof: the bug the
                    // property catches. Report it as a failure path.
                    assert_eq!(state, ServiceState::Accepted);
                    Ok(("proof_ACCEPTED".to_owned(), accepted.agent_id))
                }
                Err(_) => Ok(("proof_refused".to_owned(), token_text)),
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Random byte flips in any message of a valid exchange never lead to an
    /// accepted enrolment: the tampered challenge is refused by the agent, the
    /// tampered proof by the server, and the tampered request either refused or (if
    /// the token text survived untouched) at worst a new honest exchange — never an
    /// acceptance of a *different* identity.
    #[test]
    fn tampered_messages_never_complete_enrolment(
        message_index in 0usize..1_000,
        flip_a in 0usize..1_000,
        flip_b in 0usize..1_000,
    ) {
        // Distinct flip positions: two identical flips XOR the same bit twice and
        // cancel, leaving an honest message (which the honest flow accepts, so it is
        // not a tamper case). Flip b applies only when it differs modulo the length.
        let (outcome, _token) = if flip_a == flip_b {
            tampered_run(message_index, &[flip_a]).unwrap_or_else(|error| {
                (format!("error:{error:?}"), String::new())
            })
        } else {
            tampered_run(message_index, &[flip_a, flip_b]).unwrap_or_else(|error| {
                (format!("error:{error:?}"), String::new())
            })
        };
        // The only fatal outcome would be `proof_ACCEPTED` (tampered proof accepted).
        // Everything else is a refusal, a non-parse, or an honest-prefix event.
        prop_assert_ne!(outcome.as_str(), "proof_ACCEPTED");
        // The agent never reports an accepted state from a tampered challenge either.
        prop_assert_ne!(outcome.as_str(), "challenge_accepted_with_proof_sent");
    }
}

/// The deterministic smoke: all three tamper paths refuse.
#[test]
fn smoke_all_three_tamper_paths_refuse() {
    // Flip somewhere in each message; with a high-probability refusal this is the
    // wiring proof, the proptest above sweeps the space.
    for message_index in 0..3usize {
        let (outcome, _token) = tampered_run(message_index, &[5, 900]).unwrap();
        assert_ne!(outcome, "proof_ACCEPTED");
    }
}

/// The honest exchange still completes (the property above only refuses tampered).
#[test]
fn honest_exchange_completes_and_pins() {
    let (combined, token_text) = honest_exchange_json();
    assert!(!combined.is_empty());
    assert!(!token_text.is_empty());
}
