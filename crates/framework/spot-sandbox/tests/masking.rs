//! Masking: classification-driven selection, deterministic pseudonyms, preserved shapes,
//! untouched amounts, scrubbed text, replaced attachments, purged copies and stripped secrets.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common as fake;

use serde_json::{Value, json};
use spot_sandbox::{
    MaskReport, Masker, MaskingEngine, MaskingPolicy, SandboxData, SandboxKey, SandboxStore,
};

fn run(key: SandboxKey) -> (SandboxData, MaskReport) {
    let classes = fake::sample_classes();
    let policy = MaskingPolicy::default();
    let engine = MaskingEngine::new(&classes, &policy);
    let mut data = fake::sample();
    let outcome = engine.mask(&mut data, key).unwrap();
    (data, outcome.report)
}

fn masked() -> (SandboxData, MaskReport) {
    run(fake::sample_key())
}

fn text<'a>(data: &'a SandboxData, id: &str, field: &str) -> &'a str {
    data.by_id(id)
        .and_then(|doc| doc.fields.get(field))
        .and_then(Value::as_str)
        .unwrap()
}

#[test]
fn pii_fields_become_pseudonyms_not_values() {
    let (data, report) = masked();
    assert_ne!(text(&data, "c-1", "name"), fake::CUSTOMER_NAME);
    assert_ne!(text(&data, "c-1", "email"), fake::CUSTOMER_EMAIL);
    assert_ne!(text(&data, "c-1", "phone"), fake::CUSTOMER_PHONE);
    assert_ne!(text(&data, "c-1", "tax_id"), fake::CUSTOMER_TAX_ID);
    assert_ne!(text(&data, "c-1", "bank_account"), fake::CUSTOMER_IBAN);
    assert!(text(&data, "c-1", "name").starts_with("p-"));
    assert!(text(&data, "c-1", "email").ends_with("@example.invalid"));
    assert!(report.fields_masked >= 6);
    assert_eq!(report.documents, 2);
}

#[test]
fn the_same_value_masks_the_same_way_everywhere() {
    let (data, report) = masked();
    let name = text(&data, "c-1", "name");
    assert_eq!(text(&data, "inv-1", "customer_name"), name);

    let email = text(&data, "c-1", "email");
    let journal = data
        .journal
        .iter()
        .find(|row| row.field == "email")
        .unwrap();
    assert_eq!(journal.value, json!(email));
    let journal_before = data
        .journal
        .iter()
        .find(|row| row.field == "email")
        .unwrap();
    assert_ne!(journal_before.value, json!(fake::CUSTOMER_EMAIL));

    let audit = &data.audit[0];
    assert_eq!(audit.after, json!(email));
    assert_ne!(audit.before, json!("alice.old@example.com"));
    assert_ne!(audit.before, json!(fake::CUSTOMER_EMAIL));
    assert!(report.derived_masked >= 1);
}

#[test]
fn a_different_key_gives_a_different_fake() {
    let (first, _) = masked();
    let (second, _) = run(SandboxKey::new([9u8; 32]));
    assert_ne!(text(&first, "c-1", "name"), text(&second, "c-1", "name"));
    assert_ne!(text(&first, "c-1", "email"), text(&second, "c-1", "email"));
}

#[test]
fn shapes_are_preserved() {
    let (data, _) = masked();
    let email = text(&data, "c-1", "email");
    assert!(email.contains('@'));
    assert_eq!(email.matches('@').count(), 1);

    let phone = text(&data, "c-1", "phone");
    assert!(phone.starts_with('+'));
    assert_eq!(
        phone.chars().filter(char::is_ascii_digit).count(),
        fake::CUSTOMER_PHONE
            .chars()
            .filter(char::is_ascii_digit)
            .count()
    );
    assert_ne!(phone, fake::CUSTOMER_PHONE);

    let iban = text(&data, "c-1", "bank_account");
    assert!(iban.starts_with("GB"));
    assert_eq!(iban.len(), fake::CUSTOMER_IBAN.len());
    assert!(iban_valid(iban), "the masked IBAN must pass a mod-97 check");
}

#[test]
fn amounts_are_kept_and_the_ledger_balances() {
    let (data, report) = masked();
    assert_eq!(
        data.by_id("c-1").unwrap().fields["credit_limit"],
        json!(fake::CREDIT_LIMIT)
    );
    assert_eq!(
        data.by_id("inv-1").unwrap().fields["total"],
        json!(fake::INVOICE_TOTAL)
    );

    let sum = |rows: &[spot_sandbox::JournalRow]| -> i64 {
        rows.iter()
            .filter(|row| row.doctype == "gl_entry")
            .filter_map(|row| row.value.as_i64())
            .sum()
    };
    assert_eq!(sum(&fake::sample().journal), 0);
    assert_eq!(sum(&data.journal), 0);
    assert!(report.amounts_kept >= 3);
    assert!(report.ledger_checked);
}

#[test]
fn free_text_is_scrubbed_to_hash_tokens() {
    let (data, report) = masked();
    let notes = text(&data, "c-1", "notes");
    assert!(notes.starts_with("h:"));
    for word in ["Customer", "prefers", "Tuesday", "e-mail"] {
        assert!(!notes.contains(word), "{word} survived the scrub");
    }
    assert!(report.free_text_scrubbed >= 1);
}

#[test]
fn attachments_become_placeholders() {
    let (data, _) = masked();
    let attachment = &data.attachments[0];
    assert_ne!(attachment.filename, fake::ATTACHMENT_NAME);
    assert!(attachment.filename.starts_with("file-"));
    assert_eq!(
        attachment.content,
        spot_sandbox::plan::DEFAULT_ATTACHMENT_PLACEHOLDER
    );
    assert_eq!(attachment.size, 0);
    assert_ne!(attachment.content_hash, "f".repeat(64));
    assert_eq!(attachment.id, "f-1");
    // The document keeps the reference to the (now placeholder) file.
    assert_eq!(text(&data, "c-1", "avatar"), "f-1");
}

#[test]
fn copies_caches_secrets_and_integrations_are_gone() {
    let (data, report) = masked();
    assert!(data.search.is_empty());
    assert!(data.caches.is_empty());
    assert!(data.secrets.is_empty());
    assert!(data.integrations.is_empty());
    assert_eq!(report.search_rows_purged, 2);
    assert_eq!(report.caches_purged, 1);
    assert_eq!(report.secrets_stripped, 1);
    assert_eq!(report.integrations_stripped, 1);
}

#[test]
fn a_secret_document_field_is_nulled() {
    let (data, _) = masked();
    assert_eq!(data.by_id("c-1").unwrap().fields["api_key"], Value::Null);
}

#[test]
fn links_keep_their_record_ids() {
    let (data, report) = masked();
    assert_eq!(text(&data, "inv-1", "customer"), "c-1");
    assert_eq!(data.by_id("c-1").unwrap().id, "c-1");
    assert!(report.links_kept >= 2);
}

fn run_keeping(keep: &[(&str, &str)]) -> SandboxData {
    let classes = fake::sample_classes();
    let policy = MaskingPolicy {
        keep: keep
            .iter()
            .map(|(doctype, field)| spot_sandbox::KeepField::new(*doctype, *field))
            .collect(),
        ..MaskingPolicy::default()
    };
    let engine = MaskingEngine::new(&classes, &policy);
    let mut data = fake::sample();
    engine.mask(&mut data, fake::sample_key()).unwrap();
    data
}

#[test]
fn kept_fields_are_left_alone() {
    let data = run_keeping(&[
        ("customer", "city"),
        ("sales_invoice", "number"),
        ("sales_invoice", "status"),
    ]);
    assert_eq!(text(&data, "c-1", "city"), "Leeds");
    assert_eq!(text(&data, "inv-1", "number"), "INV-0001");
    assert_eq!(text(&data, "inv-1", "status"), "draft");
}

#[test]
fn unclassified_fields_are_masked() {
    let (data, _) = masked();
    assert_ne!(text(&data, "c-1", "city"), "Leeds");
    assert_ne!(text(&data, "inv-1", "number"), "INV-0001");
    assert_ne!(text(&data, "inv-1", "status"), "draft");
}

#[test]
fn a_keep_decision_opens_only_the_field_it_names() {
    let data = run_keeping(&[("customer", "city")]);
    assert_eq!(text(&data, "c-1", "city"), "Leeds");
    assert_ne!(text(&data, "inv-1", "number"), "INV-0001");
    assert_ne!(text(&data, "inv-1", "status"), "draft");
}

#[test]
fn a_field_the_metadata_does_not_know_is_masked() {
    let classes = fake::sample_classes();
    let policy = MaskingPolicy::default();
    let engine = MaskingEngine::new(&classes, &policy);
    let mut data = fake::sample();
    data.documents[0]
        .fields
        .insert("nickname".to_owned(), json!("Ally"));
    engine.mask(&mut data, fake::sample_key()).unwrap();
    assert_ne!(text(&data, "c-1", "nickname"), "Ally");
}

#[test]
fn the_report_and_the_key_never_carry_a_value() {
    let (data, report) = masked();
    let debug = format!("{report:?}");
    for value in [
        fake::CUSTOMER_NAME,
        fake::CUSTOMER_EMAIL,
        fake::CUSTOMER_PHONE,
        fake::CUSTOMER_TAX_ID,
        fake::SMTP_PASSWORD,
    ] {
        assert!(!debug.contains(value), "the report carries a value");
    }
    assert!(!debug.contains("Alice"));
    assert!(!format!("{data:?}").is_empty());

    let key = SandboxKey::new([3u8; 32]);
    let key_debug = format!("{key:?}");
    assert!(key_debug.contains("<redacted>"));
    assert!(!key_debug.contains("03"));
    assert!(report.key_discarded);
}

#[test]
fn masker_shapes_for_direct_values() {
    let masker = Masker::new(fake::sample_key());

    let email = masker.mask_text("bob@example.org");
    assert!(email.ends_with("@example.invalid"));
    assert!(!email.contains("bob"));

    let original_phone = "+1 (555) 010-9999";
    let phone = masker.mask_text(original_phone);
    assert_eq!(
        phone.chars().filter(char::is_ascii_digit).count(),
        original_phone.chars().filter(char::is_ascii_digit).count()
    );
    assert_eq!(phone.len(), original_phone.len());
    assert!(phone.starts_with('+'));
    assert_eq!(
        phone
            .chars()
            .filter(|c| !c.is_ascii_digit())
            .collect::<String>(),
        original_phone
            .chars()
            .filter(|c| !c.is_ascii_digit())
            .collect::<String>()
    );

    let iban = masker.mask_text("GB82WEST12345698765432");
    assert_eq!(iban.len(), "GB82WEST12345698765432".len());
    assert!(iban_valid(&iban));

    let opaque = masker.mask_text("Alice Example");
    assert!(opaque.starts_with("p-"));
    assert_eq!(opaque, masker.mask_text("Alice Example"));
    assert_ne!(opaque, masker.mask_text("Bob Example"));
}

#[test]
fn a_numeric_identifier_keeps_its_type_and_width() {
    let (data, _) = masked();
    let points = data.by_id("c-1").unwrap().fields["loyalty_points"]
        .as_i64()
        .unwrap();
    assert_ne!(points, 1_200);
    assert_eq!(points.to_string().len(), "1200".len());
}

#[test]
fn a_policy_can_leave_derived_copies_alone_and_the_scan_then_finds_them() {
    let classes = fake::sample_classes();
    let policy = MaskingPolicy::from_toml("mask_derived_copies = false\n").unwrap();
    let engine = MaskingEngine::new(&classes, &policy);
    let mut data = fake::sample();
    let outcome = engine.mask(&mut data, fake::sample_key()).unwrap();
    assert_ne!(text(&data, "c-1", "name"), fake::CUSTOMER_NAME);
    assert_eq!(text(&data, "inv-1", "customer_name"), fake::CUSTOMER_NAME);
    assert_eq!(outcome.report.derived_masked, 0);
    // The untouched copy is exactly the leak the scanner exists to find.
    let report = spot_sandbox::scan(&data, &outcome.index);
    assert!(!report.is_clean());
}

#[test]
fn masking_an_empty_store_is_a_no_op() {
    let classes = fake::sample_classes();
    let policy = MaskingPolicy::default();
    let engine = MaskingEngine::new(&classes, &policy);
    let mut data = SandboxData::new();
    let outcome = engine.mask(&mut data, fake::sample_key()).unwrap();
    assert_eq!(outcome.report.fields_masked, 0);
    assert!(data.is_empty());
}

#[test]
fn the_store_round_trips_through_the_trait() {
    let mut data = fake::sample();
    let documents = data.documents();
    let count = documents.len();
    data.replace_documents(documents).unwrap();
    assert_eq!(data.documents().len(), count);
}

/// The standard IBAN mod-97 check, written out so the test does not trust the crate under test.
fn iban_valid(iban: &str) -> bool {
    let compact: String = iban.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() < 8 {
        return false;
    }
    let rearranged = format!("{}{}", &compact[4..], &compact[..4]);
    let mut remainder = 0u32;
    for ch in rearranged.chars() {
        if let Some(digit) = ch.to_digit(10) {
            remainder = (remainder * 10 + digit) % 97;
        } else if ch.is_ascii_alphabetic() {
            let value = ch.to_ascii_uppercase() as u32 - 'A' as u32 + 10;
            remainder = (remainder * 10 + value / 10) % 97;
            remainder = (remainder * 10 + value % 10) % 97;
        } else {
            return false;
        }
    }
    remainder == 1
}
