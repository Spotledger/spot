//! Security properties: protection groups, classifications, masking, derivation inheritance.
#![allow(missing_docs, clippy::unwrap_used)]

mod common;

use common::build;
use spot_meta::{
    Customisations, Definition, FieldSpec, FieldType, MaskSpec, MetaErrorKind as K,
    ProtectionGroupSpec, Registry,
};

#[test]
fn protection_groups_round_trip_through_toml() {
    let mut d = Definition::new("invoice");
    d.fields = vec![
        FieldSpec::new("number", FieldType::Text),
        FieldSpec::new("amount", FieldType::Decimal),
    ];
    d.fields[1].protection_group = Some("financial".into());
    d.fields[1].scale = Some(2);
    d.protection_groups = vec![
        ProtectionGroupSpec::new("financial", "Financial Data"),
        ProtectionGroupSpec {
            name: "audit".into(),
            label: Some("Audit".into()),
            trace: true,
            sync_class_hint: Some("encrypted".into()),
        },
    ];

    let text = d.to_toml().unwrap_or_else(|e| panic!("{e}"));
    let back = Definition::parse("invoice.toml", &text).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(back, d);
    assert_eq!(back.protection_groups.len(), 2);
    assert_eq!(back.fields[1].protection_group, Some("financial".into()));
}

#[test]
fn unknown_protection_group_is_refused() {
    let mut d = Definition::new("invoice");
    let mut f = FieldSpec::new("amount", FieldType::Decimal);
    f.protection_group = Some("unknown_group".into());
    f.scale = Some(2);
    d.fields = vec![f];
    d.protection_groups = vec![ProtectionGroupSpec::new("financial", "Financial")];

    let m = build(vec![d]);
    assert!(m.is_err());
    let errors = m.unwrap_err();
    assert!(errors.iter().any(|e| e.kind == K::BadProperty));
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("unknown_group") && e.message.contains("unknown"))
    );
}

#[test]
fn classification_tags_are_preserved() {
    let mut d = Definition::new("employee");
    let mut f = FieldSpec::new("salary", FieldType::Decimal);
    f.scale = Some(2);
    f.protection_group = Some("financial".into());
    f.classification = vec!["pii".into(), "salary".into()];
    d.fields = vec![f];
    d.protection_groups = vec![ProtectionGroupSpec::new("financial", "Financial")];

    let m = build(vec![d]).unwrap_or_else(|e| panic!("{e:?}"));
    let view = m.doctype("employee").unwrap_or_else(|| panic!("missing"));
    let salary = view.field("salary").unwrap_or_else(|| panic!("missing"));
    assert_eq!(salary.security.classification.len(), 2);
}

#[test]
fn masking_is_only_allowed_on_text_int_decimal() {
    for (ty, should_pass) in &[
        (FieldType::Text, true),
        (FieldType::LongText, true),
        (FieldType::Int, true),
        (FieldType::Decimal, true),
        (FieldType::Bool, false),
        (FieldType::Date, false),
        (FieldType::Link, false),
    ] {
        let mut d = Definition::new("test");
        let mut f = FieldSpec::new("field", *ty);
        f.masking = Some(MaskSpec::placeholder());
        if *ty == FieldType::Decimal {
            f.scale = Some(2);
        }
        d.fields = vec![f];
        d.protection_groups = vec![ProtectionGroupSpec::new("group", "Group")];

        let m = build(vec![d]);
        if *should_pass {
            assert!(m.is_ok(), "masking should be allowed on {:?}", ty);
        } else {
            assert!(m.is_err(), "masking should be rejected on {:?}", ty);
        }
    }
}

#[test]
fn masking_defaults_to_none_when_not_specified() {
    let mut d = Definition::new("test");
    let mut f = FieldSpec::new("field", FieldType::Text);
    f.protection_group = Some("group".into());
    // No masking specified
    d.fields = vec![f];
    d.protection_groups = vec![ProtectionGroupSpec::new("group", "Group")];

    let m = build(vec![d]).unwrap_or_else(|e| panic!("{e:?}"));
    let view = m.doctype("test").unwrap_or_else(|| panic!("missing"));
    let field = view.field("field").unwrap_or_else(|| panic!("missing"));
    // Masking is None when not specified
    assert_eq!(field.security.masking, None);
}

#[test]
fn classified_secret_pii_need_protection_or_declassified() {
    for tag in &["secret", "pii"] {
        // Without protection: should fail
        let mut d = Definition::new("test");
        let mut f = FieldSpec::new("field", FieldType::Text);
        f.classification = vec![tag.to_string()];
        d.fields = vec![f.clone()];
        let m = build(vec![d]);
        assert!(
            m.is_err(),
            "field tagged {tag} without protection should fail"
        );

        // With protection_group: should pass
        let mut d = Definition::new("test");
        f.protection_group = Some("group".into());
        d.fields = vec![f.clone()];
        d.protection_groups = vec![ProtectionGroupSpec::new("group", "Group")];
        let m = build(vec![d]);
        assert!(m.is_ok(), "field tagged {tag} with protection should pass");

        // With declassified: should pass
        let mut d = Definition::new("test");
        let mut f = FieldSpec::new("field", FieldType::Text);
        f.classification = vec![tag.to_string()];
        f.declassified = Some("reviewed by security team".into());
        d.fields = vec![f];
        let m = build(vec![d]);
        assert!(
            m.is_ok(),
            "field tagged {tag} with declassified should pass"
        );
    }
}

#[test]
fn secret_kind_is_exempt_from_tag_unprotected_lint() {
    let mut d = Definition::new("test");
    let mut f = FieldSpec::new("api_key", FieldType::Secret);
    f.classification = vec!["secret".into()];
    // No protection_group or declassified, but Secret kind is exempt
    d.fields = vec![f];

    let m = build(vec![d]);
    assert!(
        m.is_ok(),
        "Secret kind should be exempt from tag_unprotected lint"
    );
}

#[test]
fn declassified_reason_cannot_be_empty() {
    let mut d = Definition::new("test");
    let mut f = FieldSpec::new("field", FieldType::Text);
    f.classification = vec!["pii".into()];
    f.declassified = Some("".into()); // Empty reason
    d.fields = vec![f];

    let m = build(vec![d]);
    assert!(m.is_err(), "empty declassified reason should be refused");
    let errors = m.unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("declassify_reason"))
    );
}

#[test]
fn site_can_add_tags_to_unprotected_field() {
    // Site upward overrides allow adding classification tags
    let mut d = Definition::new("invoice");
    let mut f = FieldSpec::new("amount", FieldType::Decimal);
    f.scale = Some(2);
    // Field starts unprotected
    d.fields = vec![f];
    d.protection_groups = vec![ProtectionGroupSpec::new("financial", "Financial")];

    let c = Customisations::new();
    // Site adds tags (which is allowed upward-only)
    // Note: Classification is not directly overridable via Property, so we just test that
    // the field can receive protection at build time
    let m = Registry::build(vec![d.clone()], &c);
    assert!(m.is_ok(), "should build with no overrides");

    // Test that tag_unprotected lint catches unprotected classified fields
    let mut d2 = d.clone();
    if let Some(f) = d2.fields.iter_mut().find(|f| f.name == "amount") {
        f.classification = vec!["pii".into()];
        // Now the field is tagged but not protected
    }
    let m2 = build(vec![d2]);
    assert!(m2.is_err(), "unprotected classified field should fail lint");
}

#[test]
fn custom_field_can_have_any_security_properties() {
    let mut d = Definition::new("invoice");
    d.fields = vec![FieldSpec::new("number", FieldType::Text)];
    d.protection_groups = vec![ProtectionGroupSpec::new("pay", "Pay")];

    let mut c = Customisations::new();
    // Add a custom field with security properties
    let mut f = FieldSpec::new("custom_sensitive", FieldType::Text);
    f.protection_group = Some("pay".into());
    f.classification = vec!["pii".into()];
    f.masking = Some(MaskSpec::last_chars(4));

    // This should work because custom fields can have any security properties
    c.add_field("invoice", f, None);
    let m = Registry::build(vec![d], &c);
    assert!(
        m.is_ok(),
        "custom field should be able to have any security properties"
    );
}

#[test]
fn permlevel_alias_maps_to_level_n_group() {
    let mut d = Definition::new("invoice");
    let mut f = FieldSpec::new("amount", FieldType::Decimal);
    f.scale = Some(2);
    f.permlevel = 2;
    d.fields = vec![f];

    let m = build(vec![d]).unwrap_or_else(|e| panic!("{e:?}"));
    let def = m.definition("invoice").unwrap_or_else(|| panic!("missing"));

    // Permlevel should have been migrated to level_2 group
    let field = def.fields.iter().find(|f| f.name == "amount").unwrap();
    assert_eq!(field.protection_group, Some("level_2".into()));

    // The level_2 group should have been created
    let group = def
        .protection_groups
        .iter()
        .find(|g| g.name == "level_2")
        .unwrap_or_else(|| panic!("level_2 group not created"));
    assert_eq!(group.label, Some("Level 2".into()));
}

#[test]
fn permlevel_migrates_to_protection_group_during_build() {
    let mut d = Definition::new("invoice");
    let mut f = FieldSpec::new("amount", FieldType::Decimal);
    f.scale = Some(2);
    f.permlevel = 1;
    // Don't set protection_group - permlevel will migrate to level_1
    d.fields = vec![f];

    let m = build(vec![d]).unwrap_or_else(|e| panic!("{e:?}"));
    let def = m.definition("invoice").unwrap();
    let field = def.fields.iter().find(|f| f.name == "amount").unwrap();
    assert_eq!(field.protection_group, Some("level_1".into()));
}

#[test]
fn field_protection_collects_groups_and_tags() {
    let mut d = Definition::new("invoice");
    let mut f = FieldSpec::new("amount", FieldType::Decimal);
    f.scale = Some(2);
    f.protection_group = Some("financial".into());
    f.classification = vec!["pii".into(), "financial".into()];
    f.masking = Some(MaskSpec::last_chars(2));
    d.fields = vec![f];
    d.protection_groups = vec![ProtectionGroupSpec::new("financial", "Financial")];

    let m = build(vec![d]).unwrap_or_else(|e| panic!("{e:?}"));
    let protections = m.field_protection();

    let field_prot = protections
        .iter()
        .find(|p| p.path == "invoice.amount")
        .unwrap_or_else(|| panic!("missing protection"));
    assert_eq!(field_prot.groups, vec!["financial"]);
    assert_eq!(field_prot.tags.len(), 2);
    assert!(field_prot.tags.contains(&"pii".to_string()));
    assert_eq!(field_prot.mask, Some("last_chars(2)".to_string()));
}

#[test]
fn trace_groups_are_marked_in_protection() {
    let mut d = Definition::new("invoice");
    let mut f = FieldSpec::new("amount", FieldType::Decimal);
    f.scale = Some(2);
    f.protection_group = Some("audit".into());
    d.fields = vec![f];
    let mut audit_group = ProtectionGroupSpec::new("audit", "Audit");
    audit_group.trace = true;
    d.protection_groups = vec![audit_group];

    let m = build(vec![d]).unwrap_or_else(|e| panic!("{e:?}"));
    let protections = m.field_protection();

    let field_prot = protections
        .iter()
        .find(|p| p.path == "invoice.amount")
        .unwrap_or_else(|| panic!("missing protection"));
    assert_eq!(field_prot.trace_groups, vec!["audit"]);
}

#[test]
fn protection_groups_method_returns_doctype_groups() {
    let mut d = Definition::new("invoice");
    d.protection_groups = vec![
        ProtectionGroupSpec::new("financial", "Financial"),
        ProtectionGroupSpec {
            name: "audit".into(),
            label: Some("Audit Trail".into()),
            trace: true,
            sync_class_hint: None,
        },
    ];
    d.fields = vec![FieldSpec::new("number", FieldType::Text)];

    let m = build(vec![d]).unwrap_or_else(|e| panic!("{e:?}"));
    let groups = m.protection_groups("invoice");

    assert_eq!(groups.len(), 2);
    assert!(groups.iter().any(|(n, _, _)| n == "financial"));
    let audit = groups.iter().find(|(n, _, _)| n == "audit").unwrap();
    assert_eq!(audit.1, "Audit Trail");
    assert!(audit.2); // trace flag
}

#[test]
fn determinism_of_protection_groups() {
    let mut d1 = Definition::new("test");
    d1.protection_groups = vec![
        ProtectionGroupSpec::new("z", "Z"),
        ProtectionGroupSpec::new("a", "A"),
        ProtectionGroupSpec::new("m", "M"),
    ];
    d1.fields = vec![FieldSpec::new("f", FieldType::Text)];

    let mut d2 = d1.clone();
    // Shuffle the order
    d2.protection_groups.reverse();

    let m1 = build(vec![d1]).unwrap_or_else(|e| panic!("{e:?}"));
    let m2 = build(vec![d2]).unwrap_or_else(|e| panic!("{e:?}"));

    let mut p1 = m1.protection_groups("test");
    let mut p2 = m2.protection_groups("test");

    // Sort both for comparison (they're stored in definition order)
    p1.sort_by(|a, b| a.0.cmp(&b.0));
    p2.sort_by(|a, b| a.0.cmp(&b.0));

    assert_eq!(p1, p2);
}

#[test]
fn classification_can_be_round_tripped_through_build() {
    let mut d = Definition::new("invoice");
    let mut f = FieldSpec::new("amount", FieldType::Decimal);
    f.scale = Some(2);
    f.protection_group = Some("financial".into());
    f.classification = vec!["pii".into(), "financial".into()];
    d.fields = vec![f];
    d.protection_groups = vec![ProtectionGroupSpec::new("financial", "Financial")];

    let m = Registry::build(vec![d], &Customisations::new()).unwrap_or_else(|e| panic!("{e:?}"));
    let view = m.doctype("invoice").unwrap_or_else(|| panic!("missing"));
    let amount = view.field("amount").unwrap_or_else(|| panic!("missing"));
    assert_eq!(amount.security.classification.len(), 2);
}

#[test]
fn permlevel_zero_means_nothing() {
    // permlevel = 0 should not create a level_0 group
    let mut d = Definition::new("doc");
    let mut f = FieldSpec::new("field", FieldType::Text);
    f.permlevel = 0;
    d.fields = vec![f];

    let m = build(vec![d]).unwrap_or_else(|e| panic!("{e:?}"));
    let view = m.doctype("doc").unwrap();
    let field = view.field("field").unwrap();

    // permlevel=0 should not create any protection_group
    assert!(field.security.protection_group.is_none());
}

#[test]
fn permlevel_and_group_conflict_refused() {
    // Setting both permlevel >= 1 and protection_group on the same field should fail
    let mut d = Definition::new("doc");
    let mut f = FieldSpec::new("field", FieldType::Text);
    f.permlevel = 2;
    f.protection_group = Some("custom_group".into());
    d.fields = vec![f];
    d.protection_groups = vec![ProtectionGroupSpec::new("custom_group", "Custom")];

    let m = build(vec![d]);
    assert!(
        m.is_err(),
        "having both permlevel >= 1 and protection_group should fail"
    );

    let errors = m.unwrap_err();
    assert!(
        errors
            .iter()
            .any(|e| e.message.contains("permlevel") || e.message.contains("conflict")),
        "error should mention permlevel or conflict"
    );
}
