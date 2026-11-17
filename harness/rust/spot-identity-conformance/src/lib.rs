//! Helpers of the ADR-083 conformance suite (WP-IAM-11.T6).
//!
//! The suite compares what a router or a table **does** with what ADR-083 says it must do, and the
//! comparisons are plain functions that return the list of failures instead of panicking, so a
//! meta-test can feed them a table that has been tampered with and prove they notice. The tests
//! are in `tests/`; this file holds what both the tests and the meta-test use.
//!
//! This crate is test code. No production crate may depend on it (see the README).

use spot_core::Audience;
use spot_http::api::{RouteAuth, RouteEntry};

/// One failure, naming the barrier and the task that owns the missing piece.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// The stable label (an `identity_conformance.*` code).
    pub code: &'static str,
    /// What was wrong, with the route (method and pattern) and never a credential.
    pub detail: String,
}

/// Label: a wrong-audience token was not refused (Barrier 1).
pub const AUDIENCE_NOT_REFUSED: &str = "identity_conformance.audience_not_refused";
/// Label: a listener mounts the wrong routes (Barrier 2).
pub const SURFACE_MISMATCH: &str = "identity_conformance.surface_mismatch";
/// Label: a default is open (Barrier 3, B7).
pub const OPEN_DEFAULT: &str = "identity_conformance.open_default";

/// A request path for an axum pattern: each `{name}` becomes a fixed, valid-looking sample.
#[must_use]
pub fn sample_path(pattern: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for ch in pattern.chars() {
        match ch {
            '{' => {
                inside = true;
                out.push_str("sample1");
            }
            '}' => inside = false,
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    out
}

/// The routes marked for `audience`, as `METHOD path` lines, sorted.
#[must_use]
pub fn surface_of(table: &[RouteEntry], audience: Audience) -> Vec<String> {
    let mut lines: Vec<String> = table
        .iter()
        .filter(|entry| entry.audiences.contains(&audience))
        .map(|entry| format!("{} {}", entry.method, entry.path))
        .collect();
    lines.sort();
    lines
}

/// The lines of a snapshot file: comments (`#`) and blanks dropped, sorted.
#[must_use]
pub fn parse_snapshot(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(str::to_owned)
        .collect();
    lines.sort();
    lines
}

/// B2: the consumer surface of `table` against the pinned snapshot.
#[must_use]
pub fn consumer_surface_failures(table: &[RouteEntry], snapshot: &[String]) -> Vec<Failure> {
    let actual = surface_of(table, Audience::Consumer);
    let mut failures = Vec::new();
    for line in actual.iter().filter(|line| !snapshot.contains(line)) {
        failures.push(Failure {
            code: SURFACE_MISMATCH,
            detail: format!("`{line}` is marked consumer but is not in consumer-surface.snapshot"),
        });
    }
    for line in snapshot.iter().filter(|line| !actual.contains(line)) {
        failures.push(Failure {
            code: SURFACE_MISMATCH,
            detail: format!("`{line}` is in consumer-surface.snapshot but not marked consumer"),
        });
    }
    failures
}

/// B2: the routes a router mounted (`mounted`, as `METHOD path` lines) against the table entries
/// that allow one of `audiences`: a route with no table entry, or an entry nothing mounts, fails.
#[must_use]
pub fn route_set_failures(
    table: &[RouteEntry],
    audiences: &[Audience],
    mounted: &[String],
    unrouted: &[&str],
) -> Vec<Failure> {
    let expected: Vec<String> = table
        .iter()
        .filter(|entry| entry.audiences.iter().any(|a| audiences.contains(a)))
        .filter(|entry| !unrouted.contains(&entry.operation))
        .map(|entry| format!("{} {}", entry.method, entry.path))
        .collect();
    let mut failures = Vec::new();
    for line in mounted.iter().filter(|line| !expected.contains(line)) {
        failures.push(Failure {
            code: SURFACE_MISMATCH,
            detail: format!("`{line}` is mounted but the table does not list it for {audiences:?}"),
        });
    }
    for line in expected.iter().filter(|line| !mounted.contains(line)) {
        failures.push(Failure {
            code: SURFACE_MISMATCH,
            detail: format!("`{line}` is listed for {audiences:?} but nothing mounts it"),
        });
    }
    failures
}

/// B1 (table half): every route that needs a credential is marked for at least one audience,
/// never for none, and a route that is `RouteAuth::None` is one of the `open` operations.
#[must_use]
pub fn mark_failures(table: &[RouteEntry], open: &[&str]) -> Vec<Failure> {
    let mut failures = Vec::new();
    for entry in table {
        if entry.audiences.is_empty() {
            failures.push(Failure {
                code: OPEN_DEFAULT,
                detail: format!("{} {} has no audience mark", entry.method, entry.path),
            });
        }
        if entry.auth == RouteAuth::None && !open.contains(&entry.operation) {
            failures.push(Failure {
                code: AUDIENCE_NOT_REFUSED,
                detail: format!(
                    "{} {} needs no credential and is not a listed unauthenticated route",
                    entry.method, entry.path
                ),
            });
        }
    }
    failures
}
