//! Copy on customise and the upgrade diff.
#![allow(missing_docs)]

mod common;

use common::registry;
use spot_iam_roles::{
    Conflict, DutyName, DutyRef, Origin, Role, RoleName, Version, copy_role, diff_reference,
    diff_reference_from_base,
};
use spot_permissions::model::{Right, Rights, Role as GrantRole, RoleGrant};

fn reference(version: u32, duties: &[&str]) -> Role {
    Role {
        origin: Origin::Reference {
            pack: "core".into(),
            version: Version(version),
        },
        duties: duties.iter().map(|d| DutyRef::named(d)).collect(),
        ..Role::custom("Core Viewer")
    }
}

fn grant(owner: &str, doctype: &str, rights: &[Right]) -> RoleGrant {
    RoleGrant {
        doctype: doctype.into(),
        role: GrantRole(owner.into()),
        rights: Rights::new(rights.iter().copied()),
        if_owner: false,
    }
}

#[test]
fn a_copy_records_its_source_and_version_and_is_a_valid_custom_role() {
    let r = registry();
    let source = r
        .role(&RoleName::from("Core Viewer"))
        .cloned()
        .unwrap_or_else(|| panic!("role"));
    let copy = copy_role(&source, "My Viewer");
    assert_eq!(copy.name.as_str(), "My Viewer");
    match &copy.origin {
        Origin::Custom {
            copied_from: Some(c),
        } => {
            assert_eq!(c.role.as_str(), "Core Viewer");
            assert_eq!(c.version, Version(3));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(copy.duties, source.duties);
    // It can be added to the registry, and the source is untouched.
    let next = r.with_role(copy.clone()).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(next.role(&RoleName::from("Core Viewer")), Some(&source));
    // A copy of the copy keeps the lineage.
    let again = copy_role(&copy, "Another");
    assert_eq!(again.origin, copy.origin);
}

#[test]
fn a_copy_rekeys_its_grants_to_the_new_name() {
    let mut source = reference(3, &[]);
    source.direct_grants = vec![grant("Core Viewer", "customer", &[Right::Read])];
    let copy = copy_role(&source, "Mine");
    assert_eq!(copy.direct_grants[0].role.as_str(), "Mine");
}

#[test]
fn the_diff_reports_added_and_removed_duties_and_grants() {
    let mut copy = copy_role(&reference(3, &["view_invoices", "edit_invoices"]), "Mine");
    copy.direct_grants = vec![grant("Mine", "customer", &[Right::Read])];
    let mut new = reference(4, &["view_invoices", "post_invoices"]);
    new.direct_grants = vec![grant("Core Viewer", "sales_invoice", &[Right::Read])];
    let d = diff_reference(&copy, &new);
    assert_eq!(d.duties_added, [DutyName::from("post_invoices")]);
    assert_eq!(d.duties_removed, [DutyName::from("edit_invoices")]);
    assert_eq!(d.grants_added.len(), 1);
    assert_eq!(d.grants_added[0].doctype, "sales_invoice");
    assert_eq!(d.grants_removed[0].doctype, "customer");
    assert!(d.conflicts_with_customisation.is_empty());
    assert!(!d.is_empty());
}

#[test]
fn the_same_item_with_different_content_is_a_conflict() {
    let mut copy = copy_role(&reference(3, &[]), "Mine");
    copy.direct_grants = vec![grant("Mine", "customer", &[Right::Read, Right::Write])];
    copy.duties = vec![DutyRef {
        name: DutyName::from("view_invoices"),
        version: Some(Version(1)),
    }];
    let mut new = reference(4, &[]);
    new.direct_grants = vec![grant("Core Viewer", "customer", &[Right::Read])];
    new.duties = vec![DutyRef {
        name: DutyName::from("view_invoices"),
        version: Some(Version(2)),
    }];
    let d = diff_reference(&copy, &new);
    assert_eq!(
        d.conflicts_with_customisation,
        [
            Conflict::Duty {
                name: DutyName::from("view_invoices")
            },
            Conflict::Grant {
                doctype: "customer".into(),
                if_owner: false
            },
        ]
    );
}

#[test]
fn the_copy_is_never_rewritten() {
    let copy = copy_role(&reference(3, &["view_invoices"]), "Mine");
    let before = copy.clone();
    let _ = diff_reference(&copy, &reference(4, &["post_invoices"]));
    assert_eq!(copy, before);
}

#[test]
fn identical_roles_have_an_empty_diff() {
    let base = reference(3, &["view_invoices"]);
    let copy = copy_role(&base, "Mine");
    assert!(diff_reference(&copy, &base).is_empty());
}

#[test]
fn with_the_base_only_conflicting_changes_are_conflicts() {
    let mut base = reference(3, &[]);
    base.direct_grants = vec![
        grant("Core Viewer", "customer", &[Right::Read]),
        grant("Core Viewer", "sales_invoice", &[Right::Read]),
    ];
    // The tenant widened customer; upstream also changes customer, differently, and drops
    // sales_invoice (which the tenant left alone).
    let mut copy = copy_role(&base, "Mine");
    copy.direct_grants = vec![
        grant("Mine", "customer", &[Right::Read, Right::Write]),
        grant("Mine", "sales_invoice", &[Right::Read]),
    ];
    let mut new = reference(4, &[]);
    new.direct_grants = vec![grant(
        "Core Viewer",
        "customer",
        &[Right::Read, Right::Print],
    )];
    let d = diff_reference_from_base(&base, &copy, &new);
    assert_eq!(d.grants_removed.len(), 1);
    assert_eq!(d.grants_removed[0].doctype, "sales_invoice");
    assert!(d.grants_added.is_empty());
    assert_eq!(
        d.conflicts_with_customisation,
        [Conflict::Grant {
            doctype: "customer".into(),
            if_owner: false
        }]
    );
    // A change upstream that the tenant did not touch is no conflict.
    let mut quiet = copy_role(&base, "Quiet");
    quiet.direct_grants = vec![
        grant("Quiet", "customer", &[Right::Read]),
        grant("Quiet", "sales_invoice", &[Right::Read]),
    ];
    assert!(
        diff_reference_from_base(&base, &quiet, &new)
            .conflicts_with_customisation
            .is_empty()
    );
}
