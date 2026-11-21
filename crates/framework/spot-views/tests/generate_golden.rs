//! The golden TOML files for the three sample DocTypes (brief deliverable 5): the
//! generated form and list of the plain master `item`, the posted document
//! `sales_invoice` with its child table, and the grid of the child DocType
//! `sales_invoice_item`. The committed bytes are the contract; the generator is
//! deterministic, so any drift is a deliberate format change.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_meta::EffectiveMeta;
use spot_views::generate::default_view;
use spot_views::viewdef::ViewKind;

#[path = "generate/common.rs"]
mod common;

use common::effective;

fn golden_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden")
}

/// The generated TOML of one fixture, fresh each call.
fn toml_of(meta: &EffectiveMeta, doctype: &str, kind: ViewKind) -> String {
    let view = default_view(meta, doctype, kind)
        .unwrap_or_else(|errors| panic!("{doctype} {kind:?} does not generate: {errors:?}"));
    view.to_toml()
        .expect("a generated view always writes TOML")
        .to_string()
}

#[test]
fn the_plain_master_matches_the_committed_bytes() {
    let meta = effective();
    assert_eq!(
        toml_of(&meta, "item", ViewKind::Form),
        std::fs::read_to_string(golden_dir().join("item.form.toml"))
            .expect("the committed golden is checked in"),
    );
    assert_eq!(
        toml_of(&meta, "item", ViewKind::List),
        std::fs::read_to_string(golden_dir().join("item.list.toml"))
            .expect("the committed golden is checked in"),
    );
}

#[test]
fn the_posted_document_matches_the_committed_bytes() {
    let meta = effective();
    assert_eq!(
        toml_of(&meta, "sales_invoice", ViewKind::Form),
        std::fs::read_to_string(golden_dir().join("sales_invoice.form.toml"))
            .expect("the committed golden is checked in"),
    );
    assert_eq!(
        toml_of(&meta, "sales_invoice", ViewKind::List),
        std::fs::read_to_string(golden_dir().join("sales_invoice.list.toml"))
            .expect("the committed golden is checked in"),
    );
}

#[test]
fn the_child_doctype_grid_matches_the_committed_bytes() {
    let meta = effective();
    assert_eq!(
        toml_of(&meta, "sales_invoice_item", ViewKind::Grid),
        std::fs::read_to_string(golden_dir().join("sales_invoice_item.grid.toml"))
            .expect("the committed golden is checked in"),
    );
}

#[test]
fn every_committed_golden_parses_back_through_the_view_model() {
    let dir = golden_dir();
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).expect("the golden directory is checked in") {
        let path = entry.expect("a file entry").path();
        if path.extension().is_some_and(|x| x == "toml") {
            let text = std::fs::read_to_string(&path).expect("readable golden");
            let parsed = spot_views::ViewDef::parse(
                &path.file_name().expect("named").to_string_lossy(),
                &text,
            )
            .unwrap_or_else(|errors| {
                panic!("golden {} does not parse: {errors:?}", path.display())
            });
            // And round-trips byte-equal: the generator writes what the model reads.
            let written = parsed.to_toml().expect("written").to_string();
            assert_eq!(
                written,
                text,
                "golden {} drifted from the model",
                path.display()
            );
            checked += 1;
        }
    }
    assert_eq!(
        checked, 5,
        "the five committed goldens (two forms, two lists, one grid)"
    );
}

#[test]
#[ignore = "writes the golden files; run by hand when the format intentionally changes"]
fn regenerate() {
    let meta = effective();
    let outputs = [
        ("item.form.toml", toml_of(&meta, "item", ViewKind::Form)),
        ("item.list.toml", toml_of(&meta, "item", ViewKind::List)),
        (
            "sales_invoice.form.toml",
            toml_of(&meta, "sales_invoice", ViewKind::Form),
        ),
        (
            "sales_invoice.list.toml",
            toml_of(&meta, "sales_invoice", ViewKind::List),
        ),
        (
            "sales_invoice_item.grid.toml",
            toml_of(&meta, "sales_invoice_item", ViewKind::Grid),
        ),
    ];
    std::fs::create_dir_all(golden_dir()).expect("golden dir");
    for (name, text) in outputs {
        let path = golden_dir().join(name);
        std::fs::write(&path, text).expect("write golden");
        println!("rewrote {}", path.display());
    }
}
