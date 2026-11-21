//! The resource-file scanner `build.rs` runs, tested through the same source file.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

#[path = "../build/scan.rs"]
mod scan;

fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

#[test]
fn files_are_listed_in_name_order_without_mod_rs() {
    let got = scan::module_names(&names(&["report.rs", "mod.rs", "function.rs", "notes.md"]));
    assert_eq!(got, Ok(names(&["function", "report"])));
}

#[test]
fn a_bad_file_name_fails_naming_the_file() {
    for bad in [
        "Webhooks.rs",
        "my-group.rs",
        "9lives.rs",
        "a b.rs",
        "type.rs",
    ] {
        let got = scan::module_names(&names(&["function.rs", bad]));
        let message = got.expect_err("a bad name must fail");
        assert!(message.contains(bad), "{message}");
    }
}

#[test]
fn the_registry_declares_one_path_module_per_file() {
    let text = scan::registry_source("/x/src/resources", &names(&["function", "report"]));
    assert!(text.contains("#[path = \"/x/src/resources/function.rs\"]\npub mod function;"));
    assert!(text.contains("pub mod report;"));
    assert_eq!(text.matches("pub mod").count(), 2);
}
