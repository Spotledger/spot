//! The declared fields, the protect transform and erase_subject, with in-memory keys.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use serde_json::json;
use spot_journal_pii::{CounterNonces, KeyHandle, MemoryKeys, SubjectKeys};
use spot_journal_privacy::{
    AdminAction, AdminAudit, CatalogueFields, ERASE_PERMISSION, ErasePermission, Eraser, FieldDecl,
    Privacy, PrivacyError, RefusingProtector, SubjectHasher,
};
use spot_journal_protect::{SharedKeys, SharedNonces};
use spot_journal_ship::Protector;

const PERSON: &str = "01900000-0000-7000-8000-0000000000aa";

fn decl(entity: &str, path: &str, class: &str, erasable: bool) -> FieldDecl {
    FieldDecl {
        entity: entity.to_owned(),
        path: path.to_owned(),
        subject: Some("person".to_owned()),
        purpose: "notifications".to_owned(),
        retention_class: class.to_owned(),
        erasable,
        secret: false,
    }
}

fn no_classes() -> BTreeSet<String> {
    BTreeSet::new()
}

fn keys(seed: u8) -> (Arc<MemoryKeys>, SharedKeys) {
    let memory = Arc::new(MemoryKeys::new(KeyHandle::new([seed; 32])));
    memory.insert(PERSON, KeyHandle::new([seed.wrapping_add(1); 32]));
    let shared: SharedKeys = Arc::<MemoryKeys>::clone(&memory);
    (memory, shared)
}

fn nonces() -> SharedNonces {
    Arc::new(Mutex::new(CounterNonces::new()))
}

fn contact_line(email: &str) -> Vec<u8> {
    json!({
        "event_id": "01900000-0000-7000-8000-000000100001",
        "topic": "doc_state",
        "schema_version": 1,
        "source_id": "acme.example",
        "position": {"generation": 1, "seq": 1},
        "transaction": "01900000-0000-7000-8000-000000200001",
        "ordinal": 0,
        "subject": {"entity": "contact", "id": PERSON},
        "subject_version": 1,
        "meta": {
            "occurred_at": "2026-01-01T00:00:00Z",
            "actor": {"kind": "system", "component": "scheduler"},
            "correlation_id": "01900000-0000-7000-8000-000000300001",
        },
        "data": {"op": "insert", "entity_schema": 1,
                 "image": {"person": PERSON, "email": email, "tax_id": "TAX-1", "city": "Leeds"}},
    })
    .to_string()
    .into_bytes()
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[test]
fn a_field_flagged_at_runtime_is_protected_from_the_next_line() {
    let fields = CatalogueFields::new(&[], &no_classes()).unwrap();
    let (_, shared) = keys(1);
    let privacy = Privacy::new(Arc::clone(&fields), shared, nonces());
    let protector = privacy.protector();
    assert!(!fields.has_personal_fields());
    let before = protector.protect(&contact_line("a@mail.example")).unwrap();
    assert!(
        contains(&before, "a@mail.example"),
        "no personal fields declared yet"
    );
    fields
        .replace(
            &[decl("contact", "email", "contact_5y", true)],
            &no_classes(),
        )
        .unwrap();
    assert!(fields.has_personal_fields());
    let after = protector.protect(&contact_line("a@mail.example")).unwrap();
    assert!(
        !contains(&after, "a@mail.example"),
        "sealed from the next line on"
    );
    assert!(contains(&after, "Leeds"), "other fields stay as they are");
}

#[test]
fn a_personal_field_without_a_subject_refuses_the_set_and_keeps_the_old_one() {
    let good = [decl("contact", "email", "contact_5y", true)];
    let fields = CatalogueFields::new(&good, &no_classes()).unwrap();
    let mut bad = decl("contact", "phone", "contact_5y", true);
    bad.subject = None;
    let error = fields
        .replace(&[good[0].clone(), bad.clone()], &no_classes())
        .unwrap_err();
    assert_eq!(error.code(), "journal_privacy.subject_missing");
    assert!(
        error.to_string().contains("contact.phone"),
        "names the field"
    );
    assert_eq!(fields.declared().len(), 1, "the old set is still in force");
    bad.subject = Some("  ".to_owned());
    assert!(matches!(
        CatalogueFields::new(&[bad], &no_classes()).err(),
        Some(PrivacyError::SubjectMissing { .. })
    ));
}

struct Allow(Vec<&'static str>);

impl ErasePermission for Allow {
    fn may_erase(&self, requester: &str) -> bool {
        self.0.contains(&requester)
    }
}

struct Hash;

impl SubjectHasher for Hash {
    fn hash(&self, subject: &str) -> String {
        format!("h{}", subject.len())
    }
}

#[derive(Default)]
struct Records(Mutex<Vec<(AdminAction, String, String)>>);

impl AdminAudit for Records {
    fn record(
        &self,
        action: &AdminAction,
        hash: &str,
        requester: &str,
    ) -> Result<(), &'static str> {
        self.0
            .lock()
            .unwrap()
            .push((action.clone(), hash.to_owned(), requester.to_owned()));
        Ok(())
    }
}

fn eraser(fields: &Arc<CatalogueFields>, shared: SharedKeys, audit: &Arc<Records>) -> Eraser {
    Eraser::new(
        Arc::clone(fields),
        shared,
        Arc::new(Allow(vec!["dpo"])),
        Arc::new(Hash),
        audit.clone(),
    )
}

#[test]
fn erase_respects_retention_classes_and_says_when_it_is_partial() {
    let forbidden: BTreeSet<String> = ["tax_10y".to_owned()].into_iter().collect();
    let fields = CatalogueFields::new(
        &[
            decl("contact", "email", "contact_5y", true),
            decl("contact", "tax_id", "tax_10y", true),
        ],
        &forbidden,
    )
    .unwrap();
    let (memory, shared) = keys(3);
    let audit = Arc::new(Records::default());
    let outcome = eraser(&fields, shared, &audit)
        .erase_subject(PERSON, "dpo")
        .unwrap();
    assert_eq!(outcome.erased_fields, 1);
    assert!(outcome.is_partial());
    assert_eq!(outcome.retained.len(), 1);
    assert_eq!(
        outcome.retained[0].path, "tax_id",
        "names the field, not a value"
    );
    assert!(
        memory.key_for(PERSON).unwrap().is_none(),
        "the key is destroyed"
    );
    let records = audit.0.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].0,
        AdminAction::SubjectErased { retained_fields: 1 }
    );
    assert_ne!(
        records[0].1, PERSON,
        "the audit target is a hash, never the subject"
    );
}

#[test]
fn a_subject_whose_every_field_is_retained_is_refused_and_audited() {
    let forbidden: BTreeSet<String> = ["tax_10y".to_owned()].into_iter().collect();
    let fields =
        CatalogueFields::new(&[decl("contact", "tax_id", "tax_10y", true)], &forbidden).unwrap();
    let (memory, shared) = keys(5);
    let audit = Arc::new(Records::default());
    let error = eraser(&fields, shared, &audit)
        .erase_subject(PERSON, "dpo")
        .unwrap_err();
    assert_eq!(error.code(), "journal_privacy.retention_forbids");
    assert!(error.to_string().contains("contact.tax_id"));
    assert!(
        memory.key_for(PERSON).unwrap().is_some(),
        "nothing was destroyed"
    );
    assert!(matches!(
        audit.0.lock().unwrap()[0].0,
        AdminAction::SubjectEraseRefused { .. }
    ));
}

#[test]
fn an_unprivileged_requester_is_refused_and_the_refusal_is_audited() {
    let fields = CatalogueFields::new(
        &[decl("contact", "email", "contact_5y", true)],
        &no_classes(),
    )
    .unwrap();
    let (memory, shared) = keys(7);
    let audit = Arc::new(Records::default());
    let error = eraser(&fields, shared, &audit)
        .erase_subject(PERSON, "clerk")
        .unwrap_err();
    assert_eq!(error, PrivacyError::PermissionDenied);
    assert!(memory.key_for(PERSON).unwrap().is_some());
    let records = audit.0.lock().unwrap().clone();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].2, "clerk");
    assert!(
        matches!(records[0].0, AdminAction::SubjectEraseRefused { code } if code == "journal_privacy.permission_denied")
    );
    assert_eq!(ERASE_PERMISSION, "journal.erase_subject");
}

#[test]
fn erasing_a_subject_in_one_site_leaves_the_same_id_in_another_site() {
    let fields = CatalogueFields::new(
        &[decl("contact", "email", "contact_5y", true)],
        &no_classes(),
    )
    .unwrap();
    let (site_a, shared_a) = keys(10);
    let (site_b, _) = keys(20);
    let audit = Arc::new(Records::default());
    eraser(&fields, shared_a, &audit)
        .erase_subject(PERSON, "dpo")
        .unwrap();
    assert!(site_a.key_for(PERSON).unwrap().is_none());
    assert!(
        site_b.key_for(PERSON).unwrap().is_some(),
        "separate key spaces per tenant"
    );
}

#[test]
fn the_refusing_protector_writes_nothing() {
    let error = RefusingProtector
        .protect(&contact_line("a@mail.example"))
        .unwrap_err();
    assert_eq!(error.code(), "journal_ship.protect_failed");
    assert!(
        error
            .to_string()
            .contains("journal_privacy.protection_missing")
    );
}

#[test]
fn an_empty_subject_is_refused_before_anything_else() {
    let fields = CatalogueFields::new(&[], &no_classes()).unwrap();
    let (_, shared) = keys(1);
    let audit = Arc::new(Records::default());
    let error = eraser(&fields, shared, &audit)
        .erase_subject("", "dpo")
        .unwrap_err();
    assert_eq!(error, PrivacyError::SubjectInvalid);
    assert!(audit.0.lock().unwrap().is_empty());
}
