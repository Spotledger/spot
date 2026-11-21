#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]
//! Shared fixtures for the break-glass tests: a tenant policy, a request, the
//! deep-access composition, an audit executor and a temporary directory guard.

use std::future::Future;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, Waker};

use ed25519_dalek::{Signer as _, SigningKey};
use spot_break_glass::{BreakGlassEvent, Policy, Principal, Request};
use spot_deep_access::{
    AccessRequest, AuditExecutor, ConsentReceipt, CustomerTrust, DeepCredential, DeepError,
    IssueContext, MemoryNonces, OperatorAttestation, TrustedApprover, VendorAttestation, issue,
};
use spot_iam_access::stepup::AuthMethod;
use spot_signed_command::KeyId;
use spot_support_session::{
    ActionScope, DocTypeScope, Scope, SessionSpec, Subject, SubjectKind, Tier,
};

/// The tenant every fixture is about.
pub const TENANT: &str = "tenant-a";

/// A tenant that has opted in, requires customer consent and two approvers, and caps the
/// window at four hours.
pub fn policy() -> Policy {
    Policy {
        opted_in: true,
        require_customer_consent: true,
        required_approvers: 2,
        max_duration_secs: 4 * 3600,
        settings_max_duration_secs: 4 * 3600,
        max_recording_bytes: 64 * 1024 * 1024,
    }
}

/// The scope a fixture session asks for.
pub fn scope() -> Scope {
    Scope {
        doctypes: DocTypeScope::Named(vec!["Invoice".into(), "Customer".into()]),
        actions: ActionScope::Any,
    }
}

/// A valid request over `[1000, 3000)`, made at `1000`.
pub fn request() -> Request {
    Request {
        id: "session-one".into(),
        tenant: TENANT.into(),
        requester: "support-agent".into(),
        requester_kind: SubjectKind::Agent,
        reason: "Repair a stuck invoice workflow".into(),
        ticket_id: "SUP-42".into(),
        scope: scope(),
        tier_ceiling: Tier::T2,
        starts_at: 1000,
        expires_at: 3000,
        requested_at: 1000,
    }
}

/// A vendor-side approver distinct from the requester.
pub fn vendor(id: &str) -> Principal {
    Principal::vendor(id)
}

/// Runs a ready future exactly once: the executor the audit-chain tests inject.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ready;

impl AuditExecutor for Ready {
    fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, DeepError> {
        let mut future = pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => Ok(value),
            Poll::Pending => Err(DeepError::AuditUnavailable),
        }
    }
}

/// A deep-access in-memory log, so the composed credential can be issued.
#[derive(Default)]
pub struct DeepLog {
    /// The events the deep-access issuer wrote.
    pub events: Vec<spot_deep_access::DeepAccessEvent>,
}

impl spot_deep_access::SessionLog for DeepLog {
    fn tenant(&self) -> &str {
        TENANT
    }

    fn append(&mut self, event: spot_deep_access::DeepAccessEvent) -> Result<(), DeepError> {
        self.events.push(event);
        Ok(())
    }
}

/// The deep-access composition fixture: a customer key that signs the consent receipt, a
/// vendor key that attests the operator's hardware key, and the platform trust lists.
pub struct DeepFixture {
    /// The customer's signing key.
    pub customer: SigningKey,
    /// The operator's hardware key.
    pub operator: SigningKey,
    /// The vendor's attestation root.
    pub vendor_key: SigningKey,
    /// The customer's trusted-approver list.
    pub trust: CustomerTrust,
    /// The platform's vendor attestation verifier.
    pub verifier: VendorAttestation,
    /// The single-use receipt nonce store.
    pub nonces: MemoryNonces,
    /// The deep-access session log.
    pub log: DeepLog,
    /// The deep-access policy (a generous window so the composition does not narrow it).
    pub deep_policy: spot_deep_access::Policy,
    /// The request the credential is for.
    pub access: AccessRequest,
    /// The signed consent receipt.
    pub receipt: ConsentReceipt,
    /// The signed hardware attestation.
    pub attestation: OperatorAttestation,
}

impl DeepFixture {
    /// Builds the fixture for `access`.
    pub fn new(access: AccessRequest) -> DeepFixture {
        let customer = SigningKey::from_bytes(&[1; 32]);
        let operator = SigningKey::from_bytes(&[2; 32]);
        let vendor_key = SigningKey::from_bytes(&[3; 32]);

        let mut trust = CustomerTrust::default();
        let customer_id = trust.add(
            TENANT,
            TrustedApprover {
                identity: "customer-owner".into(),
                public_key: customer.verifying_key().to_bytes(),
                is_owner: true,
            },
        );

        let mut receipt = ConsentReceipt {
            request_hash: access.hash().unwrap(),
            approver: "customer-owner".into(),
            approver_key: customer_id,
            method: AuthMethod::Hardware,
            approved_scope: access.spec.scope.clone(),
            valid_from: 0,
            valid_to: 4000,
            nonce: [7; 16],
            signature: Vec::new(),
        };
        receipt.signature = customer
            .sign(&receipt.signing_bytes().unwrap())
            .to_bytes()
            .to_vec();

        let mut verifier = VendorAttestation::default();
        let vendor_id = verifier.add_root(vendor_key.verifying_key().to_bytes());
        let mut attestation = OperatorAttestation {
            operator_key: operator.verifying_key().to_bytes(),
            vendor_key: vendor_id,
            kind: "hardware".into(),
            valid_from: 0,
            valid_to: 8000,
            signature: Vec::new(),
        };
        attestation.signature = vendor_key
            .sign(&attestation.signing_bytes().unwrap())
            .to_bytes()
            .to_vec();

        let deep_policy = spot_deep_access::Policy {
            max_session: [4 * 3600, 4 * 3600, 4 * 3600, 4 * 3600],
            protected_actions: std::collections::BTreeSet::new(),
            max_denials: 5,
        };

        DeepFixture {
            customer,
            operator,
            vendor_key,
            trust,
            verifier,
            nonces: MemoryNonces::default(),
            log: DeepLog::default(),
            deep_policy,
            access,
            receipt,
            attestation,
        }
    }

    /// The customer approver key id, for the recorded consent commitment.
    pub fn approver_key(&self) -> KeyId {
        self.receipt.approver_key.clone()
    }

    /// Issues the composed credential through `spot-deep-access::issue`.
    pub fn issue(&mut self, now: i64) -> Result<DeepCredential, DeepError> {
        let receipt = self.receipt.clone();
        let attestation = self.attestation.clone();
        let operator_key = self.operator.verifying_key().to_bytes();
        issue(
            &self.access,
            Some(&receipt),
            operator_key,
            Some(&attestation),
            &self.deep_policy,
            now,
            [42; 48],
            &mut IssueContext {
                trust: &self.trust,
                attestation: &self.verifier,
                nonces: &mut self.nonces,
                log: &mut self.log,
            },
        )
    }
}

/// A request whose subject kind is a person (the default is an agent).
pub fn subject_for(request: &Request) -> Subject {
    request.subject()
}

/// A temporary directory that removes itself when dropped.
pub struct TempDir {
    path: PathBuf,
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

impl TempDir {
    /// Creates a unique temporary directory named after `tag`.
    pub fn new(tag: &str) -> TempDir {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let path =
            std::env::temp_dir().join(format!("spot-break-glass-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        TempDir { path }
    }

    /// The directory path.
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// The session id a fixture request uses.
pub const SESSION: &str = "session-one";

/// A helper to build an event list from a log.
pub fn events(log: &spot_break_glass::MemoryLog) -> &[BreakGlassEvent] {
    log.events()
}

/// A session spec, for building a scoped grant directly.
pub fn session_spec() -> SessionSpec {
    request().session_spec()
}
