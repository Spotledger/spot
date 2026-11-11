#![allow(dead_code)]
//! A small, entirely synthetic tenant shared by the tests.
//!
//! The data is not personal data: every name, e-mail, phone and IBAN is a reserved example
//! value, and the "attachment" is text. Two DocTypes model the cases the masker must tell
//! apart:
//!
//! - `customer`: a `pii` name, e-mail, phone and national id, a `financial` bank account, a
//!   `financial` credit limit (an **amount**, kept), a `pii` integer (a numeric identifier,
//!   masked), a `pii` long text (scrubbed) and an unclassified city.
//! - `sales_invoice`: a plain number, a link to the customer, a `fetch_from` copy of the
//!   customer's name (the denormalised case), and a `financial` amount. `number` and `status`
//!   carry no classification.
//!
//! Alongside the documents the fixture seeds a journal, an audit row, an attachment, search
//! rows, a cache that holds a copy of the customer's e-mail, a secret and an integration. A
//! clean run must remove the copies without touching an amount.

use serde_json::json;
use spot_meta::{Definition, FieldSpec, FieldType, MaskSpec};

use spot_sandbox::classify::MetadataClasses;
use spot_sandbox::data::{
    Attachment, AuditRow, CacheEntry, Document, Integration, JournalRow, SandboxData, SearchRow,
    SecretRow,
};
use spot_sandbox::mask::SandboxKey;
use spot_sandbox::plan::{MaskingPolicy, SandboxPlan};

/// The fixture's customer name.
pub const CUSTOMER_NAME: &str = "Alice Example";
/// The fixture's customer e-mail (a reserved example domain).
pub const CUSTOMER_EMAIL: &str = "alice@example.com";
/// The fixture's customer phone (a reserved Ofcom drama range).
pub const CUSTOMER_PHONE: &str = "+44 7700 900123";
/// The fixture's national id (a synthetic value).
pub const CUSTOMER_TAX_ID: &str = "QQ123456C";
/// The fixture's bank account (a synthetic IBAN shape).
pub const CUSTOMER_IBAN: &str = "GB00TEST00000000000001";
/// The fixture's free text.
pub const CUSTOMER_NOTES: &str = "Customer prefers e-mail. Call on Tuesday.";
/// The fixture's attachment file name.
pub const ATTACHMENT_NAME: &str = "alice-photo.png";
/// The fixture's secret value.
pub const SMTP_PASSWORD: &str = "not-a-real-password-42";
/// The fixture's per-document secret value.
pub const DOCUMENT_SECRET: &str = "sk-live-not-a-real-key";
/// The fixture's credit limit (kept).
pub const CREDIT_LIMIT: i64 = 25_000;
/// The fixture's invoice total (kept).
pub const INVOICE_TOTAL: i64 = 1_200;

/// A fixed key so a run is reproducible. Never a production key.
#[must_use]
pub fn sample_key() -> SandboxKey {
    SandboxKey::new([7u8; 32])
}

/// The fixture's DocTypes.
#[must_use]
pub fn sample_definitions() -> Vec<Definition> {
    let mut customer = Definition::new("customer");
    customer.fields = vec![
        classified(
            FieldSpec::new("name", FieldType::Text),
            &["pii"],
            "personal",
        ),
        classified(
            FieldSpec::new("email", FieldType::Text),
            &["pii"],
            "personal",
        ),
        classified(
            FieldSpec::new("phone", FieldType::Text),
            &["pii"],
            "personal",
        ),
        classified(
            FieldSpec::new("tax_id", FieldType::Text),
            &["pii"],
            "personal",
        ),
        classified(
            FieldSpec::new("bank_account", FieldType::Text),
            &["financial"],
            "payment",
        ),
        classified(
            FieldSpec::new("credit_limit", FieldType::Decimal),
            &["financial"],
            "payment",
        ),
        classified(
            FieldSpec::new("loyalty_points", FieldType::Int),
            &["pii"],
            "personal",
        ),
        classified(
            FieldSpec::new("notes", FieldType::LongText),
            &["pii"],
            "personal",
        ),
        classified(
            FieldSpec::new("api_key", FieldType::Secret),
            &["secret"],
            "personal",
        ),
        FieldSpec::new("city", FieldType::Text),
        classified(
            FieldSpec::new("avatar", FieldType::Attach),
            &["pii"],
            "personal",
        ),
    ];
    customer.protection_groups = vec![
        spot_meta::ProtectionGroupSpec::new("personal", "Personal data"),
        spot_meta::ProtectionGroupSpec::new("payment", "Payment details"),
    ];

    let mut invoice = Definition::new("sales_invoice");
    let mut customer_link = FieldSpec::new("customer", FieldType::Link);
    customer_link.options = Some("customer".to_owned());
    let mut customer_name = FieldSpec::new("customer_name", FieldType::Text);
    customer_name.fetch_from = Some("customer.name".to_owned());
    invoice.fields = vec![
        FieldSpec::new("number", FieldType::Text),
        customer_link,
        customer_name,
        FieldSpec::new("status", FieldType::Select),
        classified(
            FieldSpec::new("total", FieldType::Decimal),
            &["financial"],
            "payment",
        ),
    ];

    let mut entry = Definition::new("gl_entry");
    entry.fields = vec![
        classified(
            FieldSpec::new("debit", FieldType::Decimal),
            &["financial"],
            "payment",
        ),
        classified(
            FieldSpec::new("credit", FieldType::Decimal),
            &["financial"],
            "payment",
        ),
        FieldSpec::new("account", FieldType::Text),
    ];

    vec![customer, invoice, entry]
}

fn classified(mut spec: FieldSpec, tags: &[&str], group: &str) -> FieldSpec {
    spec.classification = tags.iter().map(|tag| (*tag).to_owned()).collect();
    spec.protection_group = Some(group.to_owned());
    spec.masking = Some(MaskSpec::placeholder());
    spec
}

/// The fixture's classes, read from its definitions exactly as production metadata would be.
#[must_use]
pub fn sample_classes() -> MetadataClasses {
    MetadataClasses::from_definitions(&sample_definitions())
}

/// A plan for the fixture: clone `prod_eu` into `sandbox_acme`, taking a fresh snapshot.
#[must_use]
pub fn sample_plan() -> SandboxPlan {
    SandboxPlan::new("prod_eu", None, MaskingPolicy::default(), "sandbox_acme")
}

/// The fixture's tenant data.
#[must_use]
pub fn sample() -> SandboxData {
    let customer = Document::new("customer", "c-1")
        .with("name", json!(CUSTOMER_NAME))
        .with("email", json!(CUSTOMER_EMAIL))
        .with("phone", json!(CUSTOMER_PHONE))
        .with("tax_id", json!(CUSTOMER_TAX_ID))
        .with("bank_account", json!(CUSTOMER_IBAN))
        .with("credit_limit", json!(CREDIT_LIMIT))
        .with("loyalty_points", json!(1_200))
        .with("notes", json!(CUSTOMER_NOTES))
        .with("api_key", json!("sk-live-not-a-real-key"))
        .with("city", json!("Leeds"))
        .with("avatar", json!("f-1"));

    let invoice = Document::new("sales_invoice", "inv-1")
        .with("number", json!("INV-0001"))
        .with("customer", json!("c-1"))
        .with("customer_name", json!(CUSTOMER_NAME))
        .with("status", json!("draft"))
        .with("total", json!(INVOICE_TOTAL));

    SandboxData {
        documents: vec![customer, invoice],
        journal: vec![
            JournalRow {
                seq: 1,
                doctype: "customer".to_owned(),
                doc_id: "c-1".to_owned(),
                field: "email".to_owned(),
                value: json!(CUSTOMER_EMAIL),
            },
            JournalRow {
                seq: 2,
                doctype: "sales_invoice".to_owned(),
                doc_id: "inv-1".to_owned(),
                field: "total".to_owned(),
                value: json!(INVOICE_TOTAL),
            },
            JournalRow {
                seq: 3,
                doctype: "sales_invoice".to_owned(),
                doc_id: "inv-1".to_owned(),
                field: "number".to_owned(),
                value: json!("INV-0001"),
            },
            JournalRow {
                seq: 4,
                doctype: "gl_entry".to_owned(),
                doc_id: "gl-1".to_owned(),
                field: "debit".to_owned(),
                value: json!(-INVOICE_TOTAL),
            },
            JournalRow {
                seq: 5,
                doctype: "gl_entry".to_owned(),
                doc_id: "gl-1".to_owned(),
                field: "credit".to_owned(),
                value: json!(INVOICE_TOTAL),
            },
        ],
        audit: vec![AuditRow {
            seq: 1,
            doctype: "customer".to_owned(),
            doc_id: "c-1".to_owned(),
            field: "email".to_owned(),
            before: json!("alice.old@example.com"),
            after: json!(CUSTOMER_EMAIL),
        }],
        search: vec![
            SearchRow {
                doctype: "customer".to_owned(),
                doc_id: "c-1".to_owned(),
                text: format!("{CUSTOMER_NAME} {CUSTOMER_EMAIL}"),
                embedding: vec![12, -3, 44],
            },
            SearchRow {
                doctype: "sales_invoice".to_owned(),
                doc_id: "inv-1".to_owned(),
                text: format!("INV-0001 {CUSTOMER_NAME}"),
                embedding: vec![1, 2, 3],
            },
        ],
        attachments: vec![Attachment {
            id: "f-1".to_owned(),
            doctype: "customer".to_owned(),
            doc_id: "c-1".to_owned(),
            field: "avatar".to_owned(),
            filename: ATTACHMENT_NAME.to_owned(),
            content_hash: "f".repeat(64),
            content: "the bytes of alice's photo".to_owned(),
            size: 4_096,
        }],
        caches: vec![CacheEntry {
            key: "customer:c-1".to_owned(),
            value: format!("{CUSTOMER_NAME} <{CUSTOMER_EMAIL}>"),
        }],
        secrets: vec![SecretRow {
            name: "smtp.password".to_owned(),
            value: SMTP_PASSWORD.to_owned(),
        }],
        integrations: vec![Integration {
            id: "int-1".to_owned(),
            kind: "webhook".to_owned(),
            endpoint: "https://hooks.example.com/services/abc".to_owned(),
            credential: "token=abc123".to_owned(),
        }],
        tenant: "sandbox_acme".to_owned(),
    }
}
