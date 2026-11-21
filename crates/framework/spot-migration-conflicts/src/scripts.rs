//! Scripts against the new TypeScript declarations.
//!
//! The new effective metadata is turned into one `.d.ts` by `spot-dts`; the declaration's members
//! per interface are read back out of that generated text, so the check is against the exact
//! artefact an editor's `tsc` would see. Each script is first put through the save-time checker
//! (`spot-ts-transpile::transpile`: parse, tier rules, type stripping — never execution); then
//! every `doc.<field>` access it makes is checked against the members the declarations give its
//! DocType's interface. A reference that is not declared **and** that the plan removes or renames
//! is a conflict: the release broke a script that used to compile.
//!
//! The scan is deliberately small and lexical: it reads `doc.field`, `doc?.field` and
//! `doc["field"]` outside comments and string literals. It does not resolve types, so a local
//! variable that happens to be named `doc` is read as the document — the convention every server
//! script follows.

use std::collections::{BTreeMap, BTreeSet};

use spot_ts_transpile::{Options, transpile};

use crate::collect::Collector;
use crate::input::{ReleaseContext, SiteScript};
use crate::model::{ArtefactKind, ArtefactRef, Cause, ConflictCategory};
use crate::release::ReleaseDelta;

/// Runs the script checks.
pub(crate) fn check(ctx: &ReleaseContext<'_>, delta: &ReleaseDelta, out: &mut Collector) {
    let declarations = match spot_dts::generate(ctx.new_meta.definitions()) {
        Ok(generated) => Declarations::parse(&generated.text),
        Err(errors) => {
            let message = errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            out.push(
                ConflictCategory::DeclarationsError,
                Cause::new(
                    "release/metadata/declarations",
                    "the new metadata does not generate TypeScript declarations",
                ),
                ArtefactRef::new(ArtefactKind::Declarations, "spot_dts"),
                format!(
                    "Fix the new metadata so declarations generate ({message}); scripts cannot be checked until then."
                ),
            );
            return;
        }
    };

    for script in ctx.scripts {
        let artefact = script_artefact(script);
        if let Some(doctype) = &script.doctype
            && let Some((cause, resolution)) = delta.doctype_cause(doctype)
        {
            out.push(
                ConflictCategory::ScriptDocTypeRemoved,
                cause,
                artefact,
                resolution,
            );
            continue;
        }

        if let Err(diagnostics) = transpile(&script.source, &script.id(), &Options::default()) {
            let detail = diagnostics
                .iter()
                .map(|d| format!("{} ({}:{})", d.code, d.line, d.column))
                .collect::<Vec<_>>()
                .join(", ");
            out.push(
                ConflictCategory::ScriptSyntaxError,
                Cause::new(
                    "release/script_tier",
                    "the script does not pass the save-time TypeScript checker",
                ),
                artefact,
                format!("Fix the script so it transpiles ({detail}), then save it again."),
            );
            continue;
        }

        let Some(doctype) = &script.doctype else {
            continue;
        };
        let Some(allowed) = declarations.fields_of(doctype) else {
            continue;
        };
        for field in doc_member_refs(&script.source) {
            if allowed.contains(&field) {
                continue;
            }
            if let Some((cause, resolution)) = delta.field_cause(doctype, &field) {
                out.push(
                    ConflictCategory::ScriptTypeError,
                    cause,
                    artefact.clone(),
                    resolution,
                );
            }
        }
    }
}

/// The artefact id of a script: `script:<app>:<name>`.
fn script_artefact(script: &SiteScript) -> ArtefactRef {
    ArtefactRef::new(ArtefactKind::Script, script.id())
}

/// The declared members per interface, read from the generated `.d.ts` text.
#[derive(Clone, Debug, Default)]
struct Declarations {
    members: BTreeMap<String, BTreeSet<String>>,
    parents: BTreeSet<String>,
}

impl Declarations {
    /// Reads the `interface` blocks out of a generated declarations file.
    ///
    /// The emitter writes one `interface <Name> {` line per DocType (`export interface` for a
    /// DocType, bare `interface DocumentBase`), each property on its own line, so the parse is
    /// line-based. Anything outside an interface block (the `spot` namespace, comments) is
    /// ignored.
    fn parse(text: &str) -> Self {
        let mut declarations = Self::default();
        let mut current: Option<String> = None;
        for line in text.lines() {
            let trimmed = line.trim();
            if let Some((name, extends_base)) = interface_header(trimmed) {
                declarations.members.entry(name.clone()).or_default();
                if extends_base {
                    declarations.parents.insert(name.clone());
                }
                current = Some(name);
                continue;
            }
            let Some(name) = current.clone() else {
                continue;
            };
            if trimmed == "}" {
                current = None;
                continue;
            }
            if let Some(member) = property_name(trimmed)
                && let Some(set) = declarations.members.get_mut(&name)
            {
                set.insert(member);
            }
        }
        declarations
    }

    /// The members a script may read on a document of `doctype`: the DocType's own interface plus
    /// `DocumentBase` when the DocType is a parent (its interface extends it). `None` when the
    /// DocType has no interface.
    fn fields_of(&self, doctype: &str) -> Option<BTreeSet<String>> {
        let ident = spot_dts::naming::interface_name(doctype).ok()?;
        let own = self.members.get(&ident)?;
        let mut allowed = own.clone();
        if self.parents.contains(&ident)
            && let Some(base) = self.members.get(spot_dts::DOCUMENT_BASE)
        {
            allowed.extend(base.iter().cloned());
        }
        Some(allowed)
    }
}

/// The interface name of a header line and whether it extends `DocumentBase`, when the line
/// opens an interface.
fn interface_header(line: &str) -> Option<(String, bool)> {
    let rest = line
        .strip_prefix("export interface ")
        .or_else(|| line.strip_prefix("interface "))?;
    let end = rest.find([' ', '{', '<']).unwrap_or(rest.len());
    let name = &rest[..end];
    if name.is_empty() {
        return None;
    }
    Some((name.to_owned(), line.contains(" extends ")))
}

/// The property name of a declaration line inside an interface, or `None` for a JSDoc line, a
/// blank line or anything that is not a property.
fn property_name(line: &str) -> Option<String> {
    if line.is_empty() || line.starts_with('/') || line.starts_with('*') {
        return None;
    }
    let rest = line.strip_prefix("readonly ").unwrap_or(line).trim_start();
    if let Some(after) = rest.strip_prefix('"') {
        let end = after.find('"')?;
        return (!after[..end].is_empty()).then(|| after[..end].to_owned());
    }
    let end = rest.find(['?', ':'])?;
    let name = rest[..end].trim_end();
    (!name.is_empty()).then(|| name.to_owned())
}

/// Every field a script reads off `doc`: `doc.field`, `doc?.field` and `doc["field"]`, outside
/// comments and string literals.
fn doc_member_refs(source: &str) -> BTreeSet<String> {
    let bytes = source.as_bytes();
    let mut out = BTreeSet::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
            }
            quote @ (b'"' | b'\'' | b'`') => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if bytes[i] == quote {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            c if c.is_ascii_alphabetic() || c == b'_' || c == b'$' => {
                let start = i;
                while i < bytes.len()
                    && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'$')
                {
                    i += 1;
                }
                if &source[start..i] == "doc"
                    && !is_member(source, start)
                    && let Some(field) = member_after(source, i)
                {
                    out.insert(field);
                }
            }
            _ => i += 1,
        }
    }
    out
}

/// Whether the identifier at `start` follows a `.` (so it is a member access, not a root).
fn is_member(source: &str, start: usize) -> bool {
    source[..start]
        .trim_end()
        .chars()
        .next_back()
        .is_some_and(|c| c == '.')
}

/// The member name after an identifier ends at `at`: `.field`, `?.field` or `["field"]`.
fn member_after(source: &str, at: usize) -> Option<String> {
    let bytes = source.as_bytes();
    let mut j = at;
    let skip = |j: &mut usize| {
        while *j < bytes.len() && bytes[*j].is_ascii_whitespace() {
            *j += 1;
        }
    };
    skip(&mut j);
    if bytes.get(j) == Some(&b'?') {
        j += 1;
        skip(&mut j);
    }
    match bytes.get(j) {
        Some(&b'.') => {
            j += 1;
            skip(&mut j);
            let start = j;
            while j < bytes.len()
                && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'$')
            {
                j += 1;
            }
            (j > start).then(|| source[start..j].to_owned())
        }
        Some(&b'[') => {
            j += 1;
            skip(&mut j);
            let quote = *bytes.get(j)?;
            if quote != b'"' && quote != b'\'' {
                return None;
            }
            j += 1;
            let start = j;
            while j < bytes.len() && bytes[j] != quote {
                j += 1;
            }
            (j > start).then(|| source[start..j].to_owned())
        }
        _ => None,
    }
}
