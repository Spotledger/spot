//! The completeness gate on the real catalogue, plus the help index, lint and skeleton writer.
#![allow(clippy::expect_used, clippy::unwrap_used)]

mod common;

use std::path::PathBuf;

use spot_errors::Catalogue;
use spot_unattended_support::coverage::{check, derive_help, user_visible, write_skeletons};
use spot_unattended_support::help::{Audience, Help, HelpIndex, locale_chain};
use spot_unattended_support::lint::lint_help;

fn docs_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../docs")
}

#[test]
fn every_user_visible_code_has_clean_help_and_real_documents() {
    let catalogue = Catalogue::load();
    assert_eq!(catalogue.load_error(), None);
    assert!(!user_visible(catalogue).is_empty());
    let findings = check(catalogue, HelpIndex::load(), Some(&docs_root()));
    let lines: Vec<String> = findings.errors.iter().map(ToString::to_string).collect();
    assert!(findings.is_clean(), "{}", lines.join("\n"));
    assert!(findings.missing.is_empty());
}

#[test]
fn the_unattended_support_codes_are_catalogued_and_have_no_todo_help() {
    let catalogue = Catalogue::load();
    for code in [
        "unattended_support.help_missing",
        "unattended_support.spool_failed",
        "unattended_support.sink_failed",
    ] {
        assert!(catalogue.lookup(code).is_some(), "{code} is not catalogued");
    }
    for help in HelpIndex::load().entries() {
        let all = format!(
            "{} {} {}",
            help.what_happened,
            help.why,
            help.what_to_do.join(" ")
        );
        assert!(!all.to_lowercase().contains("todo"), "{}", help.code);
    }
}

fn sample(code: &str) -> Help {
    Help {
        code: code.to_owned(),
        locale: "en".to_owned(),
        what_happened: "The document could not be saved.".to_owned(),
        why: "Someone else changed it first.".to_owned(),
        what_to_do: vec!["Reload the page and try again.".to_owned()],
        learn_more: None,
        audience: Audience::User,
    }
}

#[test]
fn the_gate_fails_on_a_missing_orphan_and_bad_document() {
    let catalogue = Catalogue::load();
    let visible = user_visible(catalogue);
    let first = visible.first().expect("one code").code.clone();
    // An index holding only an orphan entry and a bad document reference.
    let mut orphan = sample("nothing.here");
    orphan.learn_more = Some("process/does-not-exist".to_owned());
    let text = spot_unattended_support::coverage::render_file("nothing", &[orphan]);
    let index = HelpIndex::load_files(&[("nothing.toml", &text)]).expect("parses");
    let findings = check(catalogue, &index, Some(&docs_root()));
    assert!(findings.missing.contains(&first));
    let codes: Vec<&str> = findings.errors.iter().map(|e| e.code()).collect();
    assert!(codes.contains(&"unattended_support.help_missing"));
    assert!(codes.contains(&"unattended_support.help_orphan"));
    // The orphan's document is checked too, and an escaping id never resolves.
    let mut escaping = sample("nothing.here");
    escaping.learn_more = Some("../Cargo".to_owned());
    let text = spot_unattended_support::coverage::render_file("nothing", &[escaping]);
    let index = HelpIndex::load_files(&[("nothing.toml", &text)]).expect("parses");
    let findings = check(catalogue, &index, Some(&docs_root()));
    assert!(
        findings
            .errors
            .iter()
            .any(|e| e.code() == "unattended_support.help_bad_doc")
    );
}

#[test]
fn a_real_document_id_passes() {
    let mut help = sample("document.not_found");
    help.learn_more = Some("process/pipeline".to_owned());
    let text = spot_unattended_support::coverage::render_file("document", &[help]);
    let index = HelpIndex::load_files(&[("document.toml", &text)]).expect("parses");
    let findings = check(Catalogue::load(), &index, Some(&docs_root()));
    assert!(
        !findings
            .errors
            .iter()
            .any(|e| e.code() == "unattended_support.help_bad_doc"),
        "the pipeline notes must exist"
    );
}

#[test]
fn lint_catches_long_sentences_jargon_traces_and_placeholders() {
    assert!(lint_help(&sample("a.b")).is_empty());
    let mut h = sample("a.b");
    h.what_happened = "word ".repeat(40);
    assert!(
        lint_help(&h)
            .iter()
            .any(|p| p.contains("sentence of 40 words"))
    );
    let mut h = sample("a.b");
    h.why = "The thread panicked while holding a mutex.".to_owned();
    let problems = lint_help(&h);
    assert!(problems.iter().any(|p| p.contains("panicked")));
    assert!(problems.iter().any(|p| p.contains("mutex")));
    let mut h = sample("a.b");
    h.what_to_do = vec!["See src/main.rs:12 for details.".to_owned()];
    assert!(lint_help(&h).iter().any(|p| p.contains("stack trace")));
    let mut h = sample("a.b");
    h.what_happened = "Failed for {field}.".to_owned();
    assert!(lint_help(&h).iter().any(|p| p.contains("placeholder")));
    let mut h = sample("a.b");
    h.what_to_do.clear();
    assert!(lint_help(&h).iter().any(|p| p.contains("no steps")));
}

#[test]
fn lookup_falls_back_from_region_to_language_to_english() {
    let en = sample("a.b");
    let mut de = sample("a.b");
    de.locale = "de".to_owned();
    de.what_happened = "Das Dokument konnte nicht gespeichert werden.".to_owned();
    let text = spot_unattended_support::coverage::render_file("a", &[en, de.clone()]);
    // The renderer does not write the locale (skeletons are English); add it by hand.
    let text = text.replacen(
        "code = \"a.b\"\nwhat_happened = \"Das",
        "code = \"a.b\"\nlocale = \"de\"\nwhat_happened = \"Das",
        1,
    );
    let index = HelpIndex::load_files(&[("a.toml", &text)]).expect("parses");
    assert_eq!(
        index.lookup("a.b", "de-CH").map(|h| h.locale.as_str()),
        Some("de")
    );
    assert_eq!(
        index.lookup("a.b", "fr").map(|h| h.locale.as_str()),
        Some("en")
    );
    assert_eq!(
        index
            .lookup("a.b", "de_AT")
            .map(|h| h.what_happened.clone()),
        Some(de.what_happened)
    );
    assert!(index.lookup("x.y", "en").is_none());
    assert_eq!(locale_chain("pt-BR"), ["pt-BR", "pt", "en"]);
    assert_eq!(locale_chain(""), ["en"]);
}

#[test]
fn malformed_and_duplicate_help_is_reported() {
    let errors = HelpIndex::load_files(&[("bad.toml", "[[help]]\ncode = 1")]).unwrap_err();
    assert_eq!(errors[0].code(), "unattended_support.help_malformed");
    let one = spot_unattended_support::coverage::render_file("a", &[sample("a.b")]);
    let both = format!(
        "{one}{}",
        one.split_once("\n[[help]]")
            .map(|(_, t)| format!("\n[[help]]{t}"))
            .unwrap_or_default()
    );
    let errors = HelpIndex::load_files(&[("a.toml", &both)]).unwrap_err();
    assert!(errors[0].message().contains("a second entry"));
}

#[test]
fn skeletons_are_written_for_missing_codes_and_then_pass_the_gate() {
    let catalogue = Catalogue::load();
    let dir = common::temp_dir("skeletons");
    let findings = check(catalogue, &HelpIndex::default(), None);
    assert!(!findings.missing.is_empty());
    let files = write_skeletons(catalogue, &findings.missing, &dir).expect("written");
    assert!(!files.is_empty());
    let mut loaded = Vec::new();
    for file in &files {
        loaded.push((
            file.file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned(),
            std::fs::read_to_string(file).expect("read"),
        ));
    }
    let refs: Vec<(&str, &str)> = loaded
        .iter()
        .map(|(n, t)| (n.as_str(), t.as_str()))
        .collect();
    let index = HelpIndex::load_files(&refs).expect("skeletons parse");
    let findings = check(catalogue, &index, Some(&docs_root()));
    let lines: Vec<String> = findings.errors.iter().map(ToString::to_string).collect();
    assert!(findings.is_clean(), "{}", lines.join("\n"));
    // Running it again adds nothing and keeps the files parseable.
    let again = write_skeletons(catalogue, &[], &dir).expect("nothing to do");
    assert!(again.is_empty());
}

#[test]
fn derived_help_is_plain_and_names_the_code_to_quote() {
    let catalogue = Catalogue::load();
    let entry = catalogue.lookup("document.required").expect("entry");
    let help = derive_help(entry);
    assert_eq!(
        help.what_happened,
        "A required field or child table has no value."
    );
    assert_eq!(help.audience, Audience::User);
    assert!(
        help.what_to_do
            .last()
            .expect("step")
            .contains("document.required")
    );
    assert!(lint_help(&help).is_empty());
    let internal = derive_help(catalogue.lookup("http.internal").expect("entry"));
    assert_eq!(internal.audience, Audience::Operator);
}
