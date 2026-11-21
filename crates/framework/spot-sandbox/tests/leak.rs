//! The leak scanner: a clean masked sandbox passes; a seeded copy is found on every surface;
//! normalisation catches casing and spacing; the report never carries the value; the Bloom
//! filter has no false negatives and never confirms a hit on its own.

#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use common as fake;

use serde_json::json;
use sha2::{Digest, Sha256};
use spot_sandbox::scan::{compact, normalise};
use spot_sandbox::{
    CacheEntry, MaskingEngine, MaskingPolicy, SandboxData, ScanReport, SearchRow, SourceIndex,
    Surface, scan,
};

fn masked() -> (SandboxData, SourceIndex) {
    let classes = fake::sample_classes();
    let policy = MaskingPolicy::default();
    let engine = MaskingEngine::new(&classes, &policy);
    let mut data = fake::sample();
    let outcome = engine.mask(&mut data, fake::sample_key()).unwrap();
    (data, outcome.index)
}

#[test]
fn a_masked_sandbox_scans_clean() {
    let (data, index) = masked();
    let report = scan(&data, &index);
    assert!(report.is_clean(), "{:?}", report.leaks);
    assert!(report.scanned_values > 0);
    assert!(report.indexed_values > 0);
    assert_eq!(report.by_surface.get("documents"), Some(&12));
    assert_eq!(report.by_surface.get("journal"), Some(&2));
}

#[test]
fn a_copy_in_an_unclassified_field_is_a_leak() {
    let (mut data, index) = masked();
    let invoice = data
        .documents
        .iter_mut()
        .find(|doc| doc.id == "inv-1")
        .unwrap();
    invoice
        .fields
        .insert("status".to_owned(), json!(fake::CUSTOMER_EMAIL));

    let report = scan(&data, &index);
    assert!(!report.is_clean());
    let leaks = report.leaks_on(Surface::Documents);
    assert_eq!(leaks.len(), 1);
    assert_eq!(leaks[0].field, "status");
    assert_eq!(leaks[0].record, "inv-1");
    assert_eq!(leaks[0].digest.len(), 16);
}

#[test]
fn a_copy_in_a_cache_is_a_leak() {
    let (mut data, index) = masked();
    data.caches.push(CacheEntry {
        key: "customer:c-1".to_owned(),
        value: fake::CUSTOMER_EMAIL.to_owned(),
    });
    let report = scan(&data, &index);
    assert_eq!(report.leaks_on(Surface::Caches).len(), 1);
}

#[test]
fn a_copy_in_the_audit_row_or_journal_is_a_leak() {
    let (mut data, index) = masked();
    data.audit[0].after = json!(fake::CUSTOMER_TAX_ID);
    data.journal[0].value = json!(fake::CUSTOMER_PHONE);
    let report = scan(&data, &index);
    assert_eq!(report.leaks_on(Surface::Audit).len(), 1);
    assert_eq!(report.leaks_on(Surface::Journal).len(), 1);
}

#[test]
fn a_copy_in_an_attachment_or_the_search_index_is_a_leak() {
    let (mut data, index) = masked();
    data.attachments[0].filename = fake::CUSTOMER_EMAIL.to_owned();
    data.search.push(SearchRow {
        doctype: "customer".to_owned(),
        doc_id: "c-1".to_owned(),
        text: format!("contact {}", fake::CUSTOMER_NAME),
        embedding: vec![],
    });
    let report = scan(&data, &index);
    assert_eq!(report.leaks_on(Surface::Attachments).len(), 1);
    assert_eq!(report.leaks_on(Surface::Search).len(), 1);
}

#[test]
fn normalisation_catches_case_and_spacing() {
    let (mut data, index) = masked();
    let invoice = data
        .documents
        .iter_mut()
        .find(|doc| doc.id == "inv-1")
        .unwrap();
    invoice
        .fields
        .insert("status".to_owned(), json!("  ALICE    Example "));
    let report = scan(&data, &index);
    assert!(!report.is_clean());
    assert_eq!(normalise("  ALICE    Example "), "alice example");
    assert_eq!(compact("Alice-Example"), "aliceexample");
}

#[test]
fn a_value_inside_a_long_text_is_found() {
    let (mut data, index) = masked();
    let invoice = data
        .documents
        .iter_mut()
        .find(|doc| doc.id == "inv-1")
        .unwrap();
    invoice.fields.insert(
        "status".to_owned(),
        json!(format!(
            "imported from the legacy system; reach {} for questions",
            fake::CUSTOMER_EMAIL
        )),
    );
    let report = scan(&data, &index);
    assert_eq!(report.leaks_on(Surface::Documents).len(), 1);
}

#[test]
fn the_report_never_carries_the_value() {
    let (mut data, index) = masked();
    data.caches.push(CacheEntry {
        key: "k".to_owned(),
        value: format!("{} / {}", fake::CUSTOMER_EMAIL, fake::SMTP_PASSWORD),
    });
    let report = scan(&data, &index);
    let debug = format!("{report:?}");
    let json = serde_json::to_string(&report).unwrap();
    for value in [
        fake::CUSTOMER_EMAIL,
        fake::CUSTOMER_NAME,
        fake::SMTP_PASSWORD,
    ] {
        assert!(!debug.contains(value));
        assert!(!json.contains(value));
    }
}

#[test]
fn the_scanner_detects_each_seeded_leak_and_only_those() {
    let (mut data, index) = masked();
    data.caches.push(CacheEntry {
        key: "k1".to_owned(),
        value: fake::CUSTOMER_EMAIL.to_owned(),
    });
    data.caches.push(CacheEntry {
        key: "k2".to_owned(),
        value: "nothing sensitive here".to_owned(),
    });
    let report: ScanReport = scan(&data, &index);
    assert_eq!(report.leaks.len(), 1);
}

#[test]
fn the_leak_list_has_the_briefs_shape() {
    let (mut data, index) = masked();
    assert!(spot_sandbox::scan_leaks(&data, &index).is_empty());
    data.caches.push(CacheEntry {
        key: "k".to_owned(),
        value: fake::CUSTOMER_EMAIL.to_owned(),
    });
    let leaks = spot_sandbox::scan_leaks(&data, &index);
    assert_eq!(leaks.len(), 1);
    assert_eq!(leaks[0].surface, Surface::Caches);
}

#[test]
fn short_values_are_not_indexed() {
    let mut index = SourceIndex::new(4);
    index.add("abc");
    assert!(index.is_empty());
    index.add("abcd");
    assert!(!index.is_empty());
}

#[test]
fn the_bloom_filter_has_no_false_negatives() {
    let mut bloom = spot_sandbox::scan::Bloom::with_capacity(1_000);
    let mut digests = Vec::new();
    for number in 0u32..1_000 {
        let mut hasher = Sha256::new();
        hasher.update(number.to_le_bytes());
        let digest = hasher.finalize();
        let mut bytes = [0u8; 32];
        bytes.copy_from_slice(&digest);
        bloom.insert(&bytes);
        digests.push(bytes);
    }
    for digest in &digests {
        assert!(bloom.might_contain(digest));
    }
    assert_eq!(bloom.hashes(), 8);
    assert!(bloom.bit_len() >= 10 * 1_000);
}

#[test]
fn a_bloom_maybe_is_never_a_confirmed_hit() {
    let mut index = SourceIndex::new(4);
    index.add(fake::CUSTOMER_EMAIL);
    assert!(index.contains(fake::CUSTOMER_EMAIL).is_some());
    assert!(index.contains("bob@example.org").is_none());
    // Adding many values grows the filter without losing an earlier one.
    for number in 0..2_000 {
        index.add(&format!("value-{number}@example.org"));
    }
    assert!(index.contains(fake::CUSTOMER_EMAIL).is_some());
}

#[test]
fn normal_forms_split_and_join_as_documented() {
    assert_eq!(normalise("A  B\tC\n"), "a b c");
    assert_eq!(compact("GB82 WEST 1234"), "gb82west1234");
    assert_eq!(normalise(""), "");
    assert_eq!(compact(""), "");
}
