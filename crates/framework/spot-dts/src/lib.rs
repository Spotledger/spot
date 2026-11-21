//! The TypeScript declaration generator (WP-EXT-09.T1; ADR-054).
//!
//! One crate, one job: turn the site's **effective** DocType definitions — custom fields already
//! merged in by `spot-meta`, so this crate never learns where a field came from — into one
//! `.d.ts` text that scripts type-check against in the editor. It is the TypeScript twin of
//! `spot-typed-views` (same input model, same storage-name grammar, different output), and it is
//! byte-stable for the same input: DocTypes sorted by name, fields in definition order.
//!
//! # The document a script really receives
//!
//! A script sees documents JSON-shaped, the shape the document API emits and the host bridge
//! passes through: the standard fields (`id`, `version`, the `_state` lifecycle marker) plus the
//! DocType's own. The declarations describe exactly that ([`DOCUMENT_BASE`] in `emit.rs` lists
//! them); nothing is invented. Per ADR-068 section 2, a `Secret` field's value never reaches a
//! document, so it is never declared — the interface documents the absence instead. A `Decimal`
//! travels as its exact string: no float ever reaches a script (ADR-068 section 2, ADR-12).
//!
//! # Where TypeScript differs from the Rust twin
//!
//! Rust maps storage names through its keyword system (`type` -> `r#type`, a `Self` variant
//! becomes `SelfDoc`); TypeScript has no raw identifiers, so a reserved word becomes a **quoted
//! property**, and DocType names colliding with reserved words are errors, not suffixes. The
//! choices are written down in [`naming`] and [`emit`].
//!
//! # Shape
//!
//! - [`generate`]: the whole site in one file, with a [`Fingerprint`] so an editor reloads only
//!   when the text really changed ([`changed`]).
//! - [`generate_for`]: only the DocTypes a script names plus everything they reference
//!   (children, link targets), so one script's declarations stay small. Cycles terminate.
//! - [`tsconfig_snippet`]: the `compilerOptions`/`include` that points an editor at the file.
//!   No file watching here; the dev server (WP-DX-02) will call this crate.

use std::collections::BTreeSet;

use spot_meta::Definition;

pub mod emit;
pub mod error;
pub mod naming;

pub use emit::{DOCUMENT_BASE, Generated};
pub use error::{DtsError, DtsErrorKind};

/// The fingerprint of one generated file: the SHA-256 hex digest of the text.
///
/// The type is a plain `String` alias so an editor integration can store, log and diff it
/// without touching the crate's types.
pub type Fingerprint = String;

/// The SHA-256 hex digest of `text`.
#[must_use]
pub fn fingerprint(text: &str) -> Fingerprint {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Whether the generated file changed against `prev`: true only when `next`'s fingerprint
/// differs. An editor integration calls this before reloading; an unrelated edit (a label on a
/// view) never changes the text, hence the fingerprint.
#[must_use]
pub fn changed(prev: &Fingerprint, next: &Generated) -> bool {
    *prev != next.fingerprint
}

/// Generates the declarations of every `definitions` entry.
///
/// Deterministic: DocTypes sorted by storage name, fields in definition order, sections in a
/// fixed order (header, `DocumentBase`, interfaces, `spot` namespace). The same input always
/// gives the same bytes; the fingerprint is the SHA-256 of the text.
///
/// # Errors
/// One [`DtsError`] per bad name (a DocType that maps onto no valid interface name), collision
/// (two names mapping onto one interface), unknown reference (a link or table naming a DocType
/// the input does not contain) or a table field with no child DocType. Every error is reported,
/// not just the first.
pub fn generate(definitions: &[Definition]) -> Result<Generated, Vec<DtsError>> {
    let all: Vec<&Definition> = definitions.iter().collect();
    emit::Emitter::new(definitions).emit(&all)
}

/// Generates the declarations for `only` (DocType names a script touches) **and every DocType
/// they reference**: child tables and link targets, transitively, so the generated file stays
/// small and the script sees only what it can reach. DocTypes not reached are absent.
///
/// A DocType named in `only` that the input does not contain is an error. Reference cycles
/// terminate (the walk keeps a visited set); the sorted, deduplicated result is deterministic
/// whatever order `only` lists.
///
/// # Errors
/// The [`generate`] errors plus one per unknown DocType in `only`.
pub fn generate_for(definitions: &[Definition], only: &[&str]) -> Result<Generated, Vec<DtsError>> {
    let mut errors = Vec::new();
    for name in only {
        if !definitions.iter().any(|d| d.name == *name) {
            errors.push(
                DtsError::new(
                    DtsErrorKind::UnknownDocType,
                    format!("`{name}` is not part of the input"),
                )
                .about_name(name),
            );
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    let keep = closure_of(definitions, only);
    let chosen: Vec<&Definition> = definitions
        .iter()
        .filter(|d| keep.contains(&d.name))
        .collect();
    emit::Emitter::new(definitions).emit(&chosen)
}

/// The transitive closure of `roots` over table children and link targets. Deterministic
/// (BTreeSet iteration), cycle-safe (a visited set).
fn closure_of(definitions: &[Definition], roots: &[&str]) -> BTreeSet<String> {
    let mut keep: BTreeSet<String> = BTreeSet::new();
    let mut queue: Vec<String> = roots.iter().map(|r| (*r).to_owned()).collect();
    while let Some(name) = queue.pop() {
        if !keep.insert(name.clone()) {
            continue;
        }
        if let Some(def) = definitions.iter().find(|d| d.name == name) {
            for r in emit::direct_references(def) {
                if definitions.iter().any(|d| d.name == r) && !keep.contains(&r) {
                    queue.push(r);
                }
            }
        }
    }
    keep
}

/// The `compilerOptions` / `include` snippet that points an editor (or `tsc`) at the generated
/// file at `types_path`. The dev server writes it next to the generated file (WP-DX-02 calls
/// this crate; nothing here watches files).
#[must_use]
pub fn tsconfig_snippet(types_path: &str) -> String {
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"compilerOptions\": {\n");
    out.push_str("    \"target\": \"es2022\",\n");
    out.push_str("    \"module\": \"esnext\",\n");
    out.push_str("    \"moduleResolution\": \"bundler\",\n");
    out.push_str("    \"strict\": true,\n");
    out.push_str("    \"noEmit\": true,\n");
    out.push_str("    \"types\": []\n");
    out.push_str("  },\n");
    out.push_str("  \"include\": [");
    out.push_str(&json_string(types_path));
    out.push_str("]\n");
    out.push_str("}\n");
    out
}

/// One JSON string literal (with the quotes).
fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tsconfig_snippet_includes_the_types_path() {
        let snippet = tsconfig_snippet("spot.d.ts");
        assert!(snippet.contains("\"include\": [\"spot.d.ts\"]"));
        assert!(snippet.contains("\"strict\": true"));
    }

    #[test]
    fn fingerprint_is_hex_sha256() {
        let fp = fingerprint("spot");
        assert_eq!(fp.len(), 64);
        assert!(fp.bytes().all(|b| b.is_ascii_hexdigit()));
    }
}
