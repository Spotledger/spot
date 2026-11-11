//! Shared fixture for the crate's integration tests.
#![allow(dead_code)]

use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use spot_agent_tiers::{AgentIdentity, AgentRegistry, Domain, Status, Tier as AgentTier};
use spot_audit_chain::{
    AuditChain, AuditStore, CheckpointPolicy, InMemoryStore, KeyRing, LocalSigner,
};
use spot_support_grants::{
    Action, AuditExecutor, ChainLog, ControlPlane, CustomerAdmins, CustomerView, Grant, GrantError,
    GrantPolicy, GrantSpec, MemoryGrantStore, PendingGrant, SessionRecord, SessionRef,
};
use spot_support_session::{
    ActionScope, DocTypeScope, Scope, SessionSpec, Subject, SubjectKind, Tier,
};

pub const TENANT: &str = "tenant-a";
pub const AGENT: &str = "support-agent";
pub const ADMIN: &str = "admin-1";
pub const ADMIN2: &str = "admin-2";
pub const REQUESTER: &str = "requester-1";
pub const ORG: &str = "support-org";

/// A synchronous executor for the in-memory store: every future is ready on the first poll.
pub struct Ready;

impl AuditExecutor for Ready {
    fn run<T>(&self, future: impl Future<Output = T>) -> Result<T, GrantError> {
        let mut future = pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => Ok(value),
            Poll::Pending => Err(GrantError::AuditUnavailable),
        }
    }
}

/// A scope naming DocTypes and actions.
pub fn scope(doctypes: &[&str], actions: &[&str]) -> Scope {
    Scope {
        doctypes: DocTypeScope::Named(doctypes.iter().map(|s| (*s).to_owned()).collect()),
        actions: ActionScope::Only(actions.iter().map(|s| (*s).to_owned()).collect()),
    }
}

/// A scope reaching every DocType, read only.
pub fn all_read() -> Scope {
    Scope {
        doctypes: DocTypeScope::AllRead,
        actions: ActionScope::Any,
    }
}

/// A grant request for the support agent.
pub fn agent_spec(id: &str, tier: Tier, scope: Scope, start: i64, end: i64) -> GrantSpec {
    spec(id, AGENT, SubjectKind::Agent, tier, scope, start, end)
}

/// A grant request for any subject.
pub fn spec(
    id: &str,
    subject: &str,
    kind: SubjectKind,
    tier: Tier,
    scope: Scope,
    start: i64,
    end: i64,
) -> GrantSpec {
    GrantSpec {
        session: SessionSpec {
            id: id.to_owned(),
            tenant: TENANT.to_owned(),
            subject: Subject::new(kind, subject),
            tier_ceiling: tier,
            scope,
            reason: "the customer reported a broken invoice workflow".to_owned(),
            requested_by: REQUESTER.to_owned(),
            starts_at: start,
            expires_at: end,
        },
        support_org: ORG.to_owned(),
        conditions: Vec::new(),
    }
}

/// A session reference for `grant`.
pub fn session(grant: &str) -> SessionRef {
    SessionRef {
        tenant: TENANT.to_owned(),
        grant_id: grant.to_owned(),
        support_org: ORG.to_owned(),
        subject_id: AGENT.to_owned(),
    }
}

/// A read action on `doctype`.
pub fn read(doctype: &str) -> Action {
    Action::new("read", Tier::T0, TENANT).on(doctype)
}

/// An action of `tier` named `name`.
pub fn action(name: &str, tier: Tier, doctype: Option<&str>) -> Action {
    let action = Action::new(name, tier, TENANT);
    match doctype {
        Some(doctype) => action.on(doctype),
        None => action,
    }
}

/// A log that fails every append, for the fail-closed test.
pub struct FailingLog {
    tenant: String,
}

impl FailingLog {
    pub fn new(tenant: &str) -> FailingLog {
        FailingLog {
            tenant: tenant.to_owned(),
        }
    }
}

impl spot_support_grants::CustomerLog for FailingLog {
    fn tenant(&self) -> &str {
        &self.tenant
    }

    fn append(&mut self, _event: spot_support_grants::GrantEvent) -> Result<(), GrantError> {
        Err(GrantError::AuditUnavailable)
    }
}

/// A tenant fixture: store, customer chain, administrators, agents and policy.
pub struct Harness<S: AuditStore = InMemoryStore> {
    pub grants: MemoryGrantStore,
    pub log: ChainLog<S, Ready>,
    pub admins: CustomerAdmins,
    pub agents: AgentRegistry,
    pub policy: GrantPolicy,
    pub keys: KeyRing,
}

impl Harness<InMemoryStore> {
    /// The standard fixture on an in-memory chain.
    pub fn new() -> Harness<InMemoryStore> {
        Harness::with_store(InMemoryStore::new())
    }
}

impl<S: AuditStore> Harness<S> {
    /// A fixture over `store` as the tenant's chain.
    pub fn with_store(store: S) -> Harness<S> {
        let signer = Arc::new(LocalSigner::from_seed([7; 32]));
        let mut keys = KeyRing::new();
        keys.add(&signer.public_key())
            .expect("the local key is valid");
        let chain = AuditChain::new(store, TENANT, signer, CheckpointPolicy { every: 1 });
        let mut admins = CustomerAdmins::new();
        admins.add(TENANT, ADMIN, true);
        admins.add(TENANT, ADMIN2, false);
        admins.add("tenant-b", "admin-b", true);
        let mut agents = AgentRegistry::new();
        agents
            .enroll(AgentIdentity::new(
                AGENT,
                ORG,
                Domain::Support,
                AgentTier::T3,
            ))
            .expect("the support agent enrols");
        agents
            .enroll(AgentIdentity::new(
                "ops-agent",
                "ops-org",
                Domain::Operations,
                AgentTier::T3,
            ))
            .expect("the operations agent enrols");
        agents
            .enroll(AgentIdentity::new(
                "low-agent",
                ORG,
                Domain::Support,
                AgentTier::T0,
            ))
            .expect("the low-tier agent enrols");
        agents
            .enroll(
                AgentIdentity::new("suspended-agent", ORG, Domain::Support, AgentTier::T3)
                    .with_status(Status::Suspended),
            )
            .expect("the suspended agent enrols");
        Harness {
            grants: MemoryGrantStore::new(),
            log: ChainLog::new(chain, Ready),
            admins,
            agents,
            policy: GrantPolicy::default(),
            keys,
        }
    }

    /// A control plane over this fixture.
    pub fn plane(&mut self) -> ControlPlane<'_> {
        ControlPlane::new(
            &mut self.grants,
            &mut self.log,
            &self.admins,
            &self.agents,
            &self.policy,
        )
    }

    /// The customer's view over this fixture's chain.
    pub fn view(&self) -> CustomerView<'_, S, Ready> {
        CustomerView::new(self.log.chain(), &self.keys, self.log.executor())
    }

    /// The sessions the customer sees, over the whole range.
    pub fn sessions(&self) -> Vec<SessionRecord> {
        self.view()
            .sessions(TENANT, 0..=i64::MAX)
            .expect("the tenant chain reads")
    }

    /// A requested grant awaiting approval.
    pub fn pending(
        &mut self,
        id: &str,
        tier: Tier,
        scope: Scope,
        start: i64,
        end: i64,
    ) -> PendingGrant {
        self.plane()
            .request(agent_spec(id, tier, scope, start, end), start)
            .expect("the request is valid")
    }

    /// An approved grant.
    pub fn grant(&mut self, id: &str, tier: Tier, scope: Scope, start: i64, end: i64) -> Grant {
        let pending = self.pending(id, tier, scope, start, end);
        self.plane()
            .approve(pending, ADMIN, start)
            .expect("the approval is valid")
    }
}
