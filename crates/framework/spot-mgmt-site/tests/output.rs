//! Redaction, capping and parsing of what a command printed, and the argument builders.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use spot_mgmt_site::args;
use spot_mgmt_site::output::{self, Keep};

#[test]
fn url_credentials_and_secretish_fields_are_redacted() {
    assert_eq!(
        output::scrub("dump postgres://u:p%40ss@h:5432/db done", Keep::Nothing),
        "dump postgres://***@h:5432/db done"
    );
    assert_eq!(
        output::scrub("host=h password=abc key_id=7 digest=ff", Keep::Nothing),
        "host=h password=*** key_id=*** digest=ff"
    );
    assert_eq!(
        output::scrub("confirm_token=abc expires_in=600s", Keep::ConfirmToken),
        "confirm_token=abc expires_in=600s"
    );
    assert_eq!(
        output::scrub("confirm_token=abc", Keep::Nothing),
        "confirm_token=***"
    );
    // A URL without credentials is left alone.
    assert_eq!(
        output::scrub("see https://example.org/a?b=c", Keep::Nothing),
        "see https://example.org/a?b=c"
    );
}

#[test]
fn lines_are_capped_by_count_and_length() {
    let long = "x".repeat(1000);
    let (kept, cut) = output::lines(&long, Keep::Nothing, 10);
    assert!(cut);
    assert_eq!(kept[0].chars().count(), output::MAX_LINE_CHARS);
    let many: String = (0..30).map(|n| format!("l{n}\n")).collect();
    let (kept, cut) = output::lines(&many, Keep::Nothing, 10);
    assert!(cut);
    assert_eq!(kept.len(), 10);
    let (kept, cut) = output::lines("a\n\n  \nb\n", Keep::Nothing, 10);
    assert!(!cut);
    assert_eq!(kept, ["a", "b"]);
}

#[test]
fn fields_are_the_name_value_words() {
    let lines = vec![
        "preflight site=demo backup=b-1".to_owned(),
        "pg_major=16 free_bytes_estimate=unknown".to_owned(),
        "next: spot restore apply --site demo --confirm <token>".to_owned(),
        "Bad=ignored empty=".to_owned(),
    ];
    let map = output::fields(&lines);
    assert_eq!(map["site"], "demo");
    assert_eq!(map["pg_major"], "16");
    assert!(!map.contains_key("Bad"));
    assert!(!map.contains_key("empty"));
}

#[test]
fn the_backup_list_parses_and_skips_the_header() {
    let lines = vec![
        "target nightly (1 backups of site demo)".to_owned(),
        "b-1 1793500000 bytes=10 verified encrypted digest=abc".to_owned(),
        "not a backup line".to_owned(),
    ];
    let backups = output::backups(&lines);
    assert_eq!(backups.len(), 1);
    assert_eq!(backups[0]["digest"], "abc");
}

#[test]
fn a_value_that_looks_like_a_flag_stays_one_argument() {
    let line = args::maintenance_enter("demo", "--site=other");
    assert_eq!(line.argv.last().unwrap(), "--reason=--site=other");
    assert_eq!(
        line.argv
            .iter()
            .filter(|a| a.starts_with("--site="))
            .count(),
        1
    );
}

#[test]
fn the_equivalent_line_hides_the_secret() {
    let line = args::restore_apply("demo", "b-1", "nightly", "tok");
    assert!(line.display.ends_with("--confirm <prompt>"));
    assert!(!line.display.contains("tok"));
    assert!(line.argv.iter().any(|a| a == "--confirm=tok"));
}
