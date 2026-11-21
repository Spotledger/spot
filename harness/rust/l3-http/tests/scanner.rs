//! The case-file scanner `build.rs` runs, tested through the same source file.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

#[path = "../build/scan.rs"]
mod scan;

fn file(name: &str, text: &str) -> (String, String) {
    (name.to_owned(), text.to_owned())
}

const REGISTERED: &str =
    "pub const ORDER: u32 = 10;\npub fn cases() -> Vec<CaseDef> {\n    vec![]\n}\n";

#[test]
fn a_file_with_both_declarations_registers_and_a_helper_is_left_alone() {
    let files = [
        file("mod.rs", REGISTERED),
        file("auth.rs", "pub async fn sign_in() {}\n"),
        file("events.rs", REGISTERED),
        file("export.rs", REGISTERED),
        file("notes.md", REGISTERED),
    ];
    assert_eq!(
        scan::registered(&files),
        Ok(vec!["events".to_owned(), "export".to_owned()])
    );
}

#[test]
fn a_half_declared_file_fails_naming_the_file() {
    let only_order = [file("half.rs", "pub const ORDER: u32 = 1;\n")];
    let message = scan::registered(&only_order).unwrap_err();
    assert!(message.contains("half.rs"), "{message}");
    let only_cases = [file(
        "other.rs",
        "pub fn cases() -> Vec<CaseDef> {\n    vec![]\n}\n",
    )];
    let message = scan::registered(&only_cases).unwrap_err();
    assert!(message.contains("other.rs"), "{message}");
}

#[test]
fn an_indented_declaration_does_not_count() {
    let files = [file(
        "nested.rs",
        "mod x {\n    pub const ORDER: u32 = 1;\n    pub fn cases() -> Vec<CaseDef> { vec![] }\n}\n",
    )];
    assert_eq!(scan::registered(&files), Ok(vec![]));
}

#[test]
fn a_bad_module_name_fails_naming_the_file() {
    for bad in ["Export.rs", "my-cases.rs", "9cases.rs", "type.rs"] {
        let message = scan::registered(&[file(bad, REGISTERED)]).unwrap_err();
        assert!(message.contains(bad), "{message}");
    }
}

#[test]
fn the_registry_sorts_by_order_then_name() {
    let text = scan::registry_source("/x/src/cases", &["events".to_owned(), "export".to_owned()]);
    assert!(text.contains("#[path = \"/x/src/cases/events.rs\"]\npub mod events;"));
    assert!(text.contains("events::ORDER"));
    assert!(text.contains("export::ORDER"));
    assert!(text.contains("all.sort_by(|a, b| (a.0, a.1.name).cmp(&(b.0, b.1.name)))"));
}
