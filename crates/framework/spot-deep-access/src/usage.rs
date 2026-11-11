//! Proof-of-possession use, exactly one decision event and anomaly break-off.
use crate::attestation::verify_signature;
use crate::log::{event, session_text};
use crate::model::{canonical, hash};
use crate::{
    DeepCredential, DeepError, Hash, Policy, Revocation, RevocationStore, SessionLog, UseState,
};
use spot_agent_tiers::{AgentIdentity, Approval, Authoriser, Context, Decision as TierDecision};
use spot_support_session::{ActionRequest as ScopeAction, Tier};

/// One operation. Arguments stay outside the log; only their digest is signed.
#[derive(Clone, Debug)]
pub struct ActionRequest {
    /// Catalogue operation name.
    pub action: String,
    /// Target DocType, if applicable.
    pub doctype: Option<String>,
    /// Tenant target.
    pub tenant: String,
    /// SHA-256 of the transport's canonical argument bytes.
    pub arguments_hash: Hash,
}
/// Signature made by the attested operator hardware key.
#[derive(Clone, Debug)]
pub struct UseProof {
    /// Strictly increasing counter, shared across all uses of a session.
    pub counter: u64,
    /// Raw Ed25519 signature of DeepCredential::proof_message().
    pub signature: Vec<u8>,
}
impl DeepCredential {
    /// Exact hardware-signature message, binding session, action, target, arguments,
    /// counter, consent and a commitment to the per-credential secret.
    /// Returning this message conveys no bearer authority; signing needs the hardware key.
    pub fn proof_message(
        &self,
        action: &ActionRequest,
        counter: u64,
    ) -> Result<Vec<u8>, DeepError> {
        canonical(
            b"SPOT-DEEP-USE-V1\n",
            &(
                self.session_id,
                &action.action,
                &action.tenant,
                &action.doctype,
                action.arguments_hash,
                counter,
                self.receipt_hash,
                hash(&self.secret),
            ),
        )
    }
}
/// Platform-owned use context, reloaded for every action. The operator never supplies
/// catalogue tiers, agent status, hard-floor evidence, revocations or the log writer.
pub struct UseContext<'a> {
    /// Current enrolled identity for this support principal.
    pub agent: &'a AgentIdentity,
    /// Trusted action catalogue and tier authoriser.
    pub authoriser: &'a Authoriser,
    /// Current journal, snapshot and financial-route evidence.
    pub hard_floor: &'a Context,
    /// Current policy, including protected-group catalogue operations.
    pub policy: &'a Policy,
    /// Shared irreversible revocation store, also used by anomaly rules.
    pub revocations: &'a mut dyn RevocationStore,
    /// Tenant-owned log writer.
    pub log: &'a mut dyn SessionLog,
}
/// One returned and recorded decision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    /// Whether all checks passed and the event was durably written.
    pub allowed: bool,
    /// Typed refusal, absent on allow.
    pub refusal: Option<DeepError>,
    /// Plain-language reason.
    pub reason: String,
}
impl Decision {
    /// Stable outcome code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.refusal.map_or("deep_access.allow", DeepError::code)
    }
}
/// Check every use against revocation, scope, tier, step-up, signature and replay state.
/// Every call attempts exactly one customer decision event, including refusals at
/// any check. A failed log write changes any allow to a refusal; execution must stop.
pub fn authorise(
    credential: &DeepCredential,
    action: &ActionRequest,
    proof: &UseProof,
    now: i64,
    state: &mut UseState,
    context: &mut UseContext<'_>,
) -> Decision {
    let checked = check(credential, action, proof, now, state, context);
    let mut decision = match checked {
        Ok(()) => Decision { allowed: true, refusal: None, reason: "The customer consent, hardware signature, scope and tier checks permit this action.".to_owned() },
        Err(error) => Decision { allowed: false, refusal: Some(error), reason: error.to_string() },
    };
    let session = state.sessions.entry(credential.session_id).or_default();
    let mut anomaly = None;
    if decision.allowed {
        session.counter = Some(proof.counter);
        session.denials = 0;
        if credential.step_action.is_some() {
            session.steps.insert(credential.receipt_hash);
        }
    } else {
        session.denials = session.denials.saturating_add(1);
        if session.denials >= credential.max_denials.min(context.policy.max_denials)
            || matches!(
                decision.refusal,
                Some(DeepError::Expired | DeepError::NotStarted)
            )
        {
            let reason = "Session stopped by the platform anomaly rule after denied or out-of-window access.";
            let revoked = context.revocations.insert(
                credential.tenant(),
                credential.session_id,
                Revocation {
                    by: "platform:anomaly".to_owned(),
                    reason: reason.to_owned(),
                    at: now,
                },
            );
            anomaly = Some(reason);
            if let Err(error) = revoked {
                decision.refusal = Some(error);
                decision.reason = error.to_string();
            }
        }
    }
    let kind = if decision.allowed {
        "used"
    } else if decision.refusal == Some(DeepError::Expired) {
        "expired"
    } else {
        "denied"
    };
    let summary = format!(
        "{} on {}: {}{}",
        action.action,
        action.doctype.as_deref().unwrap_or("tenant"),
        decision.reason,
        anomaly.map_or(String::new(), |r| format!(" {r}"))
    );
    let logged = if context.log.tenant() != credential.tenant() {
        Err(DeepError::AuditUnavailable)
    } else {
        context.log.append(event(
            kind,
            &session_text(credential.session_id),
            &credential.grant.subject.id,
            summary,
            decision.code(),
            now,
        ))
    };
    if logged.is_err() {
        decision = Decision {
            allowed: false,
            refusal: Some(DeepError::AuditUnavailable),
            reason: DeepError::AuditUnavailable.to_string(),
        };
        // A write may have reached storage before its acknowledgement was lost. Revoke
        // fail-closed rather than letting the same proof later execute unexpectedly.
        let _ = context.revocations.insert(
            credential.tenant(),
            credential.session_id,
            Revocation {
                by: "platform:audit".to_owned(),
                reason: "Audit acknowledgement failed".to_owned(),
                at: now,
            },
        );
    }
    decision
}
fn check(
    credential: &DeepCredential,
    action: &ActionRequest,
    proof: &UseProof,
    now: i64,
    state: &UseState,
    context: &UseContext<'_>,
) -> Result<(), DeepError> {
    if context
        .revocations
        .lookup(credential.tenant(), credential.session_id)?
        .is_some()
    {
        return Err(DeepError::Revoked);
    }
    if now < credential.not_before() {
        return Err(DeepError::NotStarted);
    }
    if now >= credential.not_after() {
        return Err(DeepError::Expired);
    }
    let spec = context
        .authoriser
        .catalogue()
        .get(&action.action)
        .ok_or(DeepError::TierRefused)?;
    let tier = match spec.tier {
        spot_agent_tiers::Tier::T0 => Tier::T0,
        spot_agent_tiers::Tier::T1 => Tier::T1,
        spot_agent_tiers::Tier::T2 => Tier::T2,
        spot_agent_tiers::Tier::T3 => Tier::T3,
    };
    if tier > credential.tier() {
        return Err(DeepError::StepUpRequired);
    }
    credential
        .grant
        .check(
            &ScopeAction {
                action: action.action.clone(),
                tier,
                doctype: action.doctype.clone(),
                tenant: action.tenant.clone(),
            },
            now,
        )
        .map_err(|_| DeepError::OutOfScope)?;
    if context.agent.id != credential.grant.subject.id
        || (credential.grant.subject.kind == spot_support_session::SubjectKind::Agent
            && !context.agent.registered_deep())
    {
        return Err(DeepError::TierRefused);
    }
    let mut facts = context.hard_floor.clone();
    facts.human_approval = Some(Approval {
        approver: credential.grant.approved_by.clone(),
        reference: session_text(credential.session_id),
        at: credential.not_before(),
    });
    if !matches!(
        context
            .authoriser
            .decide(context.agent, &action.action, &facts),
        TierDecision::Allow { .. }
    ) {
        return Err(DeepError::TierRefused);
    }
    let session = state.sessions.get(&credential.session_id);
    let needs_step = credential.protected_actions.contains(&action.action)
        || context.policy.protected_actions.contains(&action.action)
        || credential.step_action.is_some();
    if needs_step
        && (credential.step_action.as_deref() != Some(&action.action)
            || session.is_some_and(|s| s.steps.contains(&credential.receipt_hash)))
    {
        return Err(DeepError::StepUpRequired);
    }
    if session
        .and_then(|s| s.counter)
        .is_some_and(|counter| proof.counter <= counter)
    {
        return Err(DeepError::ReplayedUse);
    }
    verify_signature(
        &credential.operator_key,
        &credential.proof_message(action, proof.counter)?,
        &proof.signature,
    )
    .map_err(|_| DeepError::InvalidProof)
}
/// Irreversibly break off a session. The platform authenticates the customer's caller
/// before invoking this function; the store commits first so an audit outage cannot
/// delay revocation.
pub fn revoke(
    credential: &DeepCredential,
    by: &str,
    reason: &str,
    now: i64,
    store: &mut dyn RevocationStore,
    log: &mut dyn SessionLog,
) -> Result<(), DeepError> {
    store.insert(
        credential.tenant(),
        credential.session_id,
        Revocation {
            by: by.to_owned(),
            reason: reason.to_owned(),
            at: now,
        },
    )?;
    if log.tenant() != credential.tenant() {
        return Err(DeepError::AuditUnavailable);
    }
    log.append(event(
        "revoked",
        &session_text(credential.session_id),
        by,
        reason.to_owned(),
        "deep_access.revoked",
        now,
    ))
}
