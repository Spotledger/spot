//! The platform app builds under spot-meta, compiles to a catalogue storage accepts, keeps every
//! credential closed, and its grants load into a permission snapshot.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{collation, engine, meta};
use spot_meta::{Customisations, FieldType, Registry};
use spot_permissions::model::{FieldFacts, GroupName, Principal};
use spot_permissions::{DocFacts, FieldLevel};
use spot_platform_app::{PLATFORM_APP, definitions, doctype, group, permissions, role};
use spot_storage_contract::StorageBackend;
use spot_storage_fake::FakeBackend;

#[test]
fn every_embedded_file_parses_and_names_its_doctype() {
    let defs = definitions().unwrap();
    let names: Vec<_> = defs.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, doctype::ALL);
    for d in &defs {
        assert_eq!(d.app.as_deref(), Some(PLATFORM_APP), "{}", d.name);
    }
}

#[test]
fn the_manifest_names_the_platform_app() {
    let manifest = spot_manifest::parse(spot_platform_app::MANIFEST, "apps/platform/app.toml")
        .unwrap_or_else(|e| panic!("{e:?}"));
    assert_eq!(manifest.app.name.as_str(), PLATFORM_APP);
}

#[test]
fn the_app_builds_with_both_lints_green() {
    // `Registry::build` runs `meta.derived_unprotected` and `meta.tag_unprotected`; any error
    // fails the build.
    let m = meta();
    assert_eq!(m.doctypes().count(), doctype::ALL.len());
}

#[test]
fn the_tag_lint_is_live_over_this_app() {
    let mut defs = definitions().unwrap();
    let attempt = defs
        .iter_mut()
        .find(|d| d.name == doctype::LOGIN_ATTEMPT)
        .unwrap();
    let field = attempt
        .fields
        .iter_mut()
        .find(|f| f.name == "source_digest")
        .unwrap();
    field.protection_group = None;
    let errors = Registry::build(defs, &Customisations::new()).unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("meta.tag_unprotected")),
        "{errors:?}"
    );
}

#[tokio::test]
async fn the_catalogue_compiles_and_storage_prepares_it() {
    let m = meta();
    let catalog = m.catalog(collation()).unwrap();
    assert_eq!(m.catalog(collation()).unwrap(), catalog, "deterministic");
    FakeBackend::new().prepare(&catalog).await.unwrap();
}

#[test]
fn every_credential_field_is_a_secret_in_the_closed_credential_group() {
    let defs = definitions().unwrap();
    let mut credentials = Vec::new();
    for d in &defs {
        for f in &d.fields {
            // The webhook's sealed signing secret has its own closed group (WP-INT-01.T3);
            // every other secret is a credential.
            let closed = if d.name == doctype::WEBHOOK {
                group::WEBHOOK_SECRET
            } else {
                group::CREDENTIAL
            };
            let in_group = f.protection_group.as_deref() == Some(closed);
            let secret = f.ty == FieldType::Secret;
            let tagged = f.classification.iter().any(|t| t == "secret");
            // Named, documented exception (WP-INT-01.T7): `webhook.headers` is a `json` field
            // (a list of {name, value, is_secret}) kept in the webhook's closed `secret` group
            // because a plain header value can carry an `Authorization` or API-key token. It
            // is not a `secret`-typed credential, so it is neither counted as one nor tagged.
            let json_in_closed_group = d.name == doctype::WEBHOOK && f.name == "headers";
            if json_in_closed_group {
                assert!(in_group && f.ty == FieldType::Json, "webhook.headers");
                assert!(!tagged, "webhook.headers is not a secret-typed credential");
            } else {
                assert_eq!(
                    in_group, secret,
                    "{}.{}: a secret belongs in `credential`, and only a secret does",
                    d.name, f.name
                );
                assert_eq!(secret, tagged, "{}.{}: tagged `secret`", d.name, f.name);
            }
            if secret {
                assert!(
                    f.masking.is_none() && f.declassified.is_none(),
                    "{}.{}: never masked, never declassified",
                    d.name,
                    f.name
                );
                credentials.push(format!("{}.{}", d.name, f.name));
            }
        }
        if d.fields.iter().any(|f| f.ty == FieldType::Secret) {
            assert!(
                d.protection_groups.iter().any(|g| {
                    g.name == group::CREDENTIAL
                        || (d.name == doctype::WEBHOOK && g.name == group::WEBHOOK_SECRET)
                }),
                "{} must declare the group",
                d.name
            );
        }
    }
    credentials.sort();
    assert_eq!(
        credentials,
        [
            "api_key.digest",
            "session.token_digest",
            "user.password_hash",
            "user.totp_secret",
            "user_recovery_digest.digest",
            "webhook.secret",
        ]
    );
}

#[test]
fn credential_fields_are_in_the_effective_protection_data() {
    let m = meta();
    let protected: Vec<_> = m
        .field_protection()
        .into_iter()
        .filter(|p| p.groups.iter().any(|g| g == group::CREDENTIAL))
        .collect();
    assert_eq!(protected.len(), 5);
    for p in &protected {
        assert!(
            p.trace_groups.iter().any(|g| g == group::CREDENTIAL),
            "{}",
            p.path
        );
        assert!(p.mask.is_none(), "{}", p.path);
    }
}

#[test]
fn the_shipped_grants_load_into_a_snapshot() {
    let m = meta();
    let model = permissions().unwrap();
    assert!(!model.grants.is_empty());
    assert!(
        model
            .groups
            .iter()
            .any(|g| g.name.as_str() == group::CREDENTIAL && g.trace)
    );
    // No field grant touches the credential group.
    assert!(model.field_grants.iter().all(|g| {
        g.target != spot_permissions::FieldTarget::Group(GroupName(group::CREDENTIAL.to_owned()))
    }));
    common::snapshot(&m);
}

#[test]
fn only_the_system_principal_reads_a_credential() {
    let m = meta();
    let engine = engine(&m);
    let model = permissions().unwrap();
    // Every role that has a shipped grant, plus roles that have none.
    let mut roles: Vec<String> = model
        .grants
        .iter()
        .map(|g| g.role.as_str().to_owned())
        .collect();
    roles.extend(["Nobody".to_owned(), "Auditor".to_owned()]);
    roles.sort();
    roles.dedup();
    assert!(roles.contains(&role::SYSTEM_MANAGER.to_owned()));
    assert!(roles.contains(&role::USER_MANAGER.to_owned()));

    let mut checked = 0;
    for p in m
        .field_protection()
        .into_iter()
        .filter(|p| p.groups.iter().any(|g| g == group::CREDENTIAL))
    {
        let (dt, field) = p.path.split_once('.').unwrap();
        let facts = FieldFacts {
            path: field.to_owned(),
            groups: vec![GroupName(group::CREDENTIAL.to_owned())],
            ..FieldFacts::default()
        };
        let doc = DocFacts {
            doctype: dt.to_owned(),
            ..DocFacts::default()
        };
        for r in &roles {
            let who = Principal::with_roles("someone", &[r.as_str()]);
            let d = engine.field_access(&who, dt, &facts, &doc);
            assert_eq!(d.level, FieldLevel::Hidden, "{r} on {}", p.path);
            assert!(
                !d.can_create && !d.can_update && !d.can_report && !d.can_reveal,
                "{r} on {}",
                p.path
            );
            checked += 1;
        }
        let d = engine.field_access(&Principal::system(), dt, &facts, &doc);
        assert_eq!(d.level, FieldLevel::ReadReveal, "system on {}", p.path);
    }
    assert_eq!(checked, 5 * roles.len());
}

#[test]
fn managers_read_but_never_write_sessions_keys_and_attempts() {
    use spot_permissions::Right;
    use spot_permissions::{Action, DocFacts};
    let model = permissions().unwrap();
    for dt in [doctype::SESSION, doctype::API_KEY, doctype::LOGIN_ATTEMPT] {
        for r in [role::SYSTEM_MANAGER, role::USER_MANAGER] {
            for g in model
                .grants
                .iter()
                .filter(|g| g.doctype == dt && g.role.as_str() == r)
            {
                for forbidden in [Right::Write, Right::Create, Right::Delete] {
                    assert!(!g.rights.contains(forbidden), "{r} on {dt}");
                }
            }
        }
    }
    let m = meta();
    let engine = engine(&m);
    let who = Principal::with_roles("sam", &[role::SYSTEM_MANAGER]);
    let doc = DocFacts {
        doctype: doctype::SESSION.to_owned(),
        ..DocFacts::default()
    };
    assert!(engine.can(&who, Action::Read, &doc).allowed);
}

#[test]
fn every_doctype_has_a_system_manager_grant_and_custom_doctype_is_never_deleted() {
    use spot_permissions::Right;
    let model = permissions().unwrap();
    for dt in doctype::ALL {
        assert!(
            model
                .grants
                .iter()
                .any(|g| g.doctype == dt && g.role.as_str() == role::SYSTEM_MANAGER),
            "no System Manager grant on {dt}"
        );
    }
    for dt in [
        doctype::CUSTOM_DOCTYPE,
        doctype::CUSTOM_FIELD,
        doctype::PROPERTY_SETTER,
    ] {
        for g in model.grants.iter().filter(|g| g.doctype == dt) {
            assert_eq!(
                g.role.as_str(),
                role::SYSTEM_MANAGER,
                "only System Manager on {dt}"
            );
            assert!(!g.rights.contains(Right::Delete), "no delete on {dt}");
        }
    }
}

#[test]
fn system_manager_reads_and_creates_a_custom_field_and_a_plain_user_cannot() {
    use spot_permissions::{Action, DocFacts};
    let m = meta();
    let engine = engine(&m);
    let doc = DocFacts {
        doctype: doctype::CUSTOM_FIELD.to_owned(),
        ..DocFacts::default()
    };
    let manager = Principal::with_roles("sam", &[role::SYSTEM_MANAGER]);
    let plain = Principal::with_roles("pat", &[]);
    assert!(engine.can(&manager, Action::Read, &doc).allowed);
    assert!(!engine.can(&plain, Action::Read, &doc).allowed);
    // `create` is a lifecycle operation the engine takes from the events contract, which this
    // crate does not depend on: read it off the shipped grant instead.
    let model = permissions().unwrap();
    let holds_create = |r: &str| {
        model.grants.iter().any(|g| {
            g.doctype == doctype::CUSTOM_FIELD
                && g.role.as_str() == r
                && g.rights.contains(spot_permissions::Right::Create)
        })
    };
    assert!(holds_create(role::SYSTEM_MANAGER));
    assert!(!holds_create(role::USER_MANAGER));
}
