//! The site masker: stored form masked from the served metadata, fail closed on unknown
//! DocTypes, one immutable instance per metadata generation.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_activity::FieldChange;
use spot_app_api::prelude::{OffsetDateTime, Value};
use spot_core::ManualClock;
use spot_doc_audit::{DocAuditError, FieldMasker};
use spot_field_guard::{AuditRecord, BoundedQueue, FieldAccessRecord, ProtectionSource};
use spot_meta::{
    BuildOptions, Definition, EffectiveMeta, FieldSpec, FieldType, MaskSpec, ProtectionGroupSpec,
    Registry,
};
use spot_permissions::{DocFacts, Model, ModelTexts, PermissionEngine, Principal, Snapshot, load};
use spot_site_masker::{GuardDeps, SiteMasker};

const DOCTYPE: &str = "invoice";
const CHILD: &str = "invoice_item";
const CARD: &str = "4111 1111 1111 1111";
const MEMO: &str = "call the supplier about the hidden rebate";
const PLACEHOLDER: &str = "***";
const IBAN: &str = "PK36SCBL0000001123456702";

fn grouped(name: &str, group: &str, mask: MaskSpec) -> FieldSpec {
    let mut f = FieldSpec::new(name, FieldType::Text);
    f.protection_group = Some(group.to_owned());
    f.masking = Some(mask);
    f
}

fn last4() -> MaskSpec {
    MaskSpec {
        placeholder: false,
        last_chars: Some(4),
    }
}

/// `memo_protected` is the one difference between two metadata generations.
fn definitions(memo_protected: bool) -> Vec<Definition> {
    let mut invoice = Definition::new(DOCTYPE);
    let mut items = FieldSpec::new("items", FieldType::Table);
    items.options = Some(CHILD.into());
    let memo = if memo_protected {
        grouped("memo", "pci", MaskSpec::placeholder())
    } else {
        FieldSpec::new("memo", FieldType::Text)
    };
    invoice.fields = vec![
        FieldSpec::new("number", FieldType::Text),
        grouped("card_number", "pci", last4()),
        grouped("holder", "pci", MaskSpec::placeholder()),
        memo,
        items,
    ];
    invoice.protection_groups = vec![ProtectionGroupSpec::new("pci", "Payment cards")];
    let mut item = Definition::new(CHILD);
    item.is_child = true;
    item.fields = vec![
        FieldSpec::new("qty", FieldType::Text),
        grouped("iban", "bank", last4()),
    ];
    item.protection_groups = vec![ProtectionGroupSpec::new("bank", "Bank")];
    vec![invoice, item]
}

fn meta(memo_protected: bool) -> Arc<EffectiveMeta> {
    Registry::with_options(definitions(memo_protected), BuildOptions::new())
        .unwrap_or_else(|e| panic!("fixture meta: {e:?}"))
        .effective()
}

fn model() -> Model {
    let mut grants = String::new();
    let mut field_grants = String::new();
    for role in ["Reader", "MaskedReader"] {
        for doctype in [DOCTYPE, CHILD] {
            grants.push_str(&format!(
                "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\nrights = [\"read\"]\n"
            ));
        }
        let access = if role == "Reader" {
            "read"
        } else {
            "read_masked"
        };
        for (doctype, group) in [(DOCTYPE, "pci"), (CHILD, "bank")] {
            field_grants.push_str(&format!(
                "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\ntarget = {{ group = \"{group}\" }}\naccess = [\"{access}\"]\n"
            ));
        }
    }
    load(&ModelTexts {
        grants,
        field_grants,
        groups: "[[entries]]\nname = \"pci\"\n[[entries]]\nname = \"bank\"\n".to_owned(),
        shares: String::new(),
    })
    .unwrap_or_else(|e| panic!("fixture model: {:?}", e.as_slice()))
}

fn deps_for(meta: &EffectiveMeta) -> GuardDeps {
    let facts = spot_field_guard::MetaProtection::new(meta);
    let mut snap = Snapshot::build(meta, &model()).unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    for doctype in [DOCTYPE, CHILD] {
        let prot = facts.protection(doctype).unwrap();
        snap = snap
            .with_field_facts(doctype, prot.fields.clone())
            .unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
    }
    GuardDeps {
        engine: Arc::new(PermissionEngine::new(Arc::new(snap))),
        trace: Arc::new(BoundedQueue::<FieldAccessRecord>::new(64)),
        audit: Arc::new(BoundedQueue::<AuditRecord>::new(64)),
        clock: Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH)),
    }
}

fn masker(memo_protected: bool) -> SiteMasker {
    let meta = meta(memo_protected);
    SiteMasker::new(&meta, deps_for(&meta))
}

fn text(s: &str) -> Option<Value> {
    Some(Value::Text(s.to_owned()))
}

fn change(path: &str, before: Option<Value>, after: Option<Value>) -> FieldChange {
    FieldChange {
        path: path.to_owned(),
        before,
        after,
    }
}

fn serialised(changes: &[FieldChange]) -> String {
    serde_json::to_string(changes).unwrap()
}

const ROW: &str = "items[id=0190c0a0-0000-7000-8000-000000000001]";

#[test]
fn site_masker_is_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<SiteMasker>();
    let _: &dyn FieldMasker = &masker(false);
}

#[test]
fn protected_fields_are_stored_masked_and_plain_ones_pass_unchanged() {
    let m = masker(false);
    let plain = change("number", text("INV-1"), text("INV-2"));
    let out = m
        .mask_for_audit(
            DOCTYPE,
            &[
                change("card_number", text(CARD), text("5500 0000 0000 0004")),
                change("holder", None, text("Ayesha Khan")),
                plain.clone(),
            ],
        )
        .unwrap();
    assert!(!out.hidden_changes);
    assert_eq!(out.changes.len(), 3);
    assert_eq!(out.changes[0].before, text("***************1111"));
    assert_eq!(out.changes[0].after, text("***************0004"));
    assert_eq!(out.changes[1].after, text(PLACEHOLDER));
    assert_eq!(out.changes[2], plain);
    let wire = serialised(&out.changes);
    for raw in [CARD, "5500 0000 0000 0004", "Ayesha Khan"] {
        assert!(!wire.contains(raw), "raw value reached the stored diff");
    }
}

#[test]
fn a_child_column_is_judged_on_the_child_doctype() {
    let m = masker(false);
    let path = format!("{ROW}.iban");
    let out = m
        .mask_for_audit(
            DOCTYPE,
            &[change(&path, text(IBAN), text("PK00NEW0000000000000001"))],
        )
        .unwrap();
    assert!(!out.hidden_changes);
    assert_eq!(out.changes.len(), 1);
    assert_eq!(out.changes[0].before, text("********************6702"));
    assert!(!serialised(&out.changes).contains(IBAN));
    // an unprotected child column passes
    let qty = format!("{ROW}.qty");
    let out = m
        .mask_for_audit(DOCTYPE, &[change(&qty, text("1"), text("2"))])
        .unwrap();
    assert_eq!(out.changes, vec![change(&qty, text("1"), text("2"))]);
}

#[test]
fn a_row_added_or_removed_has_its_protected_columns_masked() {
    let m = masker(false);
    let snapshot = format!(r#"{{"fields":{{"qty":"3","iban":"{IBAN}"}}}}"#);
    let added = change(ROW, None, Some(Value::Json(snapshot.clone())));
    let removed = change(ROW, Some(Value::Json(snapshot)), None);
    let out = m.mask_for_audit(DOCTYPE, &[added, removed]).unwrap();
    assert!(!out.hidden_changes);
    assert_eq!(out.changes.len(), 2);
    let wire = serialised(&out.changes);
    assert!(!wire.contains(IBAN));
    assert!(wire.contains("6702"));
    assert!(wire.contains("qty"));
}

#[test]
fn a_change_under_a_table_the_doctype_does_not_have_is_dropped_and_flagged() {
    let m = masker(false);
    let out = m
        .mask_for_audit(
            DOCTYPE,
            &[
                change(
                    "ghost[id=0190c0a0-0000-7000-8000-000000000009].iban",
                    text(IBAN),
                    None,
                ),
                change("number", None, text("INV-9")),
            ],
        )
        .unwrap();
    assert!(out.hidden_changes);
    assert_eq!(out.changes.len(), 1);
    assert_eq!(out.changes[0].path, "number");
    assert!(!serialised(&out.changes).contains(IBAN));
}

#[test]
fn an_unknown_doctype_is_refused_without_a_value() {
    let m = masker(false);
    let err = m
        .mask_for_audit("ghost", &[change("secret", text(CARD), text(MEMO))])
        .unwrap_err();
    assert!(matches!(err, DocAuditError::UnknownDoctype(ref d) if d == "ghost"));
    for text in [err.to_string(), format!("{err:?}")] {
        assert!(!text.contains(CARD) && !text.contains(MEMO));
    }
}

#[test]
fn a_field_with_no_facts_is_unprotected_by_definition() {
    let m = masker(false);
    let prot = m.source().protection(DOCTYPE).unwrap();
    // memo carries no groups and no mask, and a name the snapshot never lists has no facts at all
    let memo = prot.field("memo");
    assert!(memo.groups.is_empty() && memo.mask.is_none());
    assert!(!prot.protected_names().contains("never_declared"));
    assert!(!prot.field("card_number").groups.is_empty());
    let out = m
        .mask_for_audit(DOCTYPE, &[change("never_declared", None, text(MEMO))])
        .unwrap();
    assert_eq!(out.changes[0].after, text(MEMO));
    let out = m
        .mask_for_audit(DOCTYPE, &[change("memo", None, text(MEMO))])
        .unwrap();
    assert_eq!(out.changes[0].after, text(MEMO));
}

#[test]
fn a_swap_changes_what_is_masked_and_the_old_instance_keeps_its_facts() {
    let old = masker(false);
    let new = masker(true);
    let edit = change("memo", None, text(MEMO));

    let a = old
        .mask_for_audit(DOCTYPE, std::slice::from_ref(&edit))
        .unwrap();
    assert_eq!(
        a.changes[0].after,
        text(MEMO),
        "metadata A leaves memo open"
    );

    let b = new
        .mask_for_audit(DOCTYPE, std::slice::from_ref(&edit))
        .unwrap();
    assert_eq!(b.changes[0].after, text(PLACEHOLDER));
    assert!(!serialised(&b.changes).contains(MEMO));

    // a request still holding the old instance is unaffected by the new one
    let again = old.mask_for_audit(DOCTYPE, &[edit]).unwrap();
    assert_eq!(again.changes[0].after, text(MEMO));
}

#[test]
fn redact_diff_goes_through_the_guard() {
    let m = masker(false);
    let facts = DocFacts {
        doctype: DOCTYPE.to_owned(),
        ..DocFacts::default()
    };
    let changes = [change(
        "card_number",
        text(CARD),
        text("5500 0000 0000 0004"),
    )];
    let masked = m
        .redact_diff(
            &Principal::with_roles("m", &["MaskedReader"]),
            DOCTYPE,
            &facts,
            &changes,
        )
        .unwrap();
    assert_eq!(masked.changes[0].before, text("***************1111"));
    assert!(!serialised(&masked.changes).contains(CARD));
    let open = m
        .redact_diff(
            &Principal::with_roles("r", &["Reader"]),
            DOCTYPE,
            &facts,
            &changes,
        )
        .unwrap();
    assert_eq!(open.changes, changes.to_vec());
}

#[test]
fn can_read_goes_through_the_guard_engine() {
    let m = masker(false);
    let facts = DocFacts {
        doctype: DOCTYPE.to_owned(),
        ..DocFacts::default()
    };
    assert!(m.can_read(&Principal::with_roles("r", &["Reader"]), DOCTYPE, &facts));
    assert!(!m.can_read(&Principal::with_roles("n", &[]), DOCTYPE, &facts));
}
