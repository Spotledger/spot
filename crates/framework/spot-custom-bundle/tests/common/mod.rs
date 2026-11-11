//! Shared helpers for the integration tests: a fixed site, a raw ZIP writer and the entry
//! view of a bundle.

#![allow(dead_code)]

use std::io::Write as _;

use spot_custom_bundle::digest::{canonical_input, sha256};
use spot_custom_bundle::{Bundle, FakeSite};
use spot_meta::{Definition, FieldSpec, FieldType, Property};
use spot_views::{LayerKind, LayerName, ViewDef, ViewLayer};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// The apps every test site has installed.
pub fn apps() -> Vec<(String, semver::Version)> {
    vec![
        ("accounting".to_owned(), semver::Version::new(1, 2, 3)),
        ("crm".to_owned(), semver::Version::new(2, 0, 1)),
    ]
}

/// A DocType definition owned by `accounting`, with a handful of fields to customise.
pub fn definition() -> Definition {
    let mut definition = Definition::new("sales_invoice");
    definition.app = Some("accounting".to_owned());
    definition.label = Some("Sales Invoice".to_owned());
    let mut customer = FieldSpec::new("customer", FieldType::Link);
    customer.options = Some("customer".to_owned());
    let mut amount = FieldSpec::new("amount", FieldType::Decimal);
    amount.scale = Some(2);
    amount.precision = Some(18);
    let mut status = FieldSpec::new("status", FieldType::Select);
    status.options = Some("draft\nposted".to_owned());
    let mut note = FieldSpec::new("note", FieldType::Text);
    note.max_chars = Some(100);
    let internal = FieldSpec::new("internal_note", FieldType::Text);
    definition.fields = vec![customer, amount, status, note, internal];
    definition
}

/// The app's base form view for `sales_invoice`.
pub fn base_view() -> ViewDef {
    ViewDef::parse(
        "base.toml",
        "doctype = \"sales_invoice\"\nkind = \"form\"\n\n[[sections]]\nid = \"main\"\n\n\
         [[sections.items]]\ntype = \"field\"\nfield = \"customer\"\n\n\
         [[sections.items]]\ntype = \"field\"\nfield = \"amount\"\n\n\
         [[sections.items]]\ntype = \"field\"\nfield = \"status\"\n\n\
         [[sections.items]]\ntype = \"field\"\nfield = \"note\"\n\n\
         [[sections.items]]\ntype = \"field\"\nfield = \"internal_note\"\n",
    )
    .expect("the base view parses")
}

/// A minimal `customer` DocType owned by `crm`, the link target of `sales_invoice.customer`.
pub fn customer_definition() -> Definition {
    let mut definition = Definition::new("customer");
    definition.app = Some("crm".to_owned());
    definition.fields = vec![FieldSpec::new("name", FieldType::Text)];
    definition
}

/// A site whose customizations and a matching site with the same apps, definitions and base
/// view; the second is empty of customizations.
pub fn empty_site() -> FakeSite {
    let mut site = FakeSite::new();
    for (app, version) in apps() {
        site = site.with_app(app, version);
    }
    site.with_definition(definition())
        .with_definition(customer_definition())
        .with_base_view(base_view())
}

/// A site with every customization kind the bundle carries.
pub fn rich_site() -> FakeSite {
    let mut site = empty_site();

    // Two custom fields, one anchored after `customer`.
    let mut region = FieldSpec::new("custom_region", FieldType::Text);
    region.label = Some("Region".to_owned());
    let mut code = FieldSpec::new("custom_code", FieldType::Text);
    code.max_chars = Some(32);
    site = site.with_custom_field("sales_invoice", region, Some("customer"));
    site = site.with_custom_field("sales_invoice", code, None);

    // Four overrides of different property kinds, one of them hiding a field.
    site = site.with_override(
        "sales_invoice",
        Some("note"),
        Property::Label("Notes".to_owned()),
    );
    site = site.with_override("sales_invoice", Some("note"), Property::Required(true));
    site = site.with_override(
        "sales_invoice",
        Some("internal_note"),
        Property::Hidden(true),
    );
    site = site.with_override(
        "sales_invoice",
        Some("status"),
        Property::Options(vec![
            "draft".to_owned(),
            "posted".to_owned(),
            "cancelled".to_owned(),
        ]),
    );

    // A site layer that moves a field and a role layer that hides one.
    site = site.with_layer(move_field_layer());
    site = site.with_layer(hide_field_layer());

    // Three scripts.
    site = site.with_script(
        "validate",
        "accounting",
        "sales_invoice.validate",
        "export function validate(): boolean { return true; }\n",
    );
    site = site.with_script(
        "on_submit",
        "accounting",
        "sales_invoice.on_submit",
        "export const submitted: boolean = true;\n",
    );
    site = site.with_script(
        "totals",
        "accounting",
        "sales_invoice.totals",
        "export function total(): number { return 1; }\n",
    );

    // Five rules that parse.
    site = site.with_rule("positive_qty", "doc.qty > 0");
    site = site.with_rule("big_total", "doc.total >= 100.00");
    site = site.with_rule("draft_only", "doc.status == \"draft\"");
    site = site.with_rule("not_cancelled", "not doc.cancelled");
    site = site.with_rule("sum_matches", "doc.a + 1 < doc.b");

    // Ten fixtures.
    let documents: Vec<serde_json::Value> = (0..10)
        .map(|i| {
            serde_json::json!({
                "id": format!("INV-{i:04}"),
                "customer": format!("CUST-{i:04}"),
                "amount": i,
            })
        })
        .collect();
    site.with_fixtures("sales_invoice", documents)
}

/// A `site` layer that moves `customer` after `amount`.
pub fn move_field_layer() -> ViewLayer {
    ViewLayer::parse(
        "site.toml",
        "doctype = \"sales_invoice\"\nkind = \"site\"\n\n[[ops]]\nop = \"move_field\"\n\
         field = \"customer\"\nafter = \"amount\"\n",
    )
    .expect("the site layer parses")
}

/// A `role` layer that hides `internal_note`.
pub fn hide_field_layer() -> ViewLayer {
    ViewLayer::parse(
        "role.toml",
        "doctype = \"sales_invoice\"\nkind = \"role\"\nrole = \"manager\"\n\n[[ops]]\n\
         op = \"hide_field\"\nfield = \"internal_note\"\n",
    )
    .expect("the role layer parses")
}

/// Writes `entries` as a ZIP in exactly the given order (not sorted), so a test can prove the
/// digest ignores entry order.
pub fn raw_zip(entries: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, bytes) in entries {
        writer
            .start_file(
                name.clone(),
                SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
            )
            .expect("a zip entry starts");
        writer.write_all(bytes).expect("a zip entry writes");
    }
    writer.finish().expect("the zip finishes").into_inner()
}

/// The entries of a bundle, in the order a naive walk finds them: `bundle.toml`, the section
/// files and the `digest` entry.
pub fn entries_of(bundle: &Bundle) -> Vec<(String, Vec<u8>)> {
    let mut entries = vec![(
        "bundle.toml".to_owned(),
        bundle
            .header()
            .to_toml()
            .expect("the header writes")
            .into_bytes(),
    )];
    for (kind, data) in bundle.sections() {
        for (name, bytes) in &data.files {
            entries.push((format!("sections/{kind}/{name}"), bytes.clone()));
        }
    }
    entries.push(("digest".to_owned(), bundle.digest().to_hex().into_bytes()));
    entries
}

/// The hex digest of an entry listing, using the crate's own canonical form.
pub fn digest_hex(entries: &[(String, Vec<u8>)]) -> String {
    let parts: Vec<(String, Vec<u8>)> = entries
        .iter()
        .filter(|(name, _)| name != "digest")
        .cloned()
        .collect();
    let mut hex = String::with_capacity(64);
    for byte in sha256(&canonical_input(&parts)) {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// A `LayerKind` for a role layer, for assertions.
pub fn role_kind(role: &str) -> LayerKind {
    LayerKind {
        kind: LayerName::Role,
        role: Some(role.to_owned()),
        step: None,
    }
}
