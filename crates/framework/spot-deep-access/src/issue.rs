//! The only credential constructor and fresh-receipt step-up.
use crate::attestation::verify_signature;
use crate::log::{event, session_text};
use crate::model::tier_index;
use crate::{
    AccessRequest, ConsentReceipt, CustomerTrust, DeepCredential, DeepError, KeyAttestation,
    OperatorAttestation, Policy, ReceiptNonces, RevocationView, SessionLog,
};
use spot_iam_access::stepup::AuthMethod;
use spot_support_session::{
    ActionScope, Approver, DocTypeScope, Scope, SessionGrant, SessionPolicy, Tier,
};

/// Platform-only issuance inputs. Trust and replay state must come from the tenant,
/// never from the operator's request.
pub struct IssueContext<'a> {
    /// Customer-owned approver trust list.
    pub trust: &'a CustomerTrust,
    /// Platform hardware verification boundary.
    pub attestation: &'a dyn KeyAttestation,
    /// Shared authoritative replay state.
    pub nonces: &'a mut dyn ReceiptNonces,
    /// Tenant-owned customer log writer.
    pub log: &'a mut dyn SessionLog,
}

pub(crate) fn scope_subset(narrow: &Scope, wide: &Scope) -> bool {
    let documents = match (&narrow.doctypes, &wide.doctypes) {
        (DocTypeScope::AllRead, DocTypeScope::AllRead) => true,
        (DocTypeScope::Named(_), DocTypeScope::AllRead) => true,
        (DocTypeScope::Named(a), DocTypeScope::Named(b)) => a.iter().all(|v| b.contains(v)),
        _ => false,
    };
    let actions = match (&narrow.actions, &wide.actions) {
        (_, ActionScope::Any) => true,
        (ActionScope::Only(a), ActionScope::Only(b)) => a.iter().all(|v| b.contains(v)),
        _ => false,
    };
    documents && actions
}

/// Issue a credential only after exact customer consent and hardware verification.
/// `entropy` must contain 48 fresh CSPRNG bytes supplied by the platform, not the operator.
/// All times are Unix seconds. Receipt nonces are consumed only after verification,
/// and before logging/issuance, so failures cannot create a replay window.
#[allow(clippy::too_many_arguments)]
pub fn issue(
    request: &AccessRequest,
    receipt: Option<&ConsentReceipt>,
    operator_key: [u8; 32],
    attestation: Option<&OperatorAttestation>,
    policy: &Policy,
    now: i64,
    entropy: [u8; 48],
    context: &mut IssueContext<'_>,
) -> Result<DeepCredential, DeepError> {
    if context.log.tenant() != request.spec.tenant {
        return Err(DeepError::AuditUnavailable);
    }
    let requested = request.hash()?;
    let mut session_id = [0; 16];
    session_id.copy_from_slice(&entropy[..16]);
    let session = session_text(session_id);
    context.log.append(event(
        "requested",
        &session,
        &request.requester.id,
        format!(
            "Requested {} for ticket {}: {}. Scope {:?}; window [{}, {}).",
            request.id,
            request.ticket_ref,
            request.reason,
            request.spec.scope,
            request.spec.starts_at,
            request.spec.expires_at
        ),
        "deep_access.requested",
        now,
    ))?;
    let result = validate(
        request,
        receipt,
        &operator_key,
        attestation,
        policy,
        now,
        context,
    );
    let (receipt, attestation, grant) = match result {
        Ok(value) => value,
        Err(error) => {
            context.log.append(event(
                "denied",
                &session,
                &request.requester.id,
                error.to_string(),
                error.code(),
                now,
            ))?;
            return Err(error);
        }
    };
    let receipt_hash = receipt.hash()?;
    let attestation_hash = attestation.hash()?;
    if !context
        .nonces
        .consume(&request.spec.tenant, &receipt.approver_key, receipt.nonce)?
    {
        return Err(DeepError::ReplayedReceipt);
    }
    context.log.append(event(
        "consented",
        &session,
        &receipt.approver,
        format!(
            "Approved ticket {} with {:?}; scope {:?}.",
            request.ticket_ref, receipt.method, grant.scope
        ),
        "deep_access.consented",
        now,
    ))?;
    context.log.append(event(
        "issued",
        &session,
        &request.requester.id,
        format!(
            "Access for {}: {}; ticket {}; scope {:?}; window [{}, {}).",
            request.requester.id,
            request.reason,
            request.ticket_ref,
            grant.scope,
            grant.starts_at,
            grant.expires_at
        ),
        "deep_access.issued",
        now,
    ))?;
    let mut secret = [0; 32];
    secret.copy_from_slice(&entropy[16..]);
    Ok(DeepCredential {
        grant,
        request_hash: requested,
        receipt_hash,
        operator_key,
        operator_key_id: spot_signed_command::KeyId::from_public_key(&operator_key),
        attestation_hash,
        session_id,
        secret,
        step_action: None,
        protected_actions: policy.protected_actions.clone(),
        max_denials: policy.max_denials,
    })
}

#[allow(clippy::too_many_arguments)]
fn validate<'a>(
    request: &AccessRequest,
    receipt: Option<&'a ConsentReceipt>,
    operator_key: &[u8; 32],
    attestation: Option<&'a OperatorAttestation>,
    policy: &Policy,
    now: i64,
    context: &IssueContext<'_>,
) -> Result<(&'a ConsentReceipt, &'a OperatorAttestation, SessionGrant), DeepError> {
    let receipt = receipt.ok_or(DeepError::MissingReceipt)?;
    if receipt.request_hash != request.hash()? {
        return Err(DeepError::WrongRequest);
    }
    if request.id.trim().is_empty()
        || request.reason.trim().is_empty()
        || request.ticket_ref.trim().is_empty()
        || request.requester != request.spec.subject
        || request.requested_at > now
        || request.reason != request.spec.reason
    {
        return Err(DeepError::InvalidRequest);
    }
    let trusted = context
        .trust
        .get(&request.spec.tenant, &receipt.approver_key)
        .ok_or(DeepError::UnknownApprover)?;
    if trusted.identity != receipt.approver {
        return Err(DeepError::UnknownApprover);
    }
    if receipt.approver == request.requester.id
        || receipt.approver == request.spec.requested_by
        || trusted.public_key == *operator_key
    {
        return Err(DeepError::OperatorApproval);
    }
    verify_signature(
        &trusted.public_key,
        &receipt.signing_bytes()?,
        &receipt.signature,
    )?;
    if now < receipt.valid_from {
        return Err(DeepError::NotYetValid);
    }
    if now >= receipt.valid_to {
        return Err(DeepError::ExpiredReceipt);
    }
    if !scope_subset(&receipt.approved_scope, &request.spec.scope) {
        return Err(DeepError::WidenedScope);
    }
    // AllRead can only be narrowed to named read-only access, never turned into a write grant.
    if matches!(request.spec.scope.doctypes, DocTypeScope::AllRead)
        && request.spec.tier_ceiling != Tier::T0
    {
        return Err(DeepError::InvalidRequest);
    }
    let tier = request.spec.tier_ceiling;
    if (tier == Tier::T3 && receipt.method != AuthMethod::Hardware)
        || (tier == Tier::T2
            && !matches!(
                receipt.method,
                AuthMethod::PasswordTotp | AuthMethod::Hardware
            ))
    {
        return Err(DeepError::WeakMethod);
    }
    let attestation = attestation.ok_or(DeepError::MissingAttestation)?;
    context.attestation.verify(attestation, operator_key, now)?;
    if policy.max_session.iter().any(|n| *n <= 0) || policy.max_denials == 0 {
        return Err(DeepError::InvalidPolicy);
    }
    let max = policy.max_session[tier_index(tier)];
    let original_policy = SessionPolicy {
        max_duration_secs: i64::MAX,
        ..SessionPolicy::default()
    };
    SessionGrant::request(&request.spec, &original_policy, request.requested_at)
        .map_err(|_| DeepError::InvalidRequest)?;
    let mut spec = request.spec.clone();
    spec.scope = receipt.approved_scope.clone();
    spec.starts_at = now.max(receipt.valid_from).max(spec.starts_at);
    spec.expires_at = spec
        .expires_at
        .min(receipt.valid_to)
        .min(attestation.valid_to)
        .min(
            spec.starts_at
                .checked_add(max)
                .ok_or(DeepError::InvalidPolicy)?,
        );
    let session_policy = SessionPolicy {
        max_duration_secs: max,
        ..SessionPolicy::default()
    };
    let pending = SessionGrant::request(&spec, &session_policy, now)
        .map_err(|_| DeepError::InvalidRequest)?;
    let (grant, _) = pending
        .approve(
            &Approver::new(&receipt.approver, trusted.is_owner),
            now,
            &session_policy,
        )
        .map_err(|_| DeepError::UnknownApprover)?;
    Ok((receipt, attestation, grant))
}

/// Obtain fresh consent limited to one action, never extending the original session.
/// A new receipt nonce is mandatory, even if the action's tier does not rise.
/// The replacement session is also immediately refused if its parent is revoked.
#[allow(clippy::too_many_arguments)]
pub fn escalate(
    parent: &DeepCredential,
    request: &AccessRequest,
    receipt: Option<&ConsentReceipt>,
    attestation: Option<&OperatorAttestation>,
    policy: &Policy,
    now: i64,
    entropy: [u8; 48],
    revocations: &dyn RevocationView,
    context: &mut IssueContext<'_>,
) -> Result<DeepCredential, DeepError> {
    if revocations
        .lookup(parent.tenant(), parent.session_id())?
        .is_some()
    {
        return Err(DeepError::Revoked);
    }
    if now < parent.not_before() || now >= parent.not_after() {
        return Err(DeepError::Expired);
    }
    let receipt = receipt.ok_or(DeepError::MissingReceipt)?;
    let ActionScope::Only(actions) = &receipt.approved_scope.actions else {
        return Err(DeepError::InvalidEscalation);
    };
    if actions.len() != 1
        || receipt.hash()? == parent.receipt_hash
        || receipt.valid_to > parent.not_after()
        || request.spec.tenant != parent.tenant()
        || request.spec.subject != parent.grant.subject
        || request.spec.expires_at > parent.not_after()
        || receipt.valid_from < now
    {
        return Err(DeepError::InvalidEscalation);
    }
    // Document scope can only narrow. Tier and action are separately consented anew.
    let mut documents = receipt.approved_scope.clone();
    documents.actions = parent.scope().actions.clone();
    if !scope_subset(&documents, parent.scope()) {
        return Err(DeepError::InvalidEscalation);
    }
    let mut entropy = entropy;
    entropy[..16].copy_from_slice(&parent.session_id);
    let mut credential = issue(
        request,
        Some(receipt),
        parent.operator_key,
        attestation,
        policy,
        now,
        entropy,
        context,
    )?;
    credential.step_action = Some(actions[0].clone());
    // Preserve the original revocation identity: customer break-off stops all escalations.
    context.log.append(event(
        "escalated",
        &session_text(parent.session_id),
        &parent.grant.subject.id,
        format!(
            "Fresh customer consent for {} at {:?}; end remains {}.",
            actions[0],
            credential.tier(),
            credential.not_after()
        ),
        "deep_access.escalated",
        now,
    ))?;
    Ok(credential)
}
