//! Shared fixtures for the access tests.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_meta::{Definition, FieldSpec, FieldType, Registry};
use spot_permissions::{Model, PermissionEngine, Right, Rights, Role, RoleGrant, Snapshot};

pub const DOCTYPES: [&str; 3] = ["customer", "sales_invoice", "project"];

pub fn definitions() -> Vec<Definition> {
    let mut customer = Definition::new("customer");
    customer.fields = vec![FieldSpec::new("customer_name", FieldType::Text)];
    let mut invoice = Definition::new("sales_invoice");
    invoice.is_posted = true;
    let mut link = FieldSpec::new("customer", FieldType::Link);
    link.options = Some("customer".into());
    invoice.fields = vec![FieldSpec::new("number", FieldType::Text), link];
    let mut project = Definition::new("project");
    let mut plink = FieldSpec::new("customer", FieldType::Link);
    plink.options = Some("customer".into());
    project.fields = vec![FieldSpec::new("project_name", FieldType::Text), plink];
    vec![customer, invoice, project]
}

pub fn grant(doctype: &str, role: &str, rights: &[Right], if_owner: bool) -> RoleGrant {
    RoleGrant {
        doctype: doctype.to_owned(),
        role: Role(role.to_owned()),
        rights: Rights::new(rights.iter().copied()),
        if_owner,
    }
}

/// Roles: `Reader` reads everything; `Clerk` writes customers and projects; `Poster` posts and
/// cancels invoices; `Own` can only update projects it created; `Weak` reads customers only.
pub fn grants() -> Vec<RoleGrant> {
    let mut g = Vec::new();
    for d in DOCTYPES {
        g.push(grant(d, "Reader", &[Right::Read], false));
    }
    for d in ["customer", "project"] {
        g.push(grant(
            d,
            "Clerk",
            &[Right::Read, Right::Write, Right::Create, Right::Delete],
            false,
        ));
    }
    g.push(grant(
        "sales_invoice",
        "Poster",
        &[Right::Read, Right::Submit, Right::Cancel, Right::Create],
        false,
    ));
    g.push(grant("project", "Own", &[Right::Write], true));
    g.push(grant("customer", "Weak", &[Right::Read], false));
    g
}

pub fn engine_with(grants: Vec<RoleGrant>) -> PermissionEngine {
    let meta = Registry::new(definitions()).unwrap().effective();
    let model = Model {
        grants,
        ..Model::default()
    };
    PermissionEngine::new(Arc::new(Snapshot::build(&meta, &model).unwrap()))
}

pub fn engine() -> PermissionEngine {
    engine_with(grants())
}
