//! Protection end to end: upward-only site overrides, computed inheritance, the derived lint.
#![allow(missing_docs, clippy::unwrap_used)]

use spot_meta::{
    BuildOptions, Customisations, Definition, Derivation, EffectiveMeta, FieldRef, FieldSpec,
    FieldType, MaskSpec, MetaError, Property, ProtectionGroupSpec, Registry,
};

fn decimal(name: &str) -> FieldSpec {
    let mut f = FieldSpec::new(name, FieldType::Decimal);
    f.scale = Some(2);
    f
}

/// amount (maybe protected) -> total -> subtotal.
fn invoice(amount_group: Option<&str>) -> Definition {
    let mut amount = decimal("amount");
    amount.protection_group = amount_group.map(str::to_owned);
    let mut d = Definition::new("invoice");
    d.fields = vec![amount, decimal("total"), decimal("subtotal")];
    if amount_group.is_some() {
        d.protection_groups = vec![ProtectionGroupSpec::new("financial", "Financial")];
    }
    d
}

fn chain() -> Vec<Derivation> {
    vec![
        Derivation::new(
            FieldRef::new("invoice", "total"),
            vec![FieldRef::new("invoice", "amount")],
        ),
        Derivation::new(
            FieldRef::new("invoice", "subtotal"),
            vec![FieldRef::new("invoice", "total")],
        ),
    ]
}

fn build(
    defs: Vec<Definition>,
    c: &Customisations,
    derivations: &[Derivation],
) -> Result<EffectiveMeta, Vec<MetaError>> {
    let derivations = derivations.to_vec();
    Registry::build_with(defs, c, &BuildOptions::new().with_derivations(&derivations))
}

fn inherited(m: &EffectiveMeta, field: &str) -> Vec<String> {
    m.doctype("invoice")
        .unwrap()
        .field(field)
        .unwrap()
        .security
        .inherited_groups
        .iter()
        .map(|g| g.as_str().to_owned())
        .collect()
}

fn messages(e: &[MetaError]) -> String {
    e.iter()
        .map(|e| e.message.clone())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn derived_unprotected_names_both_fixes() {
    let errors = build(
        vec![invoice(Some("financial"))],
        &Customisations::new(),
        &chain(),
    )
    .unwrap_err();
    let text = messages(&errors);
    assert!(text.contains("meta.derived_unprotected"), "{text}");
    assert!(
        text.contains("invoice.amount") && text.contains("`financial`"),
        "{text}"
    );
    assert!(
        text.contains("protection_group") && text.contains("declassified"),
        "{text}"
    );
    assert!(errors.iter().any(|e| e.field.as_deref() == Some("total")));
}

#[test]
fn derived_field_in_an_input_group_passes_the_lint() {
    let mut d = invoice(Some("financial"));
    d.fields[1].protection_group = Some("financial".into());
    d.fields[2].protection_group = Some("financial".into());
    let m = build(vec![d], &Customisations::new(), &chain()).unwrap();
    // Its own group is never listed as inherited.
    assert!(inherited(&m, "total").is_empty());
}

#[test]
fn declassified_removes_inheritance_and_is_listed() {
    let mut d = invoice(Some("financial"));
    d.fields[1].declassified = Some("public total, reviewed 2026-10".into());
    d.fields[2].protection_group = Some("financial".into());
    let m = build(vec![d], &Customisations::new(), &chain()).unwrap();
    assert!(inherited(&m, "total").is_empty());
    assert_eq!(
        m.declassifications(),
        &[(
            "invoice.total".to_owned(),
            "public total, reviewed 2026-10".to_owned()
        )]
    );
    let total = m
        .field_protection()
        .into_iter()
        .find(|p| p.path == "invoice.total")
        .unwrap();
    assert!(total.groups.is_empty());
}

#[test]
fn propagation_two_steps_downstream() {
    // The app declares no protection at all: nothing to lint. The site protects the input.
    let mut c = Customisations::new();
    c.set(
        "invoice",
        Some("amount"),
        Property::ProtectionGroup(Some("custom_pay".into())),
    );
    let m = build(vec![invoice(None)], &c, &chain()).unwrap();

    assert_eq!(inherited(&m, "total"), ["custom_pay"]);
    assert_eq!(
        inherited(&m, "subtotal"),
        ["custom_pay"],
        "two steps downstream"
    );
    assert!(inherited(&m, "amount").is_empty());
    let subtotal = m
        .field_protection()
        .into_iter()
        .find(|p| p.path == "invoice.subtotal")
        .unwrap();
    assert_eq!(subtotal.groups, ["custom_pay"]);
    assert!(
        m.protection_groups("invoice")
            .iter()
            .any(|g| g.0 == "custom_pay")
    );

    // Clearing the site override restores the unprotected state at the next build.
    c.clear("invoice", Some("amount"), "protection_group");
    let m = build(vec![invoice(None)], &c, &chain()).unwrap();
    assert!(inherited(&m, "subtotal").is_empty());
}

#[test]
fn derivation_cycle_refused_with_path() {
    let cycle = vec![
        Derivation::new(
            FieldRef::new("invoice", "total"),
            vec![FieldRef::new("invoice", "subtotal")],
        ),
        Derivation::new(
            FieldRef::new("invoice", "subtotal"),
            vec![FieldRef::new("invoice", "total")],
        ),
    ];
    let text = messages(&build(vec![invoice(None)], &Customisations::new(), &cycle).unwrap_err());
    assert!(text.contains("meta.derivation_cycle"), "{text}");
    // The path starts at the smallest field name, deterministically, and closes the loop.
    assert!(
        text.contains("invoice.subtotal -> invoice.total -> invoice.subtotal"),
        "{text}"
    );
}

#[test]
fn site_cannot_remove_or_change_an_app_group() {
    let mut c = Customisations::new();
    c.set("invoice", Some("amount"), Property::ProtectionGroup(None));
    let text = messages(&build(vec![invoice(Some("financial"))], &c, &[]).unwrap_err());
    assert!(
        text.contains("cannot remove protection group `financial`"),
        "{text}"
    );

    let mut c = Customisations::new();
    c.set(
        "invoice",
        Some("amount"),
        Property::ProtectionGroup(Some("custom_other".into())),
    );
    let text = messages(&build(vec![invoice(Some("financial"))], &c, &[]).unwrap_err());
    assert!(text.contains("cannot change protection group"), "{text}");
}

#[test]
fn site_cannot_remove_an_app_tag_but_can_add_one() {
    let mut d = invoice(Some("financial"));
    d.fields[0].classification = vec!["pii".into()];
    let mut c = Customisations::new();
    c.set(
        "invoice",
        Some("amount"),
        Property::Classification(vec!["financial".into()]),
    );
    let text = messages(&build(vec![d.clone()], &c, &[]).unwrap_err());
    assert!(
        text.contains("cannot remove classification tag `pii`"),
        "{text}"
    );

    let mut c = Customisations::new();
    c.set(
        "invoice",
        Some("amount"),
        Property::Classification(vec!["pii".into(), "financial".into()]),
    );
    build(vec![d], &c, &[]).unwrap();
}

#[test]
fn site_cannot_widen_a_mask() {
    let mut d = invoice(Some("financial"));
    d.fields[0].masking = Some(MaskSpec {
        placeholder: false,
        last_chars: Some(2),
    });
    let set = |m: Option<MaskSpec>| {
        let mut c = Customisations::new();
        c.set("invoice", Some("amount"), Property::Masking(m));
        build(vec![d.clone()], &c, &[])
    };
    let text = messages(
        &set(Some(MaskSpec {
            placeholder: false,
            last_chars: Some(4),
        }))
        .unwrap_err(),
    );
    assert!(text.contains("cannot widen masking"), "{text}");
    assert!(set(None).is_err(), "removing a mask is refused");
    set(Some(MaskSpec {
        placeholder: false,
        last_chars: Some(1),
    }))
    .unwrap();
    set(Some(MaskSpec {
        placeholder: true,
        last_chars: None,
    }))
    .unwrap();
}

#[test]
fn site_can_add_custom_group_and_tags_to_an_unprotected_field() {
    let mut c = Customisations::new();
    c.set(
        "invoice",
        Some("total"),
        Property::ProtectionGroup(Some("custom_hr".into())),
    );
    c.set(
        "invoice",
        Some("total"),
        Property::Classification(vec!["pii".into()]),
    );
    let m = build(vec![invoice(None)], &c, &[]).unwrap();
    assert!(
        m.protection_groups("invoice")
            .iter()
            .any(|g| g.0 == "custom_hr")
    );
    let p = m
        .field_protection()
        .into_iter()
        .find(|p| p.path == "invoice.total")
        .unwrap();
    assert_eq!(p.groups, ["custom_hr"]);
    assert_eq!(p.tags, ["pii"]);

    // A group that is neither declared by the app nor `custom_*` does not exist.
    let mut c = Customisations::new();
    c.set(
        "invoice",
        Some("total"),
        Property::ProtectionGroup(Some("secret_stuff".into())),
    );
    let text = messages(&build(vec![invoice(None)], &c, &[]).unwrap_err());
    assert!(text.contains("meta.unknown_group"), "{text}");
}

#[test]
fn site_created_field_has_free_security() {
    let mut c = Customisations::new();
    let mut f = decimal("custom_bonus");
    f.protection_group = Some("custom_pay".into());
    f.classification = vec!["pii".into()];
    c.add_field("invoice", f, None);
    // Declare the group, change it and drop the tag: all allowed on the site's own field.
    c.set(
        "invoice",
        Some("custom_bonus"),
        Property::ProtectionGroup(Some("custom_pay".into())),
    );
    c.set(
        "invoice",
        Some("custom_bonus"),
        Property::Classification(vec![]),
    );
    c.set(
        "invoice",
        Some("custom_bonus"),
        Property::ProtectionGroup(None),
    );
    let m = build(vec![invoice(None)], &c, &[]).unwrap();
    let p = m
        .field_protection()
        .into_iter()
        .find(|p| p.path == "invoice.custom_bonus")
        .unwrap();
    assert!(p.groups.is_empty() && p.tags.is_empty());
}

#[test]
fn output_is_deterministic_under_shuffled_input() {
    let mut c = Customisations::new();
    c.set(
        "invoice",
        Some("amount"),
        Property::ProtectionGroup(Some("custom_pay".into())),
    );
    let forward = chain();
    let mut backward = chain();
    backward.reverse();
    let a = build(vec![invoice(None)], &c, &forward).unwrap();
    let b = build(vec![invoice(None)], &c, &backward).unwrap();
    assert_eq!(a.field_protection(), b.field_protection());
    assert_eq!(a.doctype("invoice"), b.doctype("invoice"));
}

#[test]
fn inherited_groups_cannot_be_authored() {
    let text = r#"
name = "invoice"
[[fields]]
name = "amount"
type = "decimal"
scale = 2
inherited_groups = ["financial"]
"#;
    let e = Definition::parse("invoice.toml", text).unwrap_err();
    assert!(e.message.contains("inherited_groups"), "{}", e.message);
}
