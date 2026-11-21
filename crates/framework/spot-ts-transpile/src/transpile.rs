//! The transpile flow: parse, enforce the tier rules, strip types, emit.
//!
//! One call — [`transpile`] — takes TypeScript source and returns either
//! engine-ready JavaScript (with a v3 source map and the module links) or the
//! complete list of diagnostics to show the author at save time. No Node, no
//! network, no thread spawn: everything runs in-process, deterministically.

use std::path::{Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast_visit::Visit;
use oxc_codegen::{Codegen, CodegenOptions};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};

use crate::collect::Collector;
use crate::diagnostic::{Diagnostic, LineIndex};
use crate::limit::over_nesting_cap;
use crate::options::Options;

// [TYPES]

/// The outcome of one transpile: engine-ready JavaScript plus its map.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Transpiled {
    /// Plain JavaScript, ESM, engine-safe subset (see the README).
    pub js: String,
    /// Standard v3 source map as JSON; `sources`/`file` are relative.
    pub source_map: String,
    /// Every import the script makes (module + names), the engine resolves them.
    pub imports: Vec<Import>,
    /// Exported names in source order, `default` for a default export.
    pub exports: Vec<String>,
}

/// One import of one module, as the engine needs to resolve it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Import {
    /// The module specifier, e.g. `spot:meta` or an allow-listed module.
    pub module: String,
    /// Imported names: `imported` is the exported name, `local` the binding.
    pub named: Vec<ImportedName>,
    /// `import x from ...`.
    pub default: Option<String>,
    /// `import * as x from ...`.
    pub namespace: Option<String>,
}

/// An imported name and the local binding it becomes.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ImportedName {
    /// The name as exported by the module.
    pub imported: String,
    /// The local binding it is bound to in this script.
    pub local: String,
}

// [FLOW]

/// Transpile TypeScript `source` (from script `file`) to engine JavaScript.
///
/// Every failure returns **all** diagnostics, position-annotated (1-based):
/// syntax errors the parser recovered from, the constructs the script tier
/// forbids (`ts.*_not_allowed` / `_not_supported`), and the size/depth caps.
///
/// The output is deterministic: the same input and options give byte-identical
/// `js` and `source_map`. An absolute `file` is reduced to its final component
/// so the map never carries absolute paths.
pub fn transpile(
    source: &str,
    file: &str,
    options: &Options,
) -> Result<Transpiled, Vec<Diagnostic>> {
    if source.len() > options.source_size_cap {
        return Err(vec![Diagnostic::whole_input(
            "ts.source_too_large",
            format!(
                "script is {} bytes; the save-time cap is {} bytes",
                source.len(),
                options.source_size_cap
            ),
        )]);
    }
    let index = LineIndex::new(source);
    if let Some(span) = over_nesting_cap(source, options.nesting_depth_cap) {
        return Err(vec![Diagnostic::spanned(
            "ts.too_deep",
            format!(
                "nesting is deeper than {} levels; split the script into smaller functions",
                options.nesting_depth_cap
            ),
            &index,
            span.start,
            span.end,
        )]);
    }
    let map_name = map_name_for(file);

    // The parse, transform and print happen in one scope that owns the arena
    // the AST lives in; nothing escapes but plain strings.
    let allocator = Allocator::default();
    let source_type = SourceType::ts().with_module(true);
    let parsed = Parser::new(&allocator, source, source_type).parse();

    let parse_errors = parse_diagnostics(&parsed.diagnostics, &index);
    if !parse_errors.is_empty() {
        // The parser recovered; report everything it found, not just the first.
        return Err(parse_errors);
    }

    // Save-time tier rules, on the TypeScript AST BEFORE any rewriting:
    // TS-only constructs (decorators, namespaces, `import ... =`) vanish in
    // the transform, so the rules must fire while they are still in the tree.
    let mut tier_rules = Collector::new(options);
    tier_rules.visit_program(&parsed.program);
    tier_rules.collection.dedup_nested();
    if !tier_rules.collection.findings.is_empty() {
        return Err(reports_for(&tier_rules.collection.findings, &index));
    }

    // Type stripping (ADR-054 step 2: this is the save step; ADR-054 chose
    // the engine). The transformer needs scoping from the semantic pass and
    // the semantic model itself is dropped first so it can mutate the
    // program; `with_enum_eval` is what the TS enum transform requires to
    // inline member expressions like `Color.Green`, and `optimize_const_enums`
    // inlines `const enum` uses so no enum object is emitted for them.
    let mut program = parsed.program;
    let scoping = SemanticBuilder::new()
        .with_enum_eval(true)
        .build(&program)
        .semantic
        .into_scoping();
    let transform_options = TransformOptions {
        typescript: oxc_transformer::TypeScriptOptions {
            optimize_const_enums: true,
            ..oxc_transformer::TypeScriptOptions::default()
        },
        ..TransformOptions::default()
    };
    let transformed = Transformer::new(&allocator, Path::new(""), &transform_options)
        .build_with_scoping(scoping, &mut program);
    let transform_errors = parse_diagnostics(&transformed.diagnostics, &index);
    if !transform_errors.is_empty() {
        return Err(transform_errors);
    }

    // Module records come from the JavaScript the engine will actually run:
    // type-only imports were removed and enums became objects, so the links
    // the engine must resolve are exactly the ones collected here. (Any
    // findings in this second pass repeat pass one — the same value imports
    // with the spans preserved; tier rules are enforced on the source.)
    let mut records_pass = Collector::new(options);
    records_pass.visit_program(&program);
    let records = records_pass.collection;

    let printed = Codegen::new()
        .with_options(CodegenOptions {
            source_map_path: Some(PathBuf::from(&map_name)),
            ..CodegenOptions::default()
        })
        .build(&program);
    let source_map = match printed.map {
        Some(map) => map.to_json_string(),
        None => {
            return Err(vec![Diagnostic::whole_input(
                "ts.transpile",
                "source map generation produced no map; please report the script \
                 that triggered this"
                    .to_string(),
            )]);
        }
    };
    Ok(Transpiled {
        js: printed.code,
        source_map,
        imports: records.imports,
        exports: records.exports,
    })
}

/// Map oxc diagnostics (syntax or transform) to tier diagnostics with span
/// positions, sorted by position then code for a stable order.
fn parse_diagnostics(oxc: &oxc_diagnostics::Diagnostics, index: &LineIndex) -> Vec<Diagnostic> {
    let mut reports: Vec<Diagnostic> = oxc
        .errors()
        .chain(oxc.warnings())
        .map(|diagnostic| {
            let span = diagnostic
                .labels
                .first()
                .map(oxc_span::LabeledSpan::span)
                .unwrap_or_default();
            Diagnostic::spanned(
                "ts.syntax",
                diagnostic.message.to_string(),
                index,
                span.start,
                span.end,
            )
        })
        .collect();
    reports.sort_by(|one, other| {
        (
            one.line,
            one.column,
            one.end_line,
            one.end_column,
            one.code.as_str(),
        )
            .cmp(&(
                other.line,
                other.column,
                other.end_line,
                other.end_column,
                other.code.as_str(),
            ))
    });
    reports
}

/// Turn the collector's raw findings into position-annotated diagnostics.
fn reports_for(findings: &[crate::collect::RawFinding], index: &LineIndex) -> Vec<Diagnostic> {
    let mut reports: Vec<Diagnostic> = findings
        .iter()
        .map(|finding| {
            Diagnostic::spanned(
                finding.code,
                finding.message.clone(),
                index,
                finding.span.start,
                finding.span.end,
            )
        })
        .collect();
    reports.sort_by(|one, other| {
        (
            one.line,
            one.column,
            one.end_line,
            one.end_column,
            one.code.as_str(),
        )
            .cmp(&(
                other.line,
                other.column,
                other.end_line,
                other.end_column,
                other.code.as_str(),
            ))
    });
    reports
}

/// The name the source map carries: the `file` as given when it is relative,
/// its final component when absolute — never an absolute path.
fn map_name_for(file: &str) -> String {
    let path = Path::new(file);
    if path.is_absolute() || file.contains(":\\") || file.starts_with('/') {
        return path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "script.ts".to_string());
    }
    file.to_string()
}
