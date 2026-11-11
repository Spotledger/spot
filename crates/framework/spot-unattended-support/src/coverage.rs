//! The completeness gate: every user-visible error code has help, no help names a code or a
//! document that does not exist, and every entry follows the style rules.
//!
//! **User-visible** means the catalogue entry's class is not `none` and it is not retired: the
//! classes are the ones an `AppError` carries to a user, while `none` marks wire vocabularies
//! and library-internal kinds that no user ever sees; the test suites' `probe.*` and `l3.*`
//! vocabularies are left out too.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use spot_errors::{Catalogue, Entry};

use crate::error::{SupportError, codes};
use crate::help::{Audience, Help, HelpIndex};
use crate::lint::lint_help;

/// Code prefixes that belong to test suites (see the `spot-errors` README: `probe.*`, `l3.*`).
/// No user ever sees them, whatever class they carry.
pub const TEST_PREFIXES: &[&str] = &["probe", "l3"];

/// The catalogue entries a user can see: class other than `none`, not retired, not a test
/// suite's probe vocabulary.
#[must_use]
pub fn user_visible(catalogue: &Catalogue) -> Vec<&Entry> {
    catalogue
        .entries()
        .values()
        .filter(|e| e.class != "none" && !e.retired)
        .filter(|e| {
            let prefix = e.code.split('.').next().unwrap_or_default();
            !TEST_PREFIXES.contains(&prefix)
        })
        .collect()
}

/// What the gate found.
#[derive(Debug, Default)]
pub struct Findings {
    /// Codes (sorted) that are user-visible and have no help.
    pub missing: Vec<String>,
    /// Every problem, as an error with a stable code.
    pub errors: Vec<SupportError>,
}

impl Findings {
    /// Whether the gate passes.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.errors.is_empty()
    }
}

/// Runs the gate. `docs_root` is the repository's `docs/` directory; when `None`, `learn_more`
/// documents are not checked (the test suite and the binary always pass it).
#[must_use]
pub fn check(catalogue: &Catalogue, index: &HelpIndex, docs_root: Option<&Path>) -> Findings {
    let mut findings = Findings::default();
    for entry in user_visible(catalogue) {
        if index.lookup(&entry.code, "en").is_none() {
            findings.missing.push(entry.code.clone());
            findings.errors.push(SupportError::new(
                codes::HELP_MISSING,
                format!("the user-visible code {} has no help entry", entry.code),
            ));
        }
    }
    for help in index.entries() {
        if catalogue.lookup(&help.code).is_none() {
            findings.errors.push(SupportError::new(
                codes::HELP_ORPHAN,
                format!("help names {}, which the catalogue lacks", help.code),
            ));
        }
        if let (Some(doc), Some(root)) = (&help.learn_more, docs_root)
            && !doc_exists(root, doc)
        {
            findings.errors.push(SupportError::new(
                codes::HELP_BAD_DOC,
                format!("help for {} points to missing document {doc}", help.code),
            ));
        }
        for problem in lint_help(help) {
            findings.errors.push(SupportError::new(
                codes::HELP_STYLE,
                format!("{} ({}): {problem}", help.code, help.locale),
            ));
        }
    }
    findings
}

/// Whether `docs/<id>.md` exists. An id that tries to leave `docs/` never does.
fn doc_exists(root: &Path, id: &str) -> bool {
    let safe = !id.is_empty()
        && !id.starts_with('/')
        && id.split('/').all(|part| {
            !part.is_empty()
                && part != ".."
                && part != "."
                && part
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        });
    safe && root.join(format!("{id}.md")).is_file()
}

/// A plain-language help entry derived from a catalogue entry's summary, remedy and class.
/// The text is meant to be reviewed and edited by an author; the derivation only makes sure no
/// code is left without a starting point.
#[must_use]
pub fn derive_help(entry: &Entry) -> Help {
    let what_happened = plain(&entry.summary);
    let (why, audience) = why_and_audience(&entry.class);
    let mut what_to_do = vec![plain(&entry.remedy)];
    what_to_do.push(format!(
        "If it keeps happening, tell your administrator and quote the code {}.",
        entry.code
    ));
    Help {
        code: entry.code.clone(),
        locale: "en".to_owned(),
        what_happened,
        why: why.to_owned(),
        what_to_do,
        learn_more: None,
        audience,
    }
}

fn why_and_audience(class: &str) -> (&'static str, Audience) {
    match class {
        "Validation" => (
            "The information given did not meet a rule the system checks before it saves anything.",
            Audience::User,
        ),
        "NotFound" | "UnknownDocType" | "MethodNotFound" => (
            "The item asked for does not exist, or you are not allowed to see it.",
            Audience::User,
        ),
        "PermissionDenied" => (
            "Your account does not have the permission this action needs.",
            Audience::User,
        ),
        "Duplicate" => (
            "Something with the same identity already exists.",
            Audience::User,
        ),
        "Linked" => (
            "Other records still depend on this one, so it cannot be removed.",
            Audience::User,
        ),
        "StaleVersion" => (
            "Someone else changed the item after you opened it.",
            Audience::User,
        ),
        "InvalidState" => (
            "The item is in a state that does not allow this action right now.",
            Audience::User,
        ),
        "Unavailable" => (
            "A service the action needs is not available at the moment.",
            Audience::Admin,
        ),
        _ => (
            "The system hit a problem of its own that you could not have prevented.",
            Audience::Operator,
        ),
    }
}

/// Turns a developer-facing sentence into plain text: placeholders become words, jargon is
/// swapped for everyday words, and the sentence ends with a full stop.
fn plain(text: &str) -> String {
    let mut text = text.trim().to_owned();
    // "Something is wrong: {detail}." and "...: expected {a}, got {b}." keep the sentence and
    // drop the values, which belong in the log and not in the help.
    if let Some(open) = text.find('{')
        && let Some(colon) = text[..open].rfind(": ")
    {
        text.truncate(colon);
    }
    // Any placeholder left in the middle of a sentence is dropped with its quotes.
    let mut out = String::new();
    let mut rest = text.as_str();
    while let Some(open) = rest.find('{') {
        let Some(len) = rest[open..].find('}') else {
            break;
        };
        out.push_str(rest[..open].trim_end_matches(['`', '\'', '"']));
        rest = rest[open + len + 1..].trim_start_matches(['`', '\'', '"']);
    }
    out.push_str(rest);
    let mut text = collapse(&out);
    for (from, to) in [
        ("JSON", "data"),
        ("TOML", "settings"),
        ("SQL", "database"),
        ("UUID", "identifier"),
        ("payload", "content"),
        ("DocType", "document type"),
    ] {
        text = text.replace(from, to);
    }
    let text = text.trim_end_matches([':', ',', ' ']).to_owned();
    if text.ends_with(['.', '!', '?']) {
        text
    } else {
        format!("{text}.")
    }
}

fn collapse(text: &str) -> String {
    let joined = text.split_whitespace().collect::<Vec<_>>().join(" ");
    joined.replace(" .", ".").replace(" ,", ",")
}

/// Renders entries as a help file: compact `[[help]]` tables, sorted by code.
#[must_use]
pub fn render_file(prefix: &str, entries: &[Help]) -> String {
    let mut out = format!(
        "# Help for the {prefix} error codes. One [[help]] per code and locale; see\n# crates/framework/spot-unattended-support/src/help.rs for the fields.\n"
    );
    for help in entries {
        let _ = write!(
            out,
            "\n[[help]]\ncode = {}\nwhat_happened = {}\nwhy = {}\nwhat_to_do = [{}]\naudience = {}\n",
            quote(&help.code),
            quote(&help.what_happened),
            quote(&help.why),
            help.what_to_do
                .iter()
                .map(|s| quote(s))
                .collect::<Vec<_>>()
                .join(", "),
            quote(match help.audience {
                Audience::User => "user",
                Audience::Admin => "admin",
                Audience::Operator => "operator",
            }),
        );
        if let Some(doc) = &help.learn_more {
            let _ = writeln!(out, "learn_more = {}", quote(doc));
        }
    }
    out
}

fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if c.is_control() => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Writes skeleton entries for `missing` codes into `dir`, appending to `<prefix>.toml` (the
/// code's first segment) and creating it when absent. Returns the files touched.
///
/// # Errors
/// A `help_malformed` error when a file cannot be read or written.
pub fn write_skeletons(
    catalogue: &Catalogue,
    missing: &[String],
    dir: &Path,
) -> Result<Vec<PathBuf>, SupportError> {
    let io = |e: std::io::Error| SupportError::new(codes::HELP_MALFORMED, e.to_string());
    let mut prefixes: BTreeSet<&str> = BTreeSet::new();
    for code in missing {
        prefixes.insert(code.split('.').next().unwrap_or(code));
    }
    let mut touched = Vec::new();
    for prefix in prefixes {
        let path = dir.join(format!("{prefix}.toml"));
        let mut entries: Vec<Help> = missing
            .iter()
            .filter(|c| c.split('.').next() == Some(prefix))
            .filter_map(|c| catalogue.lookup(c))
            .map(derive_help)
            .collect();
        entries.sort_by(|a, b| a.code.cmp(&b.code));
        let text = if path.is_file() {
            let existing = fs::read_to_string(&path).map_err(io)?;
            let body = render_file(prefix, &entries);
            // Keep the existing file; add only the new tables (everything after the header).
            let tables = body.split_once("\n[[help]]").map_or("", |(_, t)| t);
            format!("{}\n[[help]]{tables}", existing.trim_end())
        } else {
            render_file(prefix, &entries)
        };
        fs::write(&path, text).map_err(io)?;
        touched.push(path);
    }
    Ok(touched)
}
