//! WP-ADM-02.T6: every `agent.*` code the agent source can emit is in the error catalogue
//! (`spot-errors/catalogue/agent.toml`), so a code added without an entry fails here.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// File names that look like codes (`agent.toml`, the key and log files in the state dir).
const NOT_CODES: &[&str] = &["agent.toml", "agent.key", "agent.log"];

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

/// Every `"agent.<name>"` literal in the production part of `text`: comment lines and the
/// `#[cfg(test)]` tail of a file are skipped.
fn literals(text: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") {
            break;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        let mut rest = line;
        while let Some(start) = rest.find("\"agent.") {
            let tail = &rest[start + 1..];
            let Some(end) = tail.find('"') else { break };
            let literal = &tail[..end];
            if !literal.is_empty()
                && literal
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'.')
                && !NOT_CODES.contains(&literal)
            {
                found.insert(literal.to_owned());
            }
            rest = &tail[end + 1..];
        }
    }
    found
}

fn catalogued() -> BTreeSet<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../spot-errors/catalogue/agent.toml");
    let text = std::fs::read_to_string(path).unwrap();
    let doc: toml::Value = toml::from_str(&text).unwrap();
    doc["entry"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["code"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn every_emitted_agent_code_is_catalogued() {
    let mut files = Vec::new();
    rust_files(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(!files.is_empty());
    let known = catalogued();
    let mut missing = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        for code in literals(&text) {
            if !known.contains(&code) {
                missing.push(format!("{code} ({})", file.display()));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "codes emitted but not in spot-errors/catalogue/agent.toml: {missing:?}"
    );
}

#[test]
fn the_error_type_codes_are_all_catalogued() {
    use spot_agent::error::AgentError;
    let known = catalogued();
    let all = [
        AgentError::ConfigInvalid(String::new()),
        AgentError::NotEnrolled,
        AgentError::AlreadyEnrolled,
        AgentError::KeyStoreUnavailable,
        AgentError::StatePermissions(String::new()),
        AgentError::StateIo(String::new()),
        AgentError::TokenInvalid,
        AgentError::GatewayProtocol(String::new()),
        AgentError::Transport(String::new()),
        AgentError::TransportUnavailable,
        AgentError::AuditFailed(String::new()),
        AgentError::Usage(String::new()),
        AgentError::SoftwareKeysNotAllowed,
    ];
    for error in all {
        assert!(
            known.contains(error.code()),
            "{} is not catalogued",
            error.code()
        );
    }
}

#[test]
fn the_scan_finds_a_missing_code() {
    let found = literals("fn f() { let _ = \"agent.brand_new_code\"; }\n// \"agent.in_comment\"\n");
    assert!(found.contains("agent.brand_new_code"));
    assert!(!found.contains("agent.in_comment"));
    assert!(!catalogued().contains("agent.brand_new_code"));
}
