//! The webhook DocTypes are part of the platform app on every site (WP-INT-01.T3): the load
//! list is complete, the signing secret never leaves the server, the delivery log is written
//! only by the system, and a site created before them gains them additively.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeSet;
use std::path::PathBuf;

use common::{collation, engine, meta};
use spot_meta::{Customisations, FieldType, Registry};
use spot_permissions::model::{FieldFacts, GroupName, Principal};
use spot_permissions::{DocFacts, FieldLevel, FieldRight, FieldTarget, Right};
use spot_platform_app::{definitions, doctype, group, permissions, role};
use spot_storage_contract::StorageBackend;
use spot_storage_fake::FakeBackend;

/// DocType files that exist on disk but are not part of the embedded platform app today: both
/// predate this task (WP-PLT-10.T3 and WP-EXT-06.T1), have no shipped grants, and the lead must
/// decide whether to load them (WP-INT-01.T3 open question). The list may only shrink: a new
/// file left out of `DOCTYPE_SOURCES` fails the test below.
const NOT_YET_LOADED: [&str; 2] = ["file", "server_script"];

fn doctypes_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../apps/platform/doctypes")
}

#[test]
fn every_doctype_file_of_the_app_is_in_the_load_list() {
    let mut on_disk = BTreeSet::new();
    for entry in std::fs::read_dir(doctypes_dir()).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            on_disk.insert(path.file_stem().unwrap().to_string_lossy().into_owned());
        }
    }
    let loaded: BTreeSet<String> = definitions().unwrap().into_iter().map(|d| d.name).collect();
    for known in NOT_YET_LOADED {
        assert!(
            on_disk.remove(known),
            "{known} is gone from disk: shrink the list"
        );
        assert!(
            !loaded.contains(known),
            "{known} is loaded: shrink the list"
        );
    }
    assert_eq!(
        on_disk, loaded,
        "apps/platform/doctypes/*.toml and DOCTYPE_SOURCES must list the same DocTypes"
    );
    let all: BTreeSet<String> = doctype::ALL.iter().map(|d| (*d).to_owned()).collect();
    assert_eq!(all, loaded, "doctype::ALL and DOCTYPE_SOURCES must agree");
    assert_eq!(doctype::ALL.len(), 16);
    assert!(loaded.contains(doctype::WEBHOOK));
    assert!(loaded.contains(doctype::WEBHOOK_DELIVERY));
}

#[test]
fn the_delivery_log_holds_no_payload_header_query_or_remote_text() {
    let defs = definitions().unwrap();
    let log = defs
        .iter()
        .find(|d| d.name == doctype::WEBHOOK_DELIVERY)
        .unwrap();
    let names: Vec<&str> = log.fields.iter().map(|f| f.name.as_str()).collect();
    for expected in [
        "webhook",
        "event_id",
        "delivery_id",
        "attempt",
        "state",
        "status_code",
        "error_class",
        "started_at",
        "finished_at",
        "duration_ms",
    ] {
        assert!(names.contains(&expected), "missing {expected}");
    }
    for forbidden in [
        "payload", "body", "headers", "url", "query", "response", "message",
    ] {
        assert!(!names.contains(&forbidden), "{forbidden} must not exist");
    }
    for f in &log.fields {
        assert!(f.read_only, "{} must be read-only", f.name);
        assert!(
            !matches!(
                f.ty,
                FieldType::LongText | FieldType::Json | FieldType::Secret
            ),
            "{} must not hold free text",
            f.name
        );
        if f.ty == FieldType::Text {
            assert!(
                f.max_chars.is_some_and(|n| n <= 200),
                "{} is bounded",
                f.name
            );
        }
    }
    let error_class = log.fields.iter().find(|f| f.name == "error_class").unwrap();
    assert!(error_class.max_chars.is_some_and(|n| n <= 64));
    let state = log.fields.iter().find(|f| f.name == "state").unwrap();
    assert_eq!(
        state.options.as_deref(),
        Some("pending\ndelivered\nretrying\ndead_letter")
    );
}

#[test]
fn the_secret_group_is_closed_and_traced_and_no_grant_opens_it() {
    let model = permissions().unwrap();
    let declared = |name: &str| model.groups.iter().find(|g| g.name.as_str() == name);
    assert!(declared(group::WEBHOOK_SECRET).is_some_and(|g| g.trace));
    assert!(declared(group::WEBHOOK_CONFIG).is_some());
    // No field grant of any role reaches the secret group, by group or by field name.
    for g in &model.field_grants {
        let is_secret = match &g.target {
            FieldTarget::Group(n) => n.as_str() == group::WEBHOOK_SECRET,
            FieldTarget::Field(f) => g.doctype == doctype::WEBHOOK && f == "secret",
        };
        assert!(
            !is_secret,
            "{} has a grant on the webhook secret",
            g.role.as_str()
        );
        // The configuration group is read only by the two administrative roles.
        if let FieldTarget::Group(n) = &g.target
            && n.as_str() == group::WEBHOOK_CONFIG
            && g.access.contains(&FieldRight::Read)
        {
            assert!(
                [role::TENANT_ADMINISTRATOR, role::SYSTEM_MANAGER].contains(&g.role.as_str()),
                "{} may not read the configuration group",
                g.role.as_str()
            );
        }
    }
}

#[test]
fn no_role_except_the_system_and_the_tenant_administrator_bypass_reads_the_webhook_secret() {
    let m = meta();
    let engine = engine(&m);
    let facts = FieldFacts {
        path: "secret".to_owned(),
        groups: vec![GroupName(group::WEBHOOK_SECRET.to_owned())],
        ..FieldFacts::default()
    };
    let doc = DocFacts {
        doctype: doctype::WEBHOOK.to_owned(),
        ..DocFacts::default()
    };
    for r in [
        role::SYSTEM_MANAGER,
        role::USER_MANAGER,
        role::WEBHOOK_MANAGER,
        "Nobody",
    ] {
        let who = Principal::with_roles("someone", &[r]);
        let d = engine.field_access(&who, doctype::WEBHOOK, &facts, &doc);
        assert_eq!(d.level, FieldLevel::Hidden, "{r}");
        assert!(
            !d.can_create && !d.can_update && !d.can_report && !d.can_reveal,
            "{r}"
        );
    }
    let d = engine.field_access(&Principal::system(), doctype::WEBHOOK, &facts, &doc);
    assert_eq!(d.level, FieldLevel::ReadReveal);
}

/// KNOWN EXCEPTION pending the architecture owner's decision on ADR-070 s12 rule 4 (see session handoff): the
/// Tenant Administrator holds every right on every protection group, so it reads, reveals and
/// writes `webhook.secret` and `webhook.headers` through the engine. This pins the current
/// engine-wide behaviour (it also applies to the credential group); when the engine changes,
/// this test must flip.
#[test]
fn known_exception_the_tenant_administrator_reaches_the_webhook_secret_and_headers() {
    let m = meta();
    let engine = engine(&m);
    let doc = DocFacts {
        doctype: doctype::WEBHOOK.to_owned(),
        ..DocFacts::default()
    };
    let ta = Principal::with_roles("admin", &[role::TENANT_ADMINISTRATOR]);
    for (path, grp) in [
        ("secret", group::WEBHOOK_SECRET),
        ("headers", group::WEBHOOK_SECRET),
    ] {
        let facts = FieldFacts {
            path: path.to_owned(),
            groups: vec![GroupName(grp.to_owned())],
            ..FieldFacts::default()
        };
        let d = engine.field_access(&ta, doctype::WEBHOOK, &facts, &doc);
        assert_eq!(d.level, FieldLevel::ReadReveal, "{path}");
        assert!(d.can_reveal, "{path} reveal");
        assert!(d.can_update, "{path} update");
    }
}

#[test]
fn the_secret_field_is_not_a_title_list_filter_or_index_field() {
    let defs = definitions().unwrap();
    let hook = defs.iter().find(|d| d.name == doctype::WEBHOOK).unwrap();
    assert_ne!(hook.title_field.as_deref(), Some("secret"));
    let secret = hook.fields.iter().find(|f| f.name == "secret").unwrap();
    assert_eq!(secret.ty, FieldType::Secret);
    assert_eq!(
        secret.protection_group.as_deref(),
        Some(group::WEBHOOK_SECRET)
    );
    assert!(!secret.in_list_view && !secret.in_standard_filter && !secret.index);
    assert!(secret.masking.is_none() && secret.declassified.is_none());
}

#[test]
fn webhook_grants_are_least_privilege_and_nobody_writes_the_log() {
    let model = permissions().unwrap();
    for g in model
        .grants
        .iter()
        .filter(|g| g.doctype == doctype::WEBHOOK_DELIVERY)
    {
        for forbidden in [Right::Create, Right::Write, Right::Delete] {
            assert!(
                !g.rights.contains(forbidden),
                "{} on the log",
                g.role.as_str()
            );
        }
    }
    for g in model
        .grants
        .iter()
        .filter(|g| g.doctype == doctype::WEBHOOK && g.role.as_str() != role::SYSTEM_MANAGER)
    {
        assert_eq!(g.role.as_str(), role::WEBHOOK_MANAGER);
        for forbidden in [Right::Create, Right::Write, Right::Delete] {
            assert!(!g.rights.contains(forbidden), "Webhook Manager on webhook");
        }
    }
    let sm = model
        .grants
        .iter()
        .find(|g| g.doctype == doctype::WEBHOOK && g.role.as_str() == role::SYSTEM_MANAGER)
        .unwrap();
    for right in [Right::Read, Right::Write, Right::Create, Right::Delete] {
        assert!(sm.rights.contains(right));
    }
    let reads = |dt: &str| {
        model
            .grants
            .iter()
            .any(|g| g.doctype == dt && g.role.as_str() == role::WEBHOOK_MANAGER)
    };
    assert!(reads(doctype::WEBHOOK) && reads(doctype::WEBHOOK_DELIVERY));
    // Nobody else (no User Manager, no unknown role) has any right on either DocType.
    for g in model
        .grants
        .iter()
        .filter(|g| g.doctype == doctype::WEBHOOK || g.doctype == doctype::WEBHOOK_DELIVERY)
    {
        assert!(
            [role::SYSTEM_MANAGER, role::WEBHOOK_MANAGER].contains(&g.role.as_str()),
            "{}",
            g.role.as_str()
        );
    }
}

#[test]
fn every_grant_names_a_known_doctype_and_field() {
    let defs = definitions().unwrap();
    let model = permissions().unwrap();
    let find = |name: &str| defs.iter().find(|d| d.name == name);
    for g in &model.grants {
        assert!(
            find(&g.doctype).is_some(),
            "grant on unknown DocType {}",
            g.doctype
        );
    }
    for g in &model.field_grants {
        let d = find(&g.doctype)
            .unwrap_or_else(|| panic!("field grant on unknown DocType {}", g.doctype));
        match &g.target {
            FieldTarget::Field(f) => assert!(
                d.fields.iter().any(|x| &x.name == f),
                "unknown field {}.{f}",
                d.name
            ),
            FieldTarget::Group(n) => assert!(
                d.protection_groups.iter().any(|x| x.name == n.as_str()),
                "unknown group {}.{}",
                d.name,
                n.as_str()
            ),
        }
    }
    // The whole model also builds into a snapshot over the effective metadata.
    common::snapshot(&meta());
}

#[tokio::test]
async fn a_site_created_before_the_webhook_doctypes_gains_them_additively() {
    let old: Vec<_> = definitions()
        .unwrap()
        .into_iter()
        .filter(|d| d.name != doctype::WEBHOOK && d.name != doctype::WEBHOOK_DELIVERY)
        .collect();
    assert_eq!(old.len(), 14);
    let old_catalog = Registry::build(old, &Customisations::new())
        .unwrap()
        .catalog(collation())
        .unwrap();
    let new_catalog = meta().catalog(collation()).unwrap();
    let names = |c: &spot_storage_contract::Catalog| -> BTreeSet<String> {
        c.entities().map(|e| e.name.to_string()).collect()
    };
    let gained: Vec<String> = names(&new_catalog)
        .difference(&names(&old_catalog))
        .cloned()
        .collect();
    assert_eq!(gained, [doctype::WEBHOOK, doctype::WEBHOOK_DELIVERY]);
    assert!(
        names(&old_catalog).is_subset(&names(&new_catalog)),
        "nothing removed"
    );
    // `spot migrate` plans the live database against this catalogue (`schema::catalog` over
    // `apps::serving`, which reads `spot_platform_app::definitions()` for the embedded
    // platform app). Storage accepts the later catalogue over the earlier one.
    let backend = FakeBackend::new();
    backend.prepare(&old_catalog).await.unwrap();
    backend.prepare(&new_catalog).await.unwrap();
}

/// The webhook protection groups are declared in `groups.toml`, the one file a catalogue-loaded
/// platform app reads (WP-INT-01.T6); no inline copy in the crate adds them.
#[test]
fn the_webhook_groups_come_from_groups_toml() {
    let texts = spot_platform_app::permission_texts();
    let file = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../apps/platform/permissions/groups.toml"),
    )
    .unwrap();
    assert_eq!(texts.groups, file);
    for name in [group::WEBHOOK_SECRET, group::WEBHOOK_CONFIG] {
        assert!(file.contains(&format!("name = \"{name}\"")), "{name}");
    }
}

// ---- WP-INT-01.T7: the protection groups of `webhook.toml` ----

const PROTECTED_BY_CONFIG: [&str; 3] = ["url", "condition", "field_allowlist"];
const PROTECTED_BY_SECRET: [&str; 2] = ["secret", "headers"];

fn webhook_def() -> spot_meta::Definition {
    definitions()
        .unwrap()
        .into_iter()
        .find(|d| d.name == doctype::WEBHOOK)
        .unwrap()
}

/// The access the engine gives `who` to `webhook.<field>`, over the field's real protection
/// facts (groups and tags) from the effective metadata.
fn access_to(who: &Principal, field: &str) -> spot_permissions::FieldDecision {
    let m = meta();
    let snapshot = common::snapshot(&m);
    let doc_facts = snapshot.facts(doctype::WEBHOOK).unwrap();
    let facts = doc_facts
        .field(field)
        .unwrap_or_else(|| panic!("no protection facts for webhook.{field}"))
        .clone();
    let doc = DocFacts {
        doctype: doctype::WEBHOOK.to_owned(),
        ..DocFacts::default()
    };
    engine(&m).field_access(who, doctype::WEBHOOK, &facts, &doc)
}

#[test]
fn every_field_the_comments_call_protected_is_in_a_group() {
    let def = webhook_def();
    let group_of = |name: &str| {
        def.fields
            .iter()
            .find(|f| f.name == name)
            .unwrap_or_else(|| panic!("webhook.{name} is gone"))
            .protection_group
            .clone()
    };
    for f in PROTECTED_BY_CONFIG {
        assert_eq!(
            group_of(f).as_deref(),
            Some(group::WEBHOOK_CONFIG),
            "webhook.{f}"
        );
    }
    for f in PROTECTED_BY_SECRET {
        assert_eq!(
            group_of(f).as_deref(),
            Some(group::WEBHOOK_SECRET),
            "webhook.{f}"
        );
    }
    // The `config` group is not empty, and neither group holds anything else.
    for f in &def.fields {
        let expected = if PROTECTED_BY_CONFIG.contains(&f.name.as_str()) {
            Some(group::WEBHOOK_CONFIG)
        } else if PROTECTED_BY_SECRET.contains(&f.name.as_str()) {
            Some(group::WEBHOOK_SECRET)
        } else {
            None
        };
        assert_eq!(
            f.protection_group.as_deref(),
            expected,
            "webhook.{}",
            f.name
        );
        // A field the list view cannot show to the listing roles is not in the list view.
        if f.protection_group.is_some() {
            assert!(!f.in_list_view, "webhook.{} is in a closed group", f.name);
        }
    }
    let secret = def.fields.iter().find(|f| f.name == "secret").unwrap();
    assert!(secret.classification.iter().any(|t| t == "secret"));
    assert_eq!(secret.ty, FieldType::Secret);
}

#[test]
fn secret_and_headers_are_in_a_group_no_grant_opens() {
    let model = permissions().unwrap();
    for g in &model.field_grants {
        if g.doctype != doctype::WEBHOOK {
            continue;
        }
        match &g.target {
            FieldTarget::Group(n) => assert_ne!(n.as_str(), group::WEBHOOK_SECRET),
            FieldTarget::Field(f) => assert!(
                !PROTECTED_BY_SECRET.contains(&f.as_str()),
                "{} has a grant on webhook.{f}",
                g.role.as_str()
            ),
        }
    }
    // Through the engine, over the real protection facts: nobody but the system and the
    // Tenant Administrator (see the known exception above) reads either field.
    for r in [
        role::SYSTEM_MANAGER,
        role::USER_MANAGER,
        role::WEBHOOK_MANAGER,
        "Nobody",
    ] {
        let who = Principal::with_roles("someone", &[r]);
        for f in PROTECTED_BY_SECRET {
            let d = access_to(&who, f);
            assert_eq!(d.level, FieldLevel::Hidden, "{r} reads webhook.{f}");
            assert!(!d.can_update && !d.can_reveal, "{r} webhook.{f}");
        }
    }
}

#[test]
fn a_webhook_manager_reads_the_record_without_the_protected_fields() {
    let who = Principal::with_roles("wm", &[role::WEBHOOK_MANAGER]);
    let def = webhook_def();
    for f in &def.fields {
        let d = access_to(&who, &f.name);
        if PROTECTED_BY_CONFIG.contains(&f.name.as_str())
            || PROTECTED_BY_SECRET.contains(&f.name.as_str())
        {
            assert_eq!(d.level, FieldLevel::Hidden, "webhook.{}", f.name);
        } else {
            assert_ne!(
                d.level,
                FieldLevel::Hidden,
                "webhook.{} is readable",
                f.name
            );
        }
    }
    // No role in the shipped model other than the two administrative ones reads `config`.
    let sm = Principal::with_roles("sm", &[role::SYSTEM_MANAGER]);
    for f in PROTECTED_BY_CONFIG {
        // Recorded as is today: no shipped field grant opens `config` for any role.
        assert_eq!(access_to(&sm, f).level, FieldLevel::Hidden, "{f}");
    }
}

/// What the engine does today for the Tenant Administrator (the role bootstrap seeds, holding
/// every right), over the REAL protection facts of `webhook.secret` and `webhook.headers`:
/// recorded, not endorsed. WP-INT-01.T3's brief says the `secret` group never leaves the
/// server; for this principal it does, as ADR-070 s12 rule 4 has it. When the architecture owner decides
/// otherwise and the engine changes, this test must flip.
#[test]
fn the_tenant_administrator_through_the_real_facts_of_secret_and_headers() {
    let ta = Principal::with_roles("admin", &[role::TENANT_ADMINISTRATOR]);
    for f in PROTECTED_BY_SECRET {
        let d = access_to(&ta, f);
        assert_eq!(d.level, FieldLevel::ReadReveal, "webhook.{f}");
        assert!(d.can_reveal && d.can_update, "webhook.{f}");
    }
    for f in PROTECTED_BY_CONFIG {
        assert_eq!(
            access_to(&ta, f).level,
            FieldLevel::ReadReveal,
            "webhook.{f}"
        );
    }
}
