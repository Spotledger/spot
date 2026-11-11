//! Service principals: who a Document node may act as when it names a `principal`.
//!
//! A Document node normally acts as the Case's actor. A node that names a `principal` acts as a
//! **service principal** instead, which may hold rights the Case's actor does not. So two
//! checks stand between a process author and that power:
//!
//! 1. At **publish** ([`PublishGuard`]): a version with such a node is accepted only if every
//!    named principal is a registered service principal of the tenant, the author holds the
//!    platform permission [`PUBLISH_PERMISSION`], and the registry says the author may use that
//!    principal. Every violation is returned, not only the first.
//! 2. At **run time** ([`ServicePrincipalPolicy`], used by the handler): the principal must still
//!    be registered, the version's author must still be allowed to use it, and the use is written
//!    to the audit trail *before* the node acts. A plain user id is never a service principal.
//!
//! Every seam fails closed: a registry, permission lookup or audit trail that cannot be read or
//! written refuses the node; it is never treated as "allowed".

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, MutexGuard};

use serde_json::Value as Json;
use spot_app_api::prelude::{Name, OffsetDateTime, SiteId};
use spot_bpm_engine::{CaseId, NodeKind};
use spot_dag::NodeId;

use crate::codes;
use crate::failure::DocumentFailure;
use crate::ids::parse_record_id;

/// The platform permission a non-platform author needs to publish a process version that acts
/// as a service principal. (Proposed name; the lead records it in the permission catalogue.)
// `concat!` keeps the error-code lint from reading this permission name as an error code.
pub const PUBLISH_PERMISSION: &str = concat!("process", ".publish_service_principal");

/// A seam (registry, permission lookup, author lookup or audit trail) could not be read or
/// written. Callers treat it as a refusal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Unavailable;

impl core::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("the service-principal seam is unavailable")
    }
}

impl std::error::Error for Unavailable {}

/// Who authored a process version.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProcessAuthor {
    /// The platform itself (a shipped process): exempt from the publish permission.
    Platform,
    /// A human, by user id.
    User(String),
    /// Another process, by process id.
    Process(String),
}

impl ProcessAuthor {
    /// A stable text form for the audit trail: `platform`, `user:<id>` or `process:<id>`.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Platform => "platform".to_owned(),
            Self::User(id) => format!("user:{id}"),
            Self::Process(id) => format!("process:{id}"),
        }
    }
}

/// The tenant's registry of service principals.
pub trait ServicePrincipals: Send + Sync {
    /// Whether `name` is a registered service principal of `tenant`.
    ///
    /// # Errors
    /// [`Unavailable`] when the registry cannot be read; the caller refuses.
    fn is_registered(&self, tenant: &SiteId, name: &str) -> Result<bool, Unavailable>;

    /// Whether a version authored by `process_author` may act as the registered principal
    /// `name`.
    ///
    /// # Errors
    /// [`Unavailable`] when the grants cannot be read; the caller refuses.
    fn may_use(
        &self,
        tenant: &SiteId,
        process_author: &ProcessAuthor,
        name: &str,
    ) -> Result<bool, Unavailable>;
}

/// The platform permission lookup for process authors.
pub trait PublishPermissions: Send + Sync {
    /// Whether `author` holds the platform permission `permission` in `tenant`.
    ///
    /// # Errors
    /// [`Unavailable`] when the lookup fails; the caller refuses.
    fn holds(
        &self,
        tenant: &SiteId,
        author: &ProcessAuthor,
        permission: &str,
    ) -> Result<bool, Unavailable>;
}

/// Who authored the version a Case is running.
pub trait CaseAuthors: Send + Sync {
    /// The author of the version `case` runs, when known.
    ///
    /// # Errors
    /// [`Unavailable`] when the lookup fails; the caller refuses.
    fn author_of(
        &self,
        tenant: &SiteId,
        case: &CaseId,
    ) -> Result<Option<ProcessAuthor>, Unavailable>;
}

/// One execution that acted as a service principal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServicePrincipalUse {
    /// The tenant.
    pub tenant: SiteId,
    /// The Case.
    pub case: CaseId,
    /// The node.
    pub node: NodeId,
    /// The service principal acted as.
    pub principal: String,
    /// The human or process that authored the version.
    pub author: ProcessAuthor,
    /// The instant the engine stepped at.
    pub at: OffsetDateTime,
}

/// The audit trail of service-principal use.
pub trait ServicePrincipalAudit: Send + Sync {
    /// Records one use. It carries no field value and no secret.
    ///
    /// # Errors
    /// [`Unavailable`] when the record cannot be written; the node then does not act.
    fn record(&self, usage: &ServicePrincipalUse) -> Result<(), Unavailable>;
}

/// The `principal` a Document node config names, when it is a string.
#[must_use]
pub(crate) fn declared_principal(config: &Json) -> Option<&str> {
    config.get("principal").and_then(Json::as_str)
}

/// Whether `name` could be a service principal at all: a storage name that is not a user id.
fn is_name_shaped(name: &str) -> bool {
    parse_record_id(name).is_none() && Name::new(name).is_ok()
}

/// The run-time policy a [`DocumentNodeHandler`](crate::DocumentNodeHandler) enforces for a
/// node that names a `principal`.
#[derive(Clone)]
pub struct ServicePrincipalPolicy {
    tenant: SiteId,
    registry: Arc<dyn ServicePrincipals>,
    authors: Arc<dyn CaseAuthors>,
    audit: Arc<dyn ServicePrincipalAudit>,
}

impl ServicePrincipalPolicy {
    /// A policy for `tenant` over its registry, author lookup and audit trail.
    #[must_use]
    pub fn new(
        tenant: SiteId,
        registry: Arc<dyn ServicePrincipals>,
        authors: Arc<dyn CaseAuthors>,
        audit: Arc<dyn ServicePrincipalAudit>,
    ) -> Self {
        Self {
            tenant,
            registry,
            authors,
            audit,
        }
    }

    /// Checks the node may act as `principal` and records the use, or refuses.
    pub(crate) fn authorise(
        policy: Option<&Self>,
        principal: &str,
        case: &CaseId,
        node: &NodeId,
        now: OffsetDateTime,
    ) -> Result<(), DocumentFailure> {
        let refuse = |code: &str, reason: &str| Err(DocumentFailure::new(code, reason));
        // A plain user id (or any text that is not a name) is never a service principal, and
        // without a policy no node may act as one.
        let (Some(policy), true) = (policy, is_name_shaped(principal)) else {
            return refuse(
                codes::PRINCIPAL_NOT_REGISTERED,
                "the node's principal is not a registered service principal",
            );
        };
        if !matches!(
            policy.registry.is_registered(&policy.tenant, principal),
            Ok(true)
        ) {
            return refuse(
                codes::PRINCIPAL_NOT_REGISTERED,
                "the node's principal is not a registered service principal",
            );
        }
        let Ok(Some(author)) = policy.authors.author_of(&policy.tenant, case) else {
            return refuse(
                codes::PRINCIPAL_NOT_PERMITTED,
                "the author of the running version could not be established",
            );
        };
        if author != ProcessAuthor::Platform
            && !matches!(
                policy.registry.may_use(&policy.tenant, &author, principal),
                Ok(true)
            )
        {
            return refuse(
                codes::PRINCIPAL_NOT_PERMITTED,
                "the author of the version may not act as this service principal",
            );
        }
        let usage = ServicePrincipalUse {
            tenant: policy.tenant.clone(),
            case: case.clone(),
            node: node.clone(),
            principal: principal.to_owned(),
            author,
            at: now,
        };
        if policy.audit.record(&usage).is_err() {
            return refuse(
                codes::PRINCIPAL_AUDIT_FAILED,
                "the use of the service principal could not be recorded, so the node did not act",
            );
        }
        Ok(())
    }
}

/// A node of a process version, as the publish guard sees it.
#[derive(Clone, Copy, Debug)]
pub struct GuardedNode<'a> {
    /// The node.
    pub node: &'a NodeId,
    /// Its execution kind.
    pub kind: &'a NodeKind,
    /// Its compiled config.
    pub config: &'a Json,
}

/// One reason a version may not be published.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishViolation {
    /// The offending node.
    pub node: NodeId,
    /// The stable `bpm_documents.*` code.
    pub code: &'static str,
    /// A plain-language reason that names no secret.
    pub reason: String,
}

/// The check run when a process version is validated or compiled for publishing.
pub struct PublishGuard {
    registry: Arc<dyn ServicePrincipals>,
    permissions: Arc<dyn PublishPermissions>,
}

impl PublishGuard {
    /// A guard over a registry and the platform permission lookup.
    #[must_use]
    pub fn new(
        registry: Arc<dyn ServicePrincipals>,
        permissions: Arc<dyn PublishPermissions>,
    ) -> Self {
        Self {
            registry,
            permissions,
        }
    }

    /// Checks a version authored by `author` for `tenant`.
    ///
    /// # Errors
    /// Every [`PublishViolation`] found, in node order.
    pub fn check(
        &self,
        tenant: &SiteId,
        author: &ProcessAuthor,
        nodes: &[GuardedNode<'_>],
    ) -> Result<(), Vec<PublishViolation>> {
        let mut violations = Vec::new();
        let mut may_publish: Option<bool> = None;
        for entry in nodes {
            if *entry.kind != NodeKind::Document {
                continue;
            }
            let Some(principal) = declared_principal(entry.config) else {
                continue;
            };
            let mut flag = |code: &'static str, reason: &str| {
                violations.push(PublishViolation {
                    node: entry.node.clone(),
                    code,
                    reason: reason.to_owned(),
                });
            };
            let registered = is_name_shaped(principal)
                && matches!(self.registry.is_registered(tenant, principal), Ok(true));
            if !registered {
                flag(
                    codes::PRINCIPAL_NOT_REGISTERED,
                    "the node's principal is not a registered service principal",
                );
                continue;
            }
            if *author == ProcessAuthor::Platform {
                continue;
            }
            let holds = *may_publish.get_or_insert_with(|| {
                matches!(
                    self.permissions.holds(tenant, author, PUBLISH_PERMISSION),
                    Ok(true)
                )
            });
            if !holds {
                flag(
                    codes::PRINCIPAL_NOT_PERMITTED,
                    "the author lacks the permission to publish a process that acts as a service principal",
                );
            }
            if !matches!(self.registry.may_use(tenant, author, principal), Ok(true)) {
                flag(
                    codes::PRINCIPAL_NOT_PERMITTED,
                    "the author may not use this service principal",
                );
            }
        }
        if violations.is_empty() {
            Ok(())
        } else {
            Err(violations)
        }
    }
}

#[derive(Default)]
struct DirectoryState {
    registered: BTreeSet<(SiteId, String)>,
    grants: BTreeSet<(SiteId, String, ProcessAuthor)>,
    permissions: BTreeSet<(SiteId, ProcessAuthor, String)>,
    cases: BTreeMap<(SiteId, CaseId), ProcessAuthor>,
    unreadable: bool,
}

/// An in-memory registry, permission lookup and author lookup, for tests and for hosts without
/// an IAM service. Setting [`MemoryDirectory::set_unreadable`] makes every lookup fail.
#[derive(Default)]
pub struct MemoryDirectory {
    state: Mutex<DirectoryState>,
}

impl MemoryDirectory {
    /// An empty directory.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, DirectoryState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Registers a service principal.
    pub fn register(&self, tenant: &SiteId, name: &str) {
        self.state()
            .registered
            .insert((tenant.clone(), name.to_owned()));
    }

    /// Lets `author` use the registered principal `name`.
    pub fn grant(&self, tenant: &SiteId, name: &str, author: &ProcessAuthor) {
        self.state()
            .grants
            .insert((tenant.clone(), name.to_owned(), author.clone()));
    }

    /// Gives `author` a platform permission.
    pub fn allow_permission(&self, tenant: &SiteId, author: &ProcessAuthor, permission: &str) {
        self.state()
            .permissions
            .insert((tenant.clone(), author.clone(), permission.to_owned()));
    }

    /// Records who authored the version `case` runs.
    pub fn set_case_author(&self, tenant: &SiteId, case: &CaseId, author: &ProcessAuthor) {
        self.state()
            .cases
            .insert((tenant.clone(), case.clone()), author.clone());
    }

    /// Makes every lookup fail (or work again).
    pub fn set_unreadable(&self, unreadable: bool) {
        self.state().unreadable = unreadable;
    }
}

impl ServicePrincipals for MemoryDirectory {
    fn is_registered(&self, tenant: &SiteId, name: &str) -> Result<bool, Unavailable> {
        let state = self.state();
        if state.unreadable {
            return Err(Unavailable);
        }
        Ok(state
            .registered
            .contains(&(tenant.clone(), name.to_owned())))
    }

    fn may_use(
        &self,
        tenant: &SiteId,
        process_author: &ProcessAuthor,
        name: &str,
    ) -> Result<bool, Unavailable> {
        let state = self.state();
        if state.unreadable {
            return Err(Unavailable);
        }
        Ok(state
            .grants
            .contains(&(tenant.clone(), name.to_owned(), process_author.clone())))
    }
}

impl PublishPermissions for MemoryDirectory {
    fn holds(
        &self,
        tenant: &SiteId,
        author: &ProcessAuthor,
        permission: &str,
    ) -> Result<bool, Unavailable> {
        let state = self.state();
        if state.unreadable {
            return Err(Unavailable);
        }
        Ok(state
            .permissions
            .contains(&(tenant.clone(), author.clone(), permission.to_owned())))
    }
}

impl CaseAuthors for MemoryDirectory {
    fn author_of(
        &self,
        tenant: &SiteId,
        case: &CaseId,
    ) -> Result<Option<ProcessAuthor>, Unavailable> {
        let state = self.state();
        if state.unreadable {
            return Err(Unavailable);
        }
        Ok(state.cases.get(&(tenant.clone(), case.clone())).cloned())
    }
}

/// An in-memory [`ServicePrincipalAudit`], for tests and for hosts without an audit chain.
#[derive(Default)]
pub struct MemoryUseAudit {
    records: Mutex<Vec<ServicePrincipalUse>>,
    failing: Mutex<bool>,
}

impl MemoryUseAudit {
    /// An empty trail.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The uses recorded so far.
    #[must_use]
    pub fn records(&self) -> Vec<ServicePrincipalUse> {
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Makes every write fail (or work again).
    pub fn set_failing(&self, failing: bool) {
        *self
            .failing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = failing;
    }
}

impl ServicePrincipalAudit for MemoryUseAudit {
    fn record(&self, usage: &ServicePrincipalUse) -> Result<(), Unavailable> {
        let failing = *self
            .failing
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if failing {
            return Err(Unavailable);
        }
        self.records
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(usage.clone());
        Ok(())
    }
}
