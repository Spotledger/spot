//! The `link.manage` right.
//!
//! Every administration operation needs it, mutating or not: a link's grants and field lists are
//! exactly what a tenant may not want every user to read. `spot-permissions` rights are bits on a
//! DocType, so `link.manage` is expressed on the DocType [`LINK_DOCTYPE`]: the mutating
//! operations need the `update` right, the reading ones need `read`. A principal that holds the
//! `link.manage` role in the tenant's permission model holds both. [`EnginePermissions`] asks the
//! tenant's [`PermissionEngine`]; a snapshot that does not know the DocType denies every
//! non-system principal, so a tenant that has not defined the right has no link administrators.

use spot_events_contract::lifecycle::Operation as DocOperation;
use spot_permissions::{Action, DocFacts, PermissionEngine, Principal};
use spot_tenant_link::LinkId;

/// The DocType the `link.manage` right is defined on.
pub const LINK_DOCTYPE: &str = "tenant_link";

/// The right every operation needs, `link.manage`, as the name the permission model and the
/// audit use. It is built from its two path segments, like every management operation id.
#[must_use]
pub fn manage_right() -> String {
    format!("{}.{}", "link", "manage")
}

/// One administration operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Operation {
    /// Start a link.
    Propose,
    /// Accept a proposal.
    Accept,
    /// Reject a proposal or a link still in handshake.
    Reject,
    /// Change the grant of a topic.
    Grant,
    /// Revoke a link.
    Revoke,
    /// List the tenant's links.
    List,
    /// Show one link.
    Show,
    /// Show a link's audit references.
    ShowAudit,
}

impl Operation {
    /// The last segment of the operation id: `propose`, `accept`, `reject`, `grant`, `revoke`,
    /// `list`, `show` or `audit`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Propose => "propose",
            Self::Accept => "accept",
            Self::Reject => "reject",
            Self::Grant => "grant",
            Self::Revoke => "revoke",
            Self::List => "list",
            Self::Show => "show",
            Self::ShowAudit => "audit",
        }
    }

    /// The management-API operation id: `link.` and the [`name`](Self::name).
    #[must_use]
    pub fn id(self) -> String {
        format!("link.{}", self.name())
    }

    /// Whether the operation changes a link.
    #[must_use]
    pub fn mutates(self) -> bool {
        !matches!(self, Self::List | Self::Show | Self::ShowAudit)
    }

    /// Every operation, in a fixed order.
    #[must_use]
    pub fn all() -> [Self; 8] {
        [
            Self::Propose,
            Self::Accept,
            Self::Reject,
            Self::Grant,
            Self::Revoke,
            Self::List,
            Self::Show,
            Self::ShowAudit,
        ]
    }
}

/// Decides whether a principal holds `link.manage` for an operation.
pub trait LinkPermissions: Send + Sync {
    /// Whether `principal` may run `operation`, on `link` when the operation names one.
    fn may(&self, principal: &Principal, operation: Operation, link: Option<&LinkId>) -> bool;
}

/// The tenant's permission engine as the `link.manage` check.
pub struct EnginePermissions {
    engine: PermissionEngine,
}

impl EnginePermissions {
    /// The check over `engine`.
    #[must_use]
    pub fn new(engine: PermissionEngine) -> Self {
        Self { engine }
    }
}

impl LinkPermissions for EnginePermissions {
    fn may(&self, principal: &Principal, operation: Operation, link: Option<&LinkId>) -> bool {
        let action = if operation.mutates() {
            Action::Op(DocOperation::Update)
        } else {
            Action::Read
        };
        let doc = DocFacts {
            doctype: LINK_DOCTYPE.to_owned(),
            id: link.map(ToString::to_string),
            ..DocFacts::default()
        };
        self.engine.can(principal, action, &doc).allowed
    }
}

/// Denies everyone, the system included: the safe value for a host that has not wired the
/// permission model.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAll;

impl LinkPermissions for DenyAll {
    fn may(&self, _principal: &Principal, _operation: Operation, _link: Option<&LinkId>) -> bool {
        false
    }
}
