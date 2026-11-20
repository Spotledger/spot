//! The completeness guard (WP-ALM-05.T4): every table any crate of the repository creates is
//! classified in `spot_sandbox::tables`, and every classified table still exists. No database.
//!
//! The scan is a guard, not a SQL parser: it finds `CREATE TABLE [IF NOT EXISTS] <name>` in the
//! `.rs` files under `crates/` (skipping `tests/` directories, whose tables are fixtures) whose name starts `spot_` and
//! compares the names with the registry.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use spot_sandbox::tables::{PREFIX_RULES, TABLES, TableTreatment, treatment_of};

/// Names created only by fixtures and examples inside `src/` files (a `tests/` directory is
/// skipped as a whole). Each says where it comes from.
const TEST_ONLY: &[&str] = &[
    // spot-pg-migrate/src/extra.rs: unit-test and doc examples of the extra-table creator.
    "spot_a", "spot_b", "spot_x", "spot_X",
];

fn repo_crates() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../crates")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name == "tests" || name == "target" || name.starts_with('.') {
                continue;
            }
            rust_files(&path, out);
        } else if name.ends_with(".rs") {
            out.push(path);
        }
    }
}

fn is_separator(c: char) -> bool {
    c.is_whitespace() || c == '\\' || c == '"' || c == '`'
}

/// Drops separators from the front, including the two characters `\n`, `\t` and `\r` of an escape
/// written in the source.
fn skip_separators(mut text: &str) -> &str {
    loop {
        if let Some(rest) = ["\\n", "\\t", "\\r"]
            .iter()
            .find_map(|escape| text.strip_prefix(escape))
        {
            text = rest;
        } else if let Some(rest) = text.strip_prefix(is_separator) {
            text = rest;
        } else {
            return text;
        }
    }
}

/// The table names after `CREATE TABLE` in `text`, with `IF NOT EXISTS` skipped.
fn created_tables(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find("CREATE TABLE") {
        rest = &rest[at + "CREATE TABLE".len()..];
        let mut tail = skip_separators(rest);
        for keyword in ["IF", "NOT", "EXISTS"] {
            if let Some(after) = tail.strip_prefix(keyword)
                && after.starts_with(is_separator)
            {
                tail = skip_separators(after);
            }
        }
        let name: String = tail
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
            .collect();
        let starts_ok = name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        // Every table the platform creates is `spot_`-prefixed; a bare name in a comment ("CREATE
        // TABLE statements") or a fixture is not one, and a document table is governed by the
        // metadata, not by this registry.
        if starts_ok && name.starts_with("spot_") {
            found.push(name);
        }
    }
    found
}

/// The tables created in the repository, each with the first file that creates it.
fn scan() -> BTreeMap<String, String> {
    let root = repo_crates();
    let mut files = Vec::new();
    rust_files(&root, &mut files);
    assert!(
        !files.is_empty(),
        "no .rs files found under {}",
        root.display()
    );
    let mut found = BTreeMap::new();
    for file in files {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for name in created_tables(&text) {
            found.entry(name).or_insert_with(|| {
                file.strip_prefix(&root)
                    .unwrap_or(&file)
                    .display()
                    .to_string()
            });
        }
    }
    found
}

#[test]
fn every_created_table_is_classified() {
    let mut missing = Vec::new();
    for (name, file) in scan() {
        if TEST_ONLY.contains(&name.as_str()) || treatment_of(&name).is_some() {
            continue;
        }
        missing.push(format!(
            "table `{name}` (created in crates/{file}) is not classified: \
             classify it in spot-sandbox/src/tables.rs"
        ));
    }
    assert!(missing.is_empty(), "\n{}", missing.join("\n"));
}

#[test]
fn every_listed_table_is_still_created_somewhere() {
    let found = scan();
    let mut stale = Vec::new();
    for rule in TABLES {
        if rule.external.is_none() && !found.contains_key(rule.name) {
            stale.push(rule.name);
        }
    }
    assert!(
        stale.is_empty(),
        "tables classified in spot-sandbox/src/tables.rs that no crate creates (remove them, or \
         mark `external` with a reason): {stale:?}"
    );
}

#[test]
fn an_external_table_says_why_and_a_prefix_rule_has_a_family() {
    for rule in TABLES {
        if let Some(reason) = rule.external {
            assert!(!reason.is_empty(), "{}", rule.name);
        }
    }
    let found = scan();
    for prefix in PREFIX_RULES {
        assert!(!prefix.why.is_empty(), "{}", prefix.prefix);
        // A prefix family earns its place: it matches a created table or names a family that
        // is generated (`why` says so); the control schemas are matched by the scan itself.
        let matched = found.keys().any(|name| name.starts_with(prefix.prefix));
        let generated = prefix.why.contains("per ") || prefix.why.contains("derived");
        assert!(matched || generated, "{} matches nothing", prefix.prefix);
    }
}

#[test]
fn no_keep_table_has_a_sensitive_name_without_a_reason() {
    assert_eq!(
        spot_sandbox::tables::keep_without_reason(),
        Vec::<&str>::new()
    );
    for rule in TABLES
        .iter()
        .filter(|r| r.treatment == TableTreatment::Keep)
    {
        if rule.allow_keep_because.is_none() {
            for word in ["key", "secret", "token", "nonce", "audit", "outbox", "mail"] {
                assert!(
                    !rule.name.contains(word),
                    "{} is Keep but looks sensitive",
                    rule.name
                );
            }
        }
    }
}

// ---- the scanner itself ----

#[test]
fn the_scanner_reads_the_forms_the_repository_uses() {
    let text = "
        \"CREATE TABLE IF NOT EXISTS spot_a (id int)\"
        \"CREATE TABLE spot_b (id int)\"
        \"CREATE TABLE IF NOT EXISTS \\\"spot_c\\\" (id int)\"
        \"CREATE TABLE IF NOT EXISTS\\n   spot_d (id int)\"
        \"CREATE TABLE IF NOT EXISTS {} (id int)\"
        \"CREATE TABLE IF NOT EXISTS spot_control.thing (id int)\"
        // CREATE TABLE AS is not a table name
        \"CREATE TABLE AS SELECT 1\"
    ";
    assert_eq!(
        created_tables(text),
        ["spot_a", "spot_b", "spot_c", "spot_d", "spot_control.thing"]
    );
}

#[test]
fn the_scanner_finds_a_table_nobody_classified() {
    let found = created_tables("CREATE TABLE IF NOT EXISTS spot_brand_new_table (id int)");
    assert_eq!(found, ["spot_brand_new_table"]);
    assert!(treatment_of(&found[0]).is_none());
}
