//! Effective rights, provenance and determinism.
#![allow(missing_docs)]

mod common;

use common::registry;
use spot_iam_roles::{Registry, RoleName, effective};
use spot_permissions::model::Right;

fn rights(role: &str, doctype: &str) -> Vec<Right> {
    let r = registry();
    let role = r
        .role(&RoleName::from(role))
        .cloned()
        .unwrap_or_else(|| panic!("role"));
    effective(&role, &r)
        .grants
        .iter()
        .find(|g| g.grant.doctype == doctype && !g.grant.if_owner)
        .map(|g| g.grant.rights.as_slice().to_vec())
        .unwrap_or_default()
}

#[test]
fn inheritance_and_duties_are_flattened() {
    assert_eq!(rights("Accounting Viewer", "sales_invoice"), [Right::Read]);
    assert_eq!(
        rights("Accounting Manager", "sales_invoice"),
        [Right::Read, Right::Write, Right::Create, Right::Submit]
    );
    assert_eq!(
        rights("Senior Accountant", "sales_invoice"),
        [
            Right::Read,
            Right::Write,
            Right::Create,
            Right::Submit,
            Right::Cancel
        ]
    );
    // Inherited from the viewer two levels up.
    assert_eq!(rights("Senior Accountant", "customer"), [Right::Read]);
    assert!(rights("Clerk", "customer").is_empty());
}

#[test]
fn grants_are_rekeyed_to_the_resolved_role() {
    let r = registry();
    let senior = r
        .role(&RoleName::from("Senior Accountant"))
        .cloned()
        .unwrap_or_else(|| panic!("role"));
    let resolved = effective(&senior, &r);
    assert!(
        resolved
            .grants
            .iter()
            .all(|g| g.grant.role.as_str() == "Senior Accountant")
    );
}

#[test]
fn every_right_names_the_role_path_and_duty_that_granted_it() {
    let r = registry();
    let senior = r
        .role(&RoleName::from("Senior Accountant"))
        .cloned()
        .unwrap_or_else(|| panic!("role"));
    let resolved = effective(&senior, &r);
    let sources = resolved.sources_of("customer", false);
    assert_eq!(sources.len(), 1);
    let path: Vec<&str> = sources[0].role_path.iter().map(RoleName::as_str).collect();
    assert_eq!(
        path,
        [
            "Senior Accountant",
            "Accounting Manager",
            "Accounting Viewer"
        ]
    );
    assert_eq!(
        sources[0].duty.as_ref().map(|d| d.as_str()),
        Some("view_invoices")
    );
    // `read` on invoices comes from several places: kept, in discovery order, without repeats.
    let invoice = resolved.sources_of("sales_invoice", false);
    assert!(invoice.len() >= 4, "{invoice:?}");
    let mut unique = invoice.to_vec();
    unique.dedup();
    assert_eq!(unique.len(), invoice.len());
}

#[test]
fn a_direct_grant_has_no_duty_and_if_owner_grants_stay_separate() {
    let r = registry();
    let own = r
        .role(&RoleName::from("Own Invoices"))
        .cloned()
        .unwrap_or_else(|| panic!("role"));
    let resolved = effective(&own, &r);
    assert_eq!(resolved.grants.len(), 1);
    assert!(resolved.grants[0].grant.if_owner);
    assert_eq!(resolved.grants[0].sources[0].duty, None);
}

#[test]
fn a_field_deny_in_the_closure_stays_a_deny() {
    let r = registry();
    let role = r
        .role(&RoleName::from("Redacted Viewer"))
        .cloned()
        .unwrap_or_else(|| panic!("role"));
    let resolved = effective(&role, &r);
    assert_eq!(resolved.field_grants.len(), 1);
    assert!(resolved.field_grants[0].grant.deny);
    assert_eq!(
        resolved.field_grants[0].grant.role.as_str(),
        "Redacted Viewer"
    );
    // The viewer rights still come through next to the deny.
    assert!(!resolved.grants.is_empty());
}

#[test]
fn a_role_reached_twice_counts_once() {
    let text = r#"
[[role]]
name = "base"
origin = { kind = "custom" }
[[role.direct_grants]]
doctype = "customer"
role = "base"
rights = ["read"]
[[role]]
name = "left"
origin = { kind = "custom" }
parents = ["base"]
[[role]]
name = "right"
origin = { kind = "custom" }
parents = ["base"]
[[role]]
name = "top"
origin = { kind = "custom" }
parents = ["left", "right"]
"#;
    let r = Registry::from_toml(text).unwrap_or_else(|e| panic!("{e}"));
    let top = r
        .role(&RoleName::from("top"))
        .cloned()
        .unwrap_or_else(|| panic!("role"));
    let resolved = effective(&top, &r);
    assert_eq!(resolved.grants.len(), 1);
    assert_eq!(resolved.grants[0].sources.len(), 1);
}

#[test]
fn the_result_does_not_depend_on_the_order_things_are_written() {
    let r = registry();
    let mut duties: Vec<_> = r.duties().cloned().collect();
    let mut roles: Vec<_> = r.roles().cloned().collect();
    duties.reverse();
    roles.reverse();
    // Grants inside each duty and role in reverse as well.
    for d in &mut duties {
        d.grants.reverse();
    }
    for role in &mut roles {
        role.direct_grants.reverse();
    }
    let shuffled = Registry::new(duties, roles).unwrap_or_else(|e| panic!("{e}"));
    for role in r.roles() {
        let a = effective(role, &r);
        let other = shuffled
            .role(&role.name)
            .cloned()
            .unwrap_or_else(|| panic!("role"));
        let b = effective(&other, &shuffled);
        let grants = |x: &spot_iam_roles::ResolvedRole| {
            x.grants.iter().map(|g| g.grant.clone()).collect::<Vec<_>>()
        };
        assert_eq!(grants(&a), grants(&b), "{}", role.name.as_str());
    }
}
