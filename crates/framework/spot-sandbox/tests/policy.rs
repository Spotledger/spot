//! Classification and policy: metadata drives the choice, the guard overrides it, derived
//! copies inherit, and the policy is strict about what it accepts.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common as fake;

use spot_meta::{Definition, FieldSpec, FieldType, MaskSpec};
use spot_sandbox::MaskKind;
use spot_sandbox::mask::kind_for;
use spot_sandbox::{ClassificationSource, MaskingPolicy, MetadataClasses, SandboxPlan};

#[test]
fn the_default_policy_closes_the_four_classification_tags() {
    let policy = MaskingPolicy::default();
    for tag in ["pii", "financial", "secret", "health"] {
        assert!(policy.closes_tag(tag));
    }
    assert!(!policy.closes_tag("public"));
    assert!(policy.groups.is_empty());
    assert!(policy.mask_derived_copies);
}

#[test]
fn a_policy_loads_from_toml_and_refuses_a_misspelt_switch() {
    let policy =
        MaskingPolicy::from_toml("tags = [\"pii\"]\ngroups = [\"payroll\"]\nscan_min_chars = 5\n")
            .unwrap();
    assert_eq!(policy.tags, vec!["pii".to_owned()]);
    assert_eq!(policy.groups, vec!["payroll".to_owned()]);
    assert_eq!(policy.scan_min_chars, 5);

    assert!(MaskingPolicy::from_toml("tgas = [\"pii\"]\n").is_err());
    assert!(MaskingPolicy::from_toml("scan_min_chars = 0\n").is_err());
    assert!(MaskingPolicy::from_toml("tags = [\"pii\", \"pii\"]\n").is_err());
}

#[test]
fn a_field_is_closed_by_a_tag_a_group_or_a_mask() {
    let policy = MaskingPolicy::default();
    let mut by_tag = spot_sandbox::FieldClass::plain("t", "f", FieldType::Text);
    by_tag.tags.insert("pii".to_owned());
    assert!(by_tag.is_closed(&policy));

    let mut by_group = spot_sandbox::FieldClass::plain("t", "g", FieldType::Text);
    by_group.groups.insert("personal".to_owned());
    assert!(by_group.is_closed(&policy));

    let mut by_mask = spot_sandbox::FieldClass::plain("t", "m", FieldType::Text);
    by_mask.mask = Some(MaskSpec::placeholder());
    assert!(by_mask.is_closed(&policy));

    // Fail closed: a field with no classification is closed ...
    let plain = spot_sandbox::FieldClass::plain("t", "p", FieldType::Text);
    assert!(plain.is_closed(&policy));
    // ... and only the administrator's explicit Keep decision in the policy opens it.
    let keeping = MaskingPolicy {
        keep: vec![spot_sandbox::KeepField::new("t", "p")],
        ..MaskingPolicy::default()
    };
    assert!(!plain.is_closed(&keeping));
    // A Keep decision for one field opens no other field.
    let other = spot_sandbox::FieldClass::plain("t", "other", FieldType::Text);
    assert!(other.is_closed(&keeping));
}

#[test]
fn a_listed_group_set_narrows_what_is_closed() {
    let policy = MaskingPolicy::from_toml("groups = [\"payroll\"]\n").unwrap();
    let mut personal = spot_sandbox::FieldClass::plain("t", "f", FieldType::Text);
    personal.groups.insert("personal".to_owned());
    assert!(!personal.is_closed(&policy));
    personal.groups.insert("payroll".to_owned());
    assert!(personal.is_closed(&policy));
}

#[test]
fn amounts_links_and_text_choose_different_maskers() {
    let policy = MaskingPolicy::default();
    let classes = fake::sample_classes();

    let amount = classes.class("customer", "credit_limit").unwrap();
    assert_eq!(kind_for(&amount, &policy), MaskKind::AmountKept);

    let name = classes.class("customer", "name").unwrap();
    assert_eq!(kind_for(&name, &policy), MaskKind::Identifier);

    let notes = classes.class("customer", "notes").unwrap();
    assert_eq!(kind_for(&notes, &policy), MaskKind::FreeText);

    let secret = classes.class("customer", "api_key").unwrap();
    assert_eq!(kind_for(&secret, &policy), MaskKind::Secret);

    let link = classes.class("sales_invoice", "customer").unwrap();
    assert_eq!(kind_for(&link, &policy), MaskKind::LinkKept);

    let city = classes.class("customer", "city").unwrap();
    // An unclassified field is masked unless the policy keeps it explicitly.
    assert_eq!(kind_for(&city, &policy), MaskKind::Identifier);
    let keeping = MaskingPolicy {
        keep: vec![spot_sandbox::KeepField::new("customer", "city")],
        ..MaskingPolicy::default()
    };
    assert_eq!(kind_for(&city, &keeping), MaskKind::Unprotected);
}

#[test]
fn a_financial_identifier_is_masked_but_a_financial_amount_is_not() {
    let policy = MaskingPolicy::default();
    let classes = fake::sample_classes();
    let account = classes.class("customer", "bank_account").unwrap();
    assert_eq!(kind_for(&account, &policy), MaskKind::Identifier);
    let limit = classes.class("customer", "credit_limit").unwrap();
    assert_eq!(kind_for(&limit, &policy), MaskKind::AmountKept);
}

#[test]
fn a_denormalised_copy_inherits_its_source_protection() {
    let classes = fake::sample_classes();
    let copy = classes.class("sales_invoice", "customer_name").unwrap();
    assert_eq!(
        copy.derived_from,
        Some(("customer".to_owned(), "name".to_owned()))
    );
    assert!(copy.tags.contains("pii"));
    assert!(copy.groups.contains("personal"));
    let source = classes.class("customer", "name").unwrap();
    assert_eq!(copy.tags, source.tags);
}

#[test]
fn the_guard_overlays_its_facts_on_the_metadata() {
    let mut definition = Definition::new("widget");
    definition.fields = vec![FieldSpec::new("label", FieldType::Text)];
    let classes = MetadataClasses::from_definitions(&[definition]);
    let before = classes.class("widget", "label").unwrap();
    assert!(before.is_closed(&MaskingPolicy::default()));
    let keeping = MaskingPolicy {
        keep: vec![spot_sandbox::KeepField::new("widget", "label")],
        ..MaskingPolicy::default()
    };
    assert!(!before.is_closed(&keeping));

    let protection = spot_field_guard::StaticProtection::from_toml(
        r#"
        [[doctype]]
        name = "widget"
        [[doctype.fields]]
        path = "label"
        groups = ["personal"]
        tags = ["pii"]
        mask = { last_chars = 2 }
        "#,
    )
    .unwrap();
    let classes = MetadataClasses::from_definitions(&[Definition {
        name: "widget".to_owned(),
        fields: vec![FieldSpec::new("label", FieldType::Text)],
        ..Definition::new("widget")
    }])
    .with_protection(&protection);
    let after = classes.class("widget", "label").unwrap();
    assert!(after.tags.contains("pii"));
    assert!(after.groups.contains("personal"));
    assert_eq!(after.mask, Some(MaskSpec::last_chars(2)));
    assert!(after.is_closed(&MaskingPolicy::default()));
}

#[test]
fn classes_load_from_a_compact_fixture() {
    let classes = MetadataClasses::from_toml(
        r#"
        [[doctype]]
        name = "customer"
        [[doctype.field]]
        name = "email"
        type = "text"
        tags = ["pii"]
        group = "personal"
        [[doctype.field]]
        name = "site"
        type = "text"
        "#,
    )
    .unwrap();
    assert_eq!(classes.doctypes(), vec!["customer".to_owned()]);
    assert_eq!(
        classes.fields("customer"),
        vec!["email".to_owned(), "site".to_owned()]
    );
    let email = classes.class("customer", "email").unwrap();
    assert!(email.is_closed(&MaskingPolicy::default()));
    assert!(classes.class("customer", "site").unwrap().tags.is_empty());
    assert!(
        classes
            .class("customer", "site")
            .unwrap()
            .is_closed(&MaskingPolicy::default())
    );
    assert!(classes.class("customer", "missing").is_none());
}

#[test]
fn a_plan_round_trips_through_serde() {
    let plan = SandboxPlan::new(
        "prod_eu",
        Some(spot_sandbox::SnapshotRef::new(
            "s-1",
            "spot-snapshot",
            "prod_eu",
            10,
            20,
        )),
        MaskingPolicy::default(),
        "sandbox_acme",
    );
    let text = serde_json::to_string(&plan).unwrap();
    let back: SandboxPlan = serde_json::from_str(&text).unwrap();
    assert_eq!(back, plan);
    assert!(back.validate().is_ok());
}
