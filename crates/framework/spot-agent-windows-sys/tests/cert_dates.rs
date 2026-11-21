//! Pure-function tests for [`spot_agent_windows_sys::parse_certutil_dump_validity`] over
//! recorded `certutil -dump` output in several locales (checked in as fixtures so these run
//! on the Linux CI host, not just a real Windows machine).
//!
//! Every locale fixture encodes the same instants — `2024-01-15T00:00:00Z` to
//! `2026-01-15T23:59:59Z` — so every locale's parse must agree on the same two Unix
//! timestamps regardless of how its `NotBefore`/`NotAfter` text is shaped.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_agent_windows_sys::parse_certutil_dump_validity;

const EXPECTED_NOT_BEFORE: i64 = 1_705_276_800; // 2024-01-15T00:00:00Z
const EXPECTED_NOT_AFTER: i64 = 1_768_521_599; // 2026-01-15T23:59:59Z

#[test]
fn en_us_locale_dump_parses() {
    let dump = include_str!("fixtures/certutil-dump/en-us.txt");
    let (before, after) = parse_certutil_dump_validity(dump).unwrap();
    assert_eq!(before, EXPECTED_NOT_BEFORE);
    assert_eq!(after, EXPECTED_NOT_AFTER);
}

#[test]
fn en_gb_locale_dump_parses() {
    let dump = include_str!("fixtures/certutil-dump/en-gb.txt");
    let (before, after) = parse_certutil_dump_validity(dump).unwrap();
    assert_eq!(before, EXPECTED_NOT_BEFORE);
    assert_eq!(after, EXPECTED_NOT_AFTER);
}

#[test]
fn de_de_locale_dump_parses() {
    let dump = include_str!("fixtures/certutil-dump/de-de.txt");
    let (before, after) = parse_certutil_dump_validity(dump).unwrap();
    assert_eq!(before, EXPECTED_NOT_BEFORE);
    assert_eq!(after, EXPECTED_NOT_AFTER);
}

#[test]
fn fr_fr_locale_dump_parses() {
    let dump = include_str!("fixtures/certutil-dump/fr-fr.txt");
    let (before, after) = parse_certutil_dump_validity(dump).unwrap();
    assert_eq!(before, EXPECTED_NOT_BEFORE);
    assert_eq!(after, EXPECTED_NOT_AFTER);
}

#[test]
fn powershell_iso_output_parses() {
    let dump = include_str!("fixtures/certutil-dump/powershell-iso.txt");
    let (before, after) = parse_certutil_dump_validity(dump).unwrap();
    assert_eq!(before, EXPECTED_NOT_BEFORE);
    assert_eq!(after, EXPECTED_NOT_AFTER);
}

/// A locale format this parser does not support (a named month) must be a typed error —
/// never silently treated as valid or expired.
#[test]
fn an_unsupported_locale_format_is_an_error_not_a_silent_pass() {
    let dump = include_str!("fixtures/certutil-dump/unsupported-named-month.txt");
    let error = parse_certutil_dump_validity(dump).unwrap_err();
    assert_eq!(error.code(), "agent_windows_sys.io");
}
