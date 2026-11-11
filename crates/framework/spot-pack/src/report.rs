//! The rendered forms of a report: human text by default, JSON with `--json`.
//!
//! Both forms carry the same facts — code, path, message per problem; the app identity,
//! digest, entries, capabilities and publisher for an inspect — so scripts and authors read
//! the same run.

use crate::check::CheckReport;
use crate::error::PackProblem;
use crate::inspect::InspectReport;

/// Renders a check report as human text: one line per problem, `ok` when none.
#[must_use]
pub fn check_text(report: &CheckReport) -> String {
    if report.problems.is_empty() {
        return format!("ok: the app checks clean ({} problem{})", 0, plural(0));
    }
    let mut lines = format!(
        "{} problem{}:\n",
        report.problems.len(),
        plural(report.problems.len())
    );
    for problem in &report.problems {
        lines.push_str(&format!("  {problem}\n"));
    }
    lines
}

/// Renders an inspect report as human text.
#[must_use]
pub fn inspect_text(report: &InspectReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("app: {} {}\n", report.app_id, report.version));
    out.push_str(&format!("digest: {}\n", report.digest));
    if let Some(publisher) = &report.publisher {
        out.push_str(&format!("publisher: {publisher}\n"));
    }
    out.push_str("entries:\n");
    for entry in &report.entries {
        out.push_str(&format!("  {} ({} bytes)\n", entry.name, entry.size));
    }
    out.push_str("capabilities:\n");
    if report.capabilities.network.is_empty() {
        out.push_str("  network: none\n");
    } else {
        out.push_str(&format!(
            "  network: {}\n",
            report.capabilities.network.join(", ")
        ));
    }
    out.push_str(&format!("  email: {}\n", on_off(report.capabilities.email)));
    out.push_str(&format!("  files: {}\n", report.capabilities.files));
    out.push_str(&format!(
        "  doctypes read: {}\n",
        join_or(&report.capabilities.doctypes_read, "none")
    ));
    out.push_str(&format!(
        "  doctypes write: {}\n",
        join_or(&report.capabilities.doctypes_write, "none")
    ));
    if !report.steps.is_empty() {
        out.push_str(&format!("steps: {}\n", report.steps.join(", ")));
    }
    if !report.imports.is_empty() {
        out.push_str(&format!("imports: {}\n", report.imports.join(", ")));
    }
    if !report.exports.is_empty() {
        out.push_str(&format!("exports: {}\n", report.exports.join(", ")));
    }
    out
}

/// The JSON form of a check report.
///
/// # Errors
/// Never in practice: the values are strings and booleans.
pub fn check_json(report: &CheckReport) -> Result<String, serde_json::Error> {
    let problems: Vec<serde_json::Value> = report.problems.iter().map(problem_json).collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "ok": report.problems.is_empty(),
        "problems": problems,
    }))
}

/// The JSON form of one problem: `{ code, path, message }`, `path` null when none.
#[must_use]
pub fn problem_json(problem: &PackProblem) -> serde_json::Value {
    serde_json::json!({
        "code": problem.code,
        "path": problem.path,
        "message": problem.message,
    })
}

/// The JSON form of an inspect report.
///
/// # Errors
/// Never in practice.
pub fn inspect_json(report: &InspectReport) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&serde_json::json!({
        "app_id": report.app_id,
        "version": report.version,
        "digest": report.digest,
        "publisher": report.publisher,
        "entries": report
            .entries
            .iter()
            .map(|e| serde_json::json!({ "name": e.name, "size": e.size }))
            .collect::<Vec<_>>(),
        "capabilities": {
            "network": report.capabilities.network,
            "email": report.capabilities.email,
            "files": report.capabilities.files,
            "doctypes_read": report.capabilities.doctypes_read,
            "doctypes_write": report.capabilities.doctypes_write,
        },
        "steps": report.steps,
        "imports": report.imports,
        "exports": report.exports,
    }))
}

/// The JSON form of a successful verify.
///
/// # Errors
/// Never in practice.
pub fn proof_json(proof: &crate::signing::BundleProof) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&serde_json::json!({
        "ok": true,
        "publisher_id": proof.publisher_id,
        "key_id": proof.key_id.as_str(),
        "signed_at": proof.signed_at,
        "revocation_generation": proof.revocation_generation,
        "digest": proof.digest.to_hex(),
    }))
}

/// The JSON form of a verify refusal.
///
/// # Errors
/// Never in practice.
pub fn refusal_json(refusal: &crate::error::VerifyRefusal) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(&serde_json::json!({
        "ok": false,
        "code": refusal.code(),
        "message": refusal.0.to_string(),
    }))
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

fn on_off(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

fn join_or(values: &[String], empty: &str) -> String {
    if values.is_empty() {
        empty.to_owned()
    } else {
        values.join(", ")
    }
}
