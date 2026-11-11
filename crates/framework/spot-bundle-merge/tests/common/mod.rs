//! Shared helpers for the `spot-bundle-merge` tests: a fixed site, three-way bundles captured
//! from it, and the raw ZIP tools that prove order independence.
//!
//! Everything is built from `spot-custom-bundle`'s public `FakeSite` and `capture`, so the tests
//! exercise the real bundle format and the real item decomposition.

#![allow(clippy::unwrap_used, clippy::expect_used)]
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::Write as _;

use spot_custom_bundle::{Bundle, CaptureOptions, FakeSite, SectionRegistry, SiteWriter, capture};
use spot_meta::{Definition, FieldSpec, FieldType, Property};
use spot_views::{ViewDef, ViewLayer};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

/// The apps every test site has installed.
pub fn apps() -> Vec<(String, semver::Version)> {
    vec![
        ("accounting".to_owned(), semver::Version::new(1, 2, 3)),
        ("crm".to_owned(), semver::Version::new(2, 0, 1)),
    ]
}

/// A `sales_invoice` DocType owned by `accounting`, with a handful of fields to customise.
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
    let note = FieldSpec::new("note", FieldType::Text);
    let internal = FieldSpec::new("internal_note", FieldType::Text);
    definition.fields = vec![customer, amount, status, note, internal];
    definition
}

/// A minimal `customer` DocType owned by `crm`.
pub fn customer_definition() -> Definition {
    let mut definition = Definition::new("customer");
    definition.app = Some("crm".to_owned());
    definition.fields = vec![FieldSpec::new("name", FieldType::Text)];
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

/// A site with the apps, both DocTypes and the base view, and no customisations.
pub fn empty_site() -> FakeSite {
    let mut site = FakeSite::new();
    for (app, version) in apps() {
        site = site.with_app(app, version);
    }
    site.with_definition(definition())
        .with_definition(customer_definition())
        .with_base_view(base_view())
}

/// The section registry with the built-in sections.
pub fn registry() -> SectionRegistry {
    SectionRegistry::new()
}

/// Capture options with a fixed clock argument (time is never read from the clock).
pub fn options(name: &str) -> CaptureOptions {
    CaptureOptions::new(name, "1.0.0", "glm", "2026-01-01T00:00:00Z")
}

/// Captures `site` into a bundle named `name`.
pub fn bundle(site: &FakeSite, name: &str) -> Bundle {
    capture(site, &options(name)).expect("the site captures")
}

/// The items of one section kind in a bundle, as `Section::items` returns them.
pub fn items_of(bundle: &Bundle, kind: &str) -> BTreeMap<String, Vec<u8>> {
    let empty = BTreeMap::new();
    let files = bundle
        .section(kind)
        .map(|data| &data.files)
        .unwrap_or(&empty);
    registry()
        .get(kind)
        .expect("the kind is a built-in section")
        .items(files)
}

/// Adds a custom field to a site.
pub fn add_field(site: &mut FakeSite, doctype: &str, name: &str, insert_after: Option<&str>) {
    let spec = FieldSpec::new(name, FieldType::Text);
    site.upsert_custom_field(doctype, spec, insert_after)
        .expect("the custom field upserts");
}

/// Removes a custom field from a site.
pub fn remove_field(site: &mut FakeSite, doctype: &str, name: &str) {
    site.remove_custom_field(doctype, name)
        .expect("the custom field removes");
}

/// Sets a label override on a field.
pub fn set_label(site: &mut FakeSite, doctype: &str, field: &str, label: &str) {
    site.set_override(doctype, Some(field), Property::Label(label.to_owned()))
        .expect("the override sets");
}

/// Adds a site view layer.
pub fn add_layer(site: &mut FakeSite, layer: ViewLayer) {
    site.upsert_view_layer(layer).expect("the layer upserts");
}

/// Writes `entries` as a ZIP in exactly the given order (not sorted), to prove the merge ignores
/// input order.
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

/// The entries of a bundle, in a naive walk order.
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
