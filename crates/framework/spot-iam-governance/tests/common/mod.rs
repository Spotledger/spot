//! Shared fixtures for the governance tests: the seeded SoD rules and role catalogue, the
//! scope hierarchy, and builders for assignments and grant requests.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_iam_governance::{GrantRequest, GrantRequestId, GrantScope, Grantor, ScopeHierarchy};
use spot_iam_roles::{Assignment, RoleName, Source};
use spot_sod::{RoleCatalogue, RuleSet};

/// The seeded segregation-of-duties rules, in our own words (ADR-070 section 8).
pub const RULES: &str = r#"
[[rules]]
id = "sod.vendor-master.payments"
title = "Maintain suppliers vs approve payments"
description = "Who maintains the supplier master must not approve the payments to those suppliers."
severity = "critical"
scope = "same_scope"
[[rules.sides]]
duties = ["Maintain supplier master"]
[[rules.sides]]
duties = ["Approve payment run"]

[[rules]]
id = "sod.purchase-order.approval"
title = "Create purchase orders vs approve purchase orders"
description = "Who raises a purchase order must not approve it."
severity = "high"
scope = "same_scope"
[[rules.sides]]
duties = ["Create purchase order"]
[[rules.sides]]
duties = ["Approve purchase order"]

[[rules]]
id = "sod.bank-details.release"
title = "Maintain bank details vs release payments"
description = "Who edits the bank accounts money is sent to must not release the payments."
severity = "critical"
scope = "any_scope"
[[rules.sides]]
duties = ["Maintain bank details"]
[[rules.sides]]
duties = ["Release payment"]

[[rules]]
id = "sod.journal-entry.reconciliation"
title = "Post journal entries vs reconcile ledgers"
description = "Who posts journal entries must not reconcile the ledgers those entries land in."
severity = "medium"
scope = "same_scope"
[[rules.sides]]
duties = ["Post journal entry"]
[[rules.sides]]
duties = ["Reconcile ledger"]
"#;

/// The seeded reference role catalogue.
pub const CATALOGUE: &str = r#"
[[roles]]
name = "Accounts Payable Clerk"
duties = ["Maintain supplier master", "Create purchase order", "Post journal entry"]

[[roles]]
name = "Finance Approver"
duties = ["Approve payment run", "Approve purchase order", "Release payment"]

[[roles]]
name = "Treasury Clerk"
duties = ["Maintain bank details"]

[[roles]]
name = "Ledger Accountant"
duties = ["Reconcile ledger"]
"#;

/// The seeded rule set.
pub fn rules() -> RuleSet {
    RuleSet::from_toml(RULES).expect("the seeded rules load")
}

/// The seeded catalogue.
pub fn catalogue() -> RoleCatalogue {
    RoleCatalogue::from_toml(CATALOGUE).expect("the seeded catalogue loads")
}

/// A hierarchy where `Company A` has the division `Company A/Division 1`.
pub fn hierarchy() -> ScopeHierarchy {
    let mut hierarchy = ScopeHierarchy::new();
    hierarchy.add("Company A", "Company A/Division 1");
    hierarchy
}

/// A manual assignment of `role` to `user`, scoped to `scope` when given.
pub fn assignment(user: &str, role: &str, scope: Option<GrantScope>) -> Assignment {
    let id = format!("{user}-{role}").replace(' ', "-");
    let mut assignment = Assignment::new(&id, user, role);
    assignment.scope = scope.unwrap_or_else(GrantScope::unscoped).to_scope();
    assignment.source = Source::Manual;
    assignment
}

/// A grant request id.
pub fn request_id(text: &str) -> GrantRequestId {
    GrantRequestId::new(text).expect("a non-blank request id")
}

/// A grant request by `grantor` for `user` of `role`, scoped to `scope` when given.
pub fn request(
    id: &str,
    user: &str,
    role: &str,
    scope: Option<GrantScope>,
    grantor: Grantor,
) -> GrantRequest {
    GrantRequest::new(&request_id(id), user, role, grantor)
        .with_scope(scope.unwrap_or_else(GrantScope::unscoped))
}

/// A role name.
pub fn role(name: &str) -> RoleName {
    RoleName::from(name)
}
