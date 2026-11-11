#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
//! The change pipeline: rules, validation (every error at once), refusals, audit, grants.

mod common;
use common::*;
use spot_customise::{Caller, DefaultGrant, RecordState, Referrer, Right, RoleAccess, codes};
use spot_meta::{FieldSpec, FieldType};
use std::sync::Arc;

fn text_field(name: &str) -> FieldSpec {
    FieldSpec::new(name, FieldType::Text)
}

async fn with_ticket(rig: &Rig) {
    rig.service
        .create_doctypes(&admin(), &[TICKET, TICKET_NOTE])
        .await
        .unwrap();
}

#[tokio::test]
async fn only_a_role_holding_the_customise_right_may_call_the_service() {
    let rig = Rig::new().await;
    let denied = clerk();
    let results = [
        rig.service.create_doctype(&denied, TICKET_NOTE).await,
        rig.service
            .add_custom_field(&denied, "invoice", &text_field("custom_x"), None)
            .await,
        rig.service
            .set_property(&denied, "invoice", None, "label", "Bill")
            .await,
        rig.service
            .clear_property(&denied, "invoice", None, "label")
            .await,
        rig.service
            .remove_custom_field(&denied, "invoice", "custom_x")
            .await,
        rig.service.update_doctype(&denied, "ticket", TICKET).await,
    ];
    for result in results {
        assert_eq!(result.unwrap_err().code(), "customise.forbidden");
    }
    assert_eq!(
        rig.service.list(&denied).await.unwrap_err().code(),
        "customise.forbidden"
    );
    assert!(
        rig.records.snapshot().doctypes.is_empty(),
        "nothing was persisted"
    );

    // A role the site granted the right to may.
    let rig2 = Rig::new().await;
    let service = Arc::new(
        spot_customise::CustomiseService::new(spot_customise::Parts {
            site: site(),
            base: base_definitions(),
            options: spot_meta::BuildOptions::new(),
            live: rig2.live.clone(),
            bus: rig2.bus.clone(),
            versions: rig2.versions.clone(),
            definitions: rig2.versions.clone(),
            records: rig2.records.clone(),
            schema: rig2.schema.clone(),
        })
        .with_access(Arc::new(RoleAccess::new().granting("Accounts User"))),
    );
    service
        .add_custom_field(&clerk(), "invoice", &text_field("custom_note"), None)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_new_doctype_gets_one_default_grant_and_lives_in_the_custom_module() {
    let rig = Rig::new().await;
    with_ticket(&rig).await;
    let grants: Vec<DefaultGrant> = rig
        .host
        .grants()
        .into_iter()
        .filter(|g| g.doctype == "ticket")
        .collect();
    assert_eq!(
        grants,
        vec![DefaultGrant {
            doctype: "ticket".into(),
            role: "System Manager".into(),
            rights: vec![Right::Read, Right::Write, Right::Create, Right::Delete],
        }],
        "System Manager read/write/create/delete and nobody else: closed by default"
    );
    let snapshot = rig.live.current();
    let definition = snapshot.meta().definition("ticket").unwrap();
    assert_eq!(definition.module.as_deref(), Some("custom"));
    assert_eq!(definition.app.as_deref(), Some("custom"));

    // A module other than `custom` is for a platform administrator only.
    let other = TICKET_NOTE.replace(
        "name = \"ticket_note\"",
        "name = \"other_note\"\nmodule = \"sales\"",
    );
    let mut customiser = Caller::new("sam", &["Customiser"]);
    let service = Arc::new(
        spot_customise::CustomiseService::new(spot_customise::Parts {
            site: site(),
            base: base_definitions(),
            options: spot_meta::BuildOptions::new(),
            live: rig.live.clone(),
            bus: rig.bus.clone(),
            versions: rig.versions.clone(),
            definitions: rig.versions.clone(),
            records: rig.records.clone(),
            schema: rig.schema.clone(),
        })
        .with_access(Arc::new(RoleAccess::new().granting("Customiser"))),
    );
    let parent = r#"
name = "other_parent"
module = "sales"
[[fields]]
name = "rows"
type = "table"
options = "other_note"
"#;
    let refused = service
        .create_doctypes(&customiser, &[&other, parent])
        .await
        .unwrap_err();
    assert_eq!(
        refused.issue_codes(),
        vec![codes::MODULE_NOT_ALLOWED, codes::MODULE_NOT_ALLOWED],
        "both are reported: {refused}"
    );
    customiser.roles.insert("System Manager".into());
    service
        .create_doctypes(&customiser, &[&other, parent])
        .await
        .unwrap();
    assert_eq!(
        rig.live
            .current()
            .meta()
            .definition("other_parent")
            .unwrap()
            .module
            .as_deref(),
        Some("sales")
    );
}

#[tokio::test]
async fn name_collisions_and_reserved_names_are_refused_all_at_once() {
    let rig = Rig::new().await;
    with_ticket(&rig).await;
    // `party` is an app DocType, `custom_field` is reserved, `spot_vault` has a platform
    // prefix, `ticket` already exists: four problems in one call, none dropped.
    let defs = [
        "name = \"party\"\n",
        "name = \"custom_field\"\n",
        "name = \"spot_vault\"\n",
        "name = \"ticket\"\n",
    ];
    let err = rig
        .service
        .create_doctypes(&admin(), &defs)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "customise.invalid");
    let mut codes_found = err.issue_codes();
    codes_found.sort_unstable();
    assert_eq!(
        codes_found,
        vec![
            codes::NAME_COLLISION,
            codes::NAME_COLLISION,
            codes::RESERVED_NAME,
            codes::RESERVED_NAME
        ],
        "{err}"
    );
    let paths: Vec<&str> = err.issues().iter().map(|i| i.path.as_str()).collect();
    for expected in ["party", "custom_field", "spot_vault", "ticket"] {
        assert!(paths.contains(&expected), "{expected} missing in {paths:?}");
    }
}

#[tokio::test]
async fn bad_option_strings_are_all_reported_with_field_paths() {
    let rig = Rig::new().await;
    let bad = r#"
name = "survey"
[[fields]]
name = "mood"
type = "select"
options = "good\nbad\ngood"
[[fields]]
name = "size"
type = "select"
[[fields]]
name = "tone"
type = "select"
options = "\n"
"#;
    let err = rig.service.create_doctype(&admin(), bad).await.unwrap_err();
    let paths: Vec<&str> = err.issues().iter().map(|i| i.path.as_str()).collect();
    for expected in ["survey.mood", "survey.size", "survey.tone"] {
        assert!(
            paths.contains(&expected),
            "{expected} missing in {paths:?}: {err}"
        );
    }
    assert!(
        err.issue_codes()
            .iter()
            .all(|c| *c == codes::METADATA_INVALID)
    );
    assert!(rig.records.snapshot().doctypes.is_empty());
}

#[tokio::test]
async fn a_cycle_among_defaults_and_fetches_is_refused_with_the_cycle_named() {
    let rig = Rig::new().await;
    // Two text fields that each take their value from the other through a link.
    let cyclic = r#"
name = "loopy"
[[fields]]
name = "a"
type = "int"
default = "=doc.b"
[[fields]]
name = "b"
type = "int"
default = "=doc.a"
"#;
    let err = rig
        .service
        .create_doctype(&admin(), cyclic)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "customise.invalid", "{err}");
    assert!(
        err.issue_codes().contains(&codes::PLAN_INVALID),
        "a plan cycle is reported: {err}"
    );
    assert!(
        rig.records.snapshot().doctypes.is_empty(),
        "nothing persisted"
    );
}

#[tokio::test]
async fn a_custom_field_may_not_collide_or_skip_the_prefix() {
    let rig = Rig::new().await;
    let a = rig
        .service
        .add_custom_field(&admin(), "invoice", &text_field("title"), None)
        .await
        .unwrap_err();
    assert_eq!(a.issue_codes(), vec![codes::FIELD_COLLISION], "{a}");
    let no_prefix = rig
        .service
        .add_custom_field(&admin(), "invoice", &text_field("note"), None)
        .await
        .unwrap_err();
    assert_eq!(
        no_prefix.issue_codes(),
        vec![codes::METADATA_INVALID],
        "{no_prefix}"
    );
    assert!(no_prefix.to_string().contains("must start with `custom_`"));
    // The prefix is `custom_`; with it a name that exists is a collision.
    rig.service
        .add_custom_field(&admin(), "invoice", &text_field("custom_note"), None)
        .await
        .unwrap();
    let again = rig
        .service
        .add_custom_field(&admin(), "invoice", &text_field("custom_note"), None)
        .await
        .unwrap_err();
    assert_eq!(again.issue_codes(), vec![codes::FIELD_COLLISION]);
    let unknown = rig
        .service
        .add_custom_field(&admin(), "nope", &text_field("custom_x"), None)
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), "customise.invalid");
}

#[tokio::test]
async fn a_destructive_change_is_refused_and_leaves_nothing_behind() {
    let rig = Rig::new().await;
    with_ticket(&rig).await;
    let version = rig.live.current().version();
    let records = rig.records.snapshot();
    let audit = rig.host.audit().len();
    let events = rig.host.events().len();
    // Dropping the `priority` field would drop a column.
    let without = TICKET.replace(
        "[[fields]]\nname = \"priority\"\ntype = \"select\"\noptions = \"low\\nnormal\\nurgent\"\ndefault = \"normal\"\n",
        "",
    );
    assert_ne!(without, TICKET);
    let err = rig
        .service
        .update_doctype(&admin(), "ticket", &without)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "customise.destructive_change", "{err}");
    assert!(
        err.to_string().contains("priority"),
        "the step is named: {err}"
    );
    assert_eq!(rig.records.snapshot(), records, "no record changed");
    assert_eq!(
        rig.live.current().version(),
        version,
        "the snapshot is the same"
    );
    assert_eq!(rig.host.audit().len(), audit);
    assert_eq!(rig.host.events().len(), events);
    assert!(
        rig.live
            .current()
            .meta()
            .definition("ticket")
            .unwrap()
            .fields
            .iter()
            .any(|f| f.name == "priority")
    );
}

#[tokio::test]
async fn an_additive_update_adds_a_field_and_a_label_override_replaces_and_clears() {
    let rig = Rig::new().await;
    with_ticket(&rig).await;
    let wider = TICKET.replace(
        "[[fields]]\nname = \"notes\"",
        "[[fields]]\nname = \"due\"\ntype = \"date\"\n[[fields]]\nname = \"notes\"",
    );
    rig.service
        .update_doctype(&admin(), "ticket", &wider)
        .await
        .unwrap();
    assert!(
        rig.live
            .current()
            .meta()
            .definition("ticket")
            .unwrap()
            .fields
            .iter()
            .any(|f| f.name == "due")
    );

    rig.service
        .set_property(&admin(), "invoice", None, "label", "Bill")
        .await
        .unwrap();
    rig.service
        .set_property(&admin(), "invoice", None, "label", "Sales Bill")
        .await
        .unwrap();
    rig.service
        .set_property(&admin(), "invoice", Some("amount"), "read_only", "true")
        .await
        .unwrap();
    let snapshot = rig.live.current();
    let invoice = snapshot.meta().definition("invoice").unwrap();
    assert_eq!(invoice.label.as_deref(), Some("Sales Bill"));
    assert!(
        invoice
            .fields
            .iter()
            .find(|f| f.name == "amount")
            .unwrap()
            .read_only
    );
    // The second set replaced the first: one record.
    let labels = rig
        .records
        .snapshot()
        .properties
        .into_iter()
        .filter(|p| p.property == "label")
        .count();
    assert_eq!(labels, 1);
    // Clearing restores the standard.
    rig.service
        .clear_property(&admin(), "invoice", None, "label")
        .await
        .unwrap();
    assert_eq!(
        rig.live
            .current()
            .meta()
            .definition("invoice")
            .unwrap()
            .label
            .as_deref(),
        Some("Invoice")
    );
    // A property not in the allowed set is refused with the key named.
    let err = rig
        .service
        .set_property(&admin(), "invoice", Some("amount"), "unique", "true")
        .await
        .unwrap_err();
    assert_eq!(err.code(), "customise.invalid");
    // Clearing what was never set is "not found".
    let missing = rig
        .service
        .clear_property(&admin(), "invoice", None, "label")
        .await
        .unwrap_err();
    assert_eq!(missing.code(), "customise.not_found");
}

#[tokio::test]
async fn a_security_property_can_be_tightened_but_never_loosened() {
    let rig = Rig::new().await;
    // Tighten: add a classification tag, and put a field in a (site-owned) protection group.
    rig.service
        .set_property(
            &admin(),
            "invoice",
            Some("tax_id"),
            "classification",
            "pii\nfinancial",
        )
        .await
        .unwrap();
    rig.service
        .set_property(
            &admin(),
            "invoice",
            Some("amount"),
            "protection_group",
            "custom_money",
        )
        .await
        .unwrap();
    // Loosen by set: drop the app's tag.
    let drop_tag = rig
        .service
        .set_property(
            &admin(),
            "invoice",
            Some("tax_id"),
            "classification",
            "financial",
        )
        .await
        .unwrap_err();
    assert!(
        drop_tag
            .to_string()
            .contains("cannot remove classification tag `pii`"),
        "spot-meta's validation catches it: {drop_tag}"
    );
    // Loosen by set: lower the permlevel the app declared.
    let lower = rig
        .service
        .set_property(&admin(), "invoice", Some("ref_party"), "permlevel", "1")
        .await
        .unwrap_err();
    assert!(
        lower.issue_codes().contains(&codes::SECURITY_LOOSENED),
        "{lower}"
    );
    // Loosen by clear: removing the override of a standard field's classification.
    let clear = rig
        .service
        .clear_property(&admin(), "invoice", Some("tax_id"), "classification")
        .await
        .unwrap_err();
    assert_eq!(clear.issue_codes(), vec![codes::SECURITY_LOOSENED]);
    // The override on the group a site made for a standard field is a site choice: clearing the
    // group of a standard field is also a loosening.
    let group = rig
        .service
        .clear_property(&admin(), "invoice", Some("amount"), "protection_group")
        .await
        .unwrap_err();
    assert_eq!(group.issue_codes(), vec![codes::SECURITY_LOOSENED]);
}

#[tokio::test]
async fn removing_a_field_that_something_refers_to_lists_the_referrers() {
    let rig = Rig::new().await;
    let mut customer = FieldSpec::new("custom_customer", FieldType::Link);
    customer.options = Some("party".into());
    rig.service
        .add_custom_field(&admin(), "invoice", &customer, None)
        .await
        .unwrap();
    let mut fetched = text_field("custom_customer_name");
    fetched.fetch_from = Some("custom_customer.party_name".into());
    rig.service
        .add_custom_field(&admin(), "invoice", &fetched, None)
        .await
        .unwrap();
    rig.host.add_referrer(
        "invoice",
        "custom_customer",
        Referrer {
            kind: "view".into(),
            name: "invoice list".into(),
        },
    );
    let err = rig
        .service
        .remove_custom_field(&admin(), "invoice", "custom_customer")
        .await
        .unwrap_err();
    assert_eq!(err.code(), "customise.invalid");
    let text = err.to_string();
    assert!(text.contains("field `custom_customer_name`"), "{text}");
    assert!(text.contains("view `invoice list`"), "{text}");
    assert!(err.issue_codes().contains(&codes::REFERENCED));
    // The referrer first, then the field.
    rig.service
        .remove_custom_field(&admin(), "invoice", "custom_customer_name")
        .await
        .unwrap();
    let stuck = rig
        .service
        .remove_custom_field(&admin(), "invoice", "custom_customer")
        .await
        .unwrap_err();
    assert!(stuck.to_string().contains("view `invoice list`"));
}

#[tokio::test]
async fn every_change_is_audited_with_the_actor_and_before_and_after_text() {
    let rig = Rig::new().await;
    let sam = Caller::new("sam", &["System Manager"]);
    rig.service
        .add_custom_field(&sam, "invoice", &text_field("custom_note"), None)
        .await
        .unwrap();
    rig.service
        .set_property(&sam, "invoice", None, "label", "Bill")
        .await
        .unwrap();
    rig.service
        .set_property(&admin(), "invoice", None, "label", "Sales Bill")
        .await
        .unwrap();
    rig.service
        .clear_property(&sam, "invoice", None, "label")
        .await
        .unwrap();
    rig.service
        .remove_custom_field(&sam, "invoice", "custom_note")
        .await
        .unwrap();
    let audit = rig.host.audit();
    let actions: Vec<_> = audit.iter().map(|a| (a.action, a.actor.as_str())).collect();
    assert_eq!(
        actions,
        vec![
            ("add_custom_field", "sam"),
            ("set_property", "sam"),
            ("set_property", "alice"),
            ("clear_property", "sam"),
            ("remove_custom_field", "sam"),
        ]
    );
    assert_eq!(audit[0].subject, "invoice.custom_note");
    assert!(
        audit[0].before.is_none() && audit[0].after.as_deref().unwrap().contains("custom_note")
    );
    assert_eq!(audit[2].before.as_deref(), Some("Bill"));
    assert_eq!(audit[2].after.as_deref(), Some("Sales Bill"));
    assert_eq!(audit[3].before.as_deref(), Some("Sales Bill"));
    assert!(audit[3].after.is_none());
    assert!(
        audit[4].before.as_deref().unwrap().contains("custom_note") && audit[4].after.is_none()
    );
    let versions: Vec<u64> = audit.iter().map(|a| a.version).collect();
    assert!(
        versions.windows(2).all(|w| w[0] < w[1]),
        "versions rise: {versions:?}"
    );

    let kinds: Vec<_> = rig.host.events().into_iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            "field_added",
            "property_set",
            "property_set",
            "property_cleared",
            "field_removed"
        ]
    );
    // The label override is gone; the removed field stays as a tombstone, because its column
    // stays in the database (a runtime change never drops data).
    let listed = rig.service.list(&admin()).await.unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].state, RecordState::Removed);
    assert!(
        !rig.live
            .current()
            .meta()
            .definition("invoice")
            .unwrap()
            .fields
            .iter()
            .any(|f| f.name == "custom_note"),
        "the metadata no longer has the field"
    );
    // The column is still in the schema, so later changes are not refused as a drop...
    rig.service
        .add_custom_field(&sam, "invoice", &text_field("custom_other"), None)
        .await
        .unwrap();
    // ... and the same name can be added again, over the tombstone.
    rig.service
        .add_custom_field(&sam, "invoice", &text_field("custom_note"), None)
        .await
        .unwrap();
    assert!(
        rig.live
            .current()
            .meta()
            .definition("invoice")
            .unwrap()
            .fields
            .iter()
            .any(|f| f.name == "custom_note")
    );
}

#[tokio::test]
async fn a_new_doctype_is_closed_to_everyone_but_the_system_manager_until_granted() {
    use spot_permissions::{
        Action, DocFacts, Model, PermissionEngine, Principal, Rights, Role, RoleGrant, Snapshot,
    };
    let rig = Rig::new().await;
    with_ticket(&rig).await;
    // The grants the service handed the permission layer, evaluated by the real evaluator.
    let model = Model {
        grants: rig
            .host
            .grants()
            .into_iter()
            .map(|g| RoleGrant {
                doctype: g.doctype,
                role: Role(g.role),
                rights: Rights::new(g.rights),
                if_owner: false,
            })
            .collect(),
        ..Model::default()
    };
    let snapshot = Snapshot::build(rig.live.current().meta(), &model).unwrap();
    let engine = PermissionEngine::new(Arc::new(snapshot));
    let facts = DocFacts {
        doctype: "ticket".into(),
        id: None,
        created_by: None,
        state: None,
        values: Default::default(),
    };
    let can = |roles: &[&str], action: &str| {
        engine
            .can(
                &Principal::with_roles("someone", roles),
                Action::parse(action).unwrap(),
                &facts,
            )
            .allowed
    };
    for action in ["read", "update", "create", "delete"] {
        assert!(
            can(&["System Manager"], action),
            "System Manager may {action}"
        );
        assert!(!can(&["Accounts User"], action), "nobody else may {action}");
    }
    assert!(
        !can(&["System Manager"], "post"),
        "only the four rights are granted"
    );
}
