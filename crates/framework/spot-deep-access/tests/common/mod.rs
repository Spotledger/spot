#![allow(dead_code)]
use ed25519_dalek::{Signer, SigningKey};
use spot_agent_tiers::{ActionCatalogue, AgentIdentity, Authoriser, Context, Domain};
use spot_deep_access::*;
use spot_iam_access::stepup::AuthMethod;
use spot_support_session::{
    ActionScope, DocTypeScope, Scope, SessionSpec, Subject, SubjectKind, Tier,
};

#[derive(Default)]
pub struct Log {
    pub events: Vec<DeepAccessEvent>,
}
impl SessionLog for Log {
    fn tenant(&self) -> &str {
        "tenant-a"
    }
    fn append(&mut self, event: DeepAccessEvent) -> Result<(), DeepError> {
        self.events.push(event);
        Ok(())
    }
}
pub struct Fixture {
    pub customer: SigningKey,
    pub operator: SigningKey,
    pub vendor: SigningKey,
    pub request: AccessRequest,
    pub receipt: ConsentReceipt,
    pub attestation: OperatorAttestation,
    pub trust: CustomerTrust,
    pub verifier: VendorAttestation,
    pub nonces: MemoryNonces,
    pub log: Log,
    pub policy: Policy,
    pub revocations: MemoryRevocations,
    pub state: UseState,
    pub agent: AgentIdentity,
    pub authoriser: Authoriser,
}
impl Fixture {
    pub fn new() -> Self {
        let customer = SigningKey::from_bytes(&[1; 32]);
        let operator = SigningKey::from_bytes(&[2; 32]);
        let vendor = SigningKey::from_bytes(&[3; 32]);
        let scope = Scope {
            doctypes: DocTypeScope::Named(vec!["Invoice".into(), "Customer".into()]),
            actions: ActionScope::Any,
        };
        let subject = Subject::new(SubjectKind::Agent, "support-agent");
        let request = AccessRequest {
            id: "request-one".into(),
            requester: subject.clone(),
            reason: "Repair invoice".into(),
            ticket_ref: "SUP-42".into(),
            requested_at: 100,
            spec: SessionSpec {
                id: "grant-one".into(),
                tenant: "tenant-a".into(),
                subject,
                tier_ceiling: Tier::T3,
                scope: scope.clone(),
                reason: "Repair invoice".into(),
                requested_by: "support-requester".into(),
                starts_at: 100,
                expires_at: 10000,
            },
        };
        let mut trust = CustomerTrust::default();
        let customer_id = trust.add(
            "tenant-a",
            TrustedApprover {
                identity: "customer-owner".into(),
                public_key: customer.verifying_key().to_bytes(),
                is_owner: true,
            },
        );
        let mut receipt = ConsentReceipt {
            request_hash: request.hash().unwrap(),
            approver: "customer-owner".into(),
            approver_key: customer_id,
            method: AuthMethod::Hardware,
            approved_scope: scope,
            valid_from: 100,
            valid_to: 5000,
            nonce: [7; 16],
            signature: vec![],
        };
        receipt.signature = customer
            .sign(&receipt.signing_bytes().unwrap())
            .to_bytes()
            .to_vec();
        let mut verifier = VendorAttestation::default();
        let vendor_id = verifier.add_root(vendor.verifying_key().to_bytes());
        let mut attestation = OperatorAttestation {
            operator_key: operator.verifying_key().to_bytes(),
            vendor_key: vendor_id,
            kind: "hardware".into(),
            valid_from: 0,
            valid_to: 20000,
            signature: vec![],
        };
        attestation.signature = vendor
            .sign(&attestation.signing_bytes().unwrap())
            .to_bytes()
            .to_vec();
        let catalogue = ActionCatalogue::from_toml(
            r#"
[[actions]]
name = "read"
tier = "T0"
domain = "support"
effect = "read"
[[actions]]
name = "update"
tier = "T1"
domain = "support"
effect = "reversible"
[[actions]]
name = "install"
tier = "T3"
domain = "support"
effect = "structural"
"#,
        )
        .unwrap();
        let mut registry = spot_agent_tiers::AgentRegistry::default();
        registry
            .enroll(AgentIdentity::new(
                "support-agent",
                "support-owner",
                Domain::Support,
                spot_agent_tiers::Tier::T3,
            ))
            .unwrap();
        registry
            .register_deep("support-agent", "platform-security", "registration-42", 0)
            .unwrap();
        let enrolled = registry.get("support-agent").unwrap().clone();
        Self {
            customer,
            operator,
            vendor,
            request,
            receipt,
            attestation,
            trust,
            verifier,
            nonces: MemoryNonces::default(),
            log: Log::default(),
            policy: Policy::default(),
            revocations: MemoryRevocations::default(),
            state: UseState::default(),
            agent: enrolled,
            authoriser: Authoriser::new(catalogue),
        }
    }
    pub fn sign_receipt(&mut self) {
        self.receipt.signature = self
            .customer
            .sign(&self.receipt.signing_bytes().unwrap())
            .to_bytes()
            .to_vec();
    }
    pub fn issue(&mut self) -> Result<DeepCredential, DeepError> {
        let receipt = self.receipt.clone();
        let attestation = self.attestation.clone();
        self.issue_with(Some(&receipt), Some(&attestation), 100)
    }
    pub fn issue_with(
        &mut self,
        receipt: Option<&ConsentReceipt>,
        attestation: Option<&OperatorAttestation>,
        now: i64,
    ) -> Result<DeepCredential, DeepError> {
        issue(
            &self.request,
            receipt,
            self.operator.verifying_key().to_bytes(),
            attestation,
            &self.policy,
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
    pub fn action(&self) -> ActionRequest {
        ActionRequest {
            action: "read".into(),
            doctype: Some("Invoice".into()),
            tenant: "tenant-a".into(),
            arguments_hash: [5; 32],
        }
    }
    pub fn proof(&self, c: &DeepCredential, a: &ActionRequest, counter: u64) -> UseProof {
        UseProof {
            counter,
            signature: self
                .operator
                .sign(&c.proof_message(a, counter).unwrap())
                .to_bytes()
                .to_vec(),
        }
    }
    pub fn use_at(
        &mut self,
        c: &DeepCredential,
        a: &ActionRequest,
        p: &UseProof,
        now: i64,
    ) -> Decision {
        let n = self.log.events.len();
        let result = authorise(
            c,
            a,
            p,
            now,
            &mut self.state,
            &mut UseContext {
                agent: &self.agent,
                authoriser: &self.authoriser,
                hard_floor: &Context::change(),
                policy: &self.policy,
                revocations: &mut self.revocations,
                log: &mut self.log,
            },
        );
        assert_eq!(
            self.log.events.len(),
            n + 1,
            "every use leaves exactly one event"
        );
        result
    }
    pub fn step(
        &mut self,
        parent: &DeepCredential,
        receipt: Option<&ConsentReceipt>,
        now: i64,
    ) -> Result<DeepCredential, DeepError> {
        escalate(
            parent,
            &self.request,
            receipt,
            Some(&self.attestation),
            &self.policy,
            now,
            [43; 48],
            &self.revocations,
            &mut IssueContext {
                trust: &self.trust,
                attestation: &self.verifier,
                nonces: &mut self.nonces,
                log: &mut self.log,
            },
        )
    }
    pub fn operator_as_customer(&mut self) {
        self.receipt.approver_key = self.trust.add(
            "tenant-a",
            TrustedApprover {
                identity: "customer-owner".into(),
                public_key: self.operator.verifying_key().to_bytes(),
                is_owner: true,
            },
        );
        self.receipt.signature = self
            .operator
            .sign(&self.receipt.signing_bytes().unwrap())
            .to_bytes()
            .to_vec();
    }
}
