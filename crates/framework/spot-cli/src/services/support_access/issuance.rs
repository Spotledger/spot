//! Issuance: the tenant side of `spot_deep_access::issue` (WP-SEC-07.T2).
//!
//! `issue` is synchronous and its receipt-nonce store blocks on PostgreSQL through the shared
//! bridge, which refuses a thread that belongs to a tokio runtime. So the call runs on its own
//! short-lived OS thread and the async caller awaits the answer. Nothing is issued unless the
//! request is `approved` with enough approvals; the crate then re-verifies the consent, the
//! attestation and the window, consumes the nonce atomically (a replay is refused) and writes
//! requested / consented / issued to the tenant audit chain.

use std::sync::Arc;

use spot_deep_access::{AccessRequest, to_hex};
use spot_deep_access::{
    ConsentRecord, CustomerTrust, DeepCredential, IssueContext, Policy, TrustedApprover,
    VendorAttestation, issue,
};
use spot_deep_access_pg::{BridgeNonces, CredentialRow, RequestStatus};
use spot_pg_bridge::Bridge;

use super::engine::{Refusal, SupportAccess, from_store};
use super::{EntropySource, IntakeError, IssueRequest, SessionLogOpener};

/// What issuance needs besides the engine. Any missing part makes `issue` answer `Unavailable`.
pub(super) struct Issuance {
    pub(super) bridge: Option<Arc<Bridge>>,
    pub(super) entropy: Option<Arc<dyn EntropySource>>,
    pub(super) logs: Option<Arc<dyn SessionLogOpener>>,
    pub(super) verifier: Option<Arc<VendorAttestation>>,
    pub(super) policy: Policy,
}

impl Issuance {
    pub(super) async fn issue(
        &self,
        engine: &SupportAccess,
        request: IssueRequest,
        now: i64,
    ) -> Result<DeepCredential, IntakeError> {
        let refused = |r: Refusal| IntakeError::Refused(r);
        let row = engine
            .store
            .get_request(&request.request_id)
            .await
            .map_err(|e| refused(from_store(e)))?
            .ok_or_else(|| refused(Refusal::NotFound))?;
        // Default deny: only an approved request can be issued from.
        if row.status != RequestStatus::Approved {
            return Err(refused(Refusal::NotPending));
        }
        let access: AccessRequest =
            serde_json::from_str(&row.request).map_err(|_| refused(Refusal::Unavailable))?;
        let consents = engine
            .store
            .consents(&row.id)
            .await
            .map_err(|e| refused(from_store(e)))?;
        let needed = engine.needed_for(&access);
        if consents.len() < needed {
            return Err(refused(Refusal::NotPending));
        }
        // The first approval is the receipt `issue` verifies and consumes; the others are the
        // second person the policy demanded, already verified when they were recorded.
        let first = consents
            .first()
            .ok_or_else(|| refused(Refusal::NotPending))?;
        let receipt = ConsentRecord::from_json(&first.record)
            .and_then(|record| record.into_receipt(first.signature.clone()))
            .map_err(IntakeError::Deep)?;
        let mut trust = CustomerTrust::default();
        for key in engine
            .store
            .keys()
            .await
            .map_err(|e| refused(from_store(e)))?
        {
            trust.add(
                &engine.tenant,
                TrustedApprover {
                    identity: key.identity,
                    public_key: key.public_key,
                    is_owner: key.is_owner,
                },
            );
        }
        let (Some(bridge), Some(entropy), Some(logs), Some(verifier)) =
            (&self.bridge, &self.entropy, &self.logs, &self.verifier)
        else {
            return Err(refused(Refusal::Unavailable));
        };
        let mut entropy_bytes = [0_u8; 48];
        entropy.fill(&mut entropy_bytes);

        let (tx, rx) = tokio::sync::oneshot::channel();
        let (bridge, logs, verifier, policy) = (
            Arc::clone(bridge),
            Arc::clone(logs),
            Arc::clone(verifier),
            self.policy.clone(),
        );
        let (operator_key, attestation) = (request.operator_key, request.attestation);
        let access_for_thread = access.clone();
        std::thread::spawn(move || {
            let mut nonces = BridgeNonces::new(bridge);
            let mut log = logs.open();
            let mut context = IssueContext {
                trust: &trust,
                attestation: verifier.as_ref(),
                nonces: &mut nonces,
                log: &mut *log,
            };
            let outcome = issue(
                &access_for_thread,
                Some(&receipt),
                operator_key,
                Some(&attestation),
                &policy,
                now,
                entropy_bytes,
                &mut context,
            );
            let _ = tx.send(outcome);
        });
        let credential = rx
            .await
            .map_err(|_| refused(Refusal::Unavailable))?
            .map_err(IntakeError::Deep)?;

        let moved = engine
            .store
            .transition(
                &row.id,
                &[RequestStatus::Approved],
                RequestStatus::Issued,
                now,
            )
            .await
            .map_err(|e| refused(from_store(e)))?;
        let scope = format!("{:?}", credential.scope());
        let issued = CredentialRow {
            session_id: credential.session_id(),
            request_id: row.id.clone(),
            request_hash: to_hex(&credential.request_hash()),
            receipt_hash: to_hex(&credential.receipt_hash()),
            scope,
            starts_at: credential.not_before(),
            expires_at: credential.not_after(),
            issued_at: now,
        };
        engine
            .store
            .record_credential(&issued, credential.secret_digest())
            .await
            .map_err(|e| refused(from_store(e)))?;
        if !moved {
            // The customer revoked between approval and issuance: the credential is born dead.
            engine
                .store
                .revoke(
                    credential.session_id(),
                    "customer",
                    "revoked before issuance",
                    now,
                )
                .await
                .map_err(|e| refused(from_store(e)))?;
            return Err(refused(Refusal::NotPending));
        }
        Ok(credential)
    }
}
