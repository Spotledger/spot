//! The IAM-backed service-principal seams (WP-BPM-04.T4) and the publish hook.
//!
//! [`ServicePrincipals`] and [`PublishPermissions`] are ports; this module implements them over
//! two injected ports of its own, so the crate holds no storage access:
//!
//! - [`ServicePrincipalDirectory`]: the tenant's service-principal entries in the IAM directory,
//!   each with its grant list. The registry is read through IAM; BPM owns none of it.
//! - [`AuthorPrincipals`]: the IAM identity (a permission [`Principal`]) of a process author.
//!
//! [`IamServicePrincipals`] answers `is_registered` and `may_use` from the directory entries.
//! [`IamPublishPermission`] answers whether an author holds
//! [`PUBLISH_PERMISSION`](crate::service_principal::PUBLISH_PERMISSION) through the
//! [`PermissionEngine`]. Every read failure, unknown author, other tenant or unrecognised answer
//! is `false` (fail closed); the traits' `Err(Unavailable)` is used only when a port reports it.
//!
//! [`validate_process_version`] is the single hook the publish path calls.

use std::sync::Arc;

use spot_app_api::prelude::SiteId;
use spot_dag::CompiledDag;
use spot_permissions::{PermissionEngine, Principal};

use crate::service_principal::{
    GuardedNode, ProcessAuthor, PublishGuard, PublishPermissions, PublishViolation,
    ServicePrincipals, Unavailable,
};

/// One service principal of a tenant, as the IAM directory holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServicePrincipalEntry {
    /// The principal's name, compared exactly.
    pub name: String,
    /// The authors whose process versions may act as this principal.
    pub grants: Vec<ProcessAuthor>,
}

/// The tenant's service-principal entries in the IAM directory.
pub trait ServicePrincipalDirectory: Send + Sync {
    /// Every service-principal entry of `tenant`.
    ///
    /// # Errors
    /// [`Unavailable`] when the directory cannot be read.
    fn service_principals(
        &self,
        tenant: &SiteId,
    ) -> Result<Vec<ServicePrincipalEntry>, Unavailable>;
}

/// The IAM identity of a process author.
pub trait AuthorPrincipals: Send + Sync {
    /// The permission principal of `author` in `tenant`, or `None` when IAM does not know it.
    ///
    /// # Errors
    /// [`Unavailable`] when IAM cannot be read.
    fn principal_of(
        &self,
        tenant: &SiteId,
        author: &ProcessAuthor,
    ) -> Result<Option<Principal>, Unavailable>;
}

/// [`ServicePrincipals`] over the IAM directory.
pub struct IamServicePrincipals {
    directory: Arc<dyn ServicePrincipalDirectory>,
}

impl IamServicePrincipals {
    /// A registry reading `directory`.
    #[must_use]
    pub fn new(directory: Arc<dyn ServicePrincipalDirectory>) -> Self {
        Self { directory }
    }

    fn entry(
        &self,
        tenant: &SiteId,
        name: &str,
    ) -> Result<Option<ServicePrincipalEntry>, Unavailable> {
        Ok(self
            .directory
            .service_principals(tenant)?
            .into_iter()
            .find(|entry| entry.name == name))
    }
}

impl ServicePrincipals for IamServicePrincipals {
    fn is_registered(&self, tenant: &SiteId, name: &str) -> Result<bool, Unavailable> {
        Ok(self.entry(tenant, name)?.is_some())
    }

    fn may_use(
        &self,
        tenant: &SiteId,
        process_author: &ProcessAuthor,
        name: &str,
    ) -> Result<bool, Unavailable> {
        Ok(self
            .entry(tenant, name)?
            .is_some_and(|entry| entry.grants.contains(process_author)))
    }
}

/// [`PublishPermissions`] over the permission engine of one tenant.
pub struct IamPublishPermission {
    tenant: SiteId,
    engine: Arc<PermissionEngine>,
    authors: Arc<dyn AuthorPrincipals>,
}

impl IamPublishPermission {
    /// A lookup for `tenant`, whose permission snapshot `engine` holds.
    #[must_use]
    pub fn new(
        tenant: SiteId,
        engine: Arc<PermissionEngine>,
        authors: Arc<dyn AuthorPrincipals>,
    ) -> Self {
        Self {
            tenant,
            engine,
            authors,
        }
    }
}

impl PublishPermissions for IamPublishPermission {
    fn holds(
        &self,
        tenant: &SiteId,
        author: &ProcessAuthor,
        permission: &str,
    ) -> Result<bool, Unavailable> {
        // An engine answers for one tenant only; another tenant's question is refused.
        if *tenant != self.tenant {
            return Ok(false);
        }
        if *author == ProcessAuthor::Platform {
            return Ok(true);
        }
        let Some(principal) = self.authors.principal_of(tenant, author)? else {
            return Ok(false);
        };
        // A "system" principal from the resolver is not an author: only a named user counts.
        if principal.is_system || principal.user.is_empty() {
            return Ok(false);
        }
        Ok(self
            .engine
            .app_permission(&principal, &engine_permission_name(permission))
            .allowed)
    }
}

/// The name the permission engine knows a platform permission by.
///
/// The engine's named-permission grammar is lowercase letters, digits and `_` (no dot), so
/// `process.publish_service_principal` is asked as `process_publish_service_principal`. The
/// permission catalogue must declare that name; until it does, the engine answers "unknown" and
/// the lookup fails closed.
#[must_use]
pub fn engine_permission_name(permission: &str) -> String {
    permission.replace('.', "_")
}

/// Runs the [`PublishGuard`] over every Document node of a compiled process version and returns
/// every violation (empty means the version may be published).
///
/// A node is a Document node when its DAG kind is `document`; its `principal` is read from the
/// node config. The publish path calls this once per version, before it registers it.
#[must_use]
pub fn validate_process_version(
    guard: &PublishGuard,
    tenant: &SiteId,
    author: &ProcessAuthor,
    dag: &CompiledDag,
) -> Vec<PublishViolation> {
    let kind = spot_bpm_engine::NodeKind::Document;
    let nodes: Vec<GuardedNode<'_>> = dag
        .nodes()
        .iter()
        .filter(|node| node.kind == "document")
        .map(|node| GuardedNode {
            node: &node.id,
            kind: &kind,
            config: &node.config,
        })
        .collect();
    guard
        .check(tenant, author, &nodes)
        .err()
        .unwrap_or_default()
}
