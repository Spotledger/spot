//! One AST walk that collects the script-tier rules and the module linkage.
//!
//! Everything save-time enforcement needs comes out of a single visitation:
//! the module links (import/export), and every construct the tier forbids —
//! each rejected with a stable code and a span, resolved to positions later.

use oxc_ast::ast::{
    BindingPattern, CallExpression, Declaration, Decorator, ExportAllDeclaration,
    ExportDeclaration, ExportDefaultDeclaration, ExportFromDeclaration, ExportNamedDeclaration,
    IdentifierReference, ImportDeclaration, ImportDeclarationSpecifier, ImportExpression,
    ImportOrExportKind, ModuleExportName, NewExpression, TSImportEqualsDeclaration,
    TSModuleReference, TSNamespaceDeclaration, WithStatement,
};
use oxc_ast_visit::{Visit, walk};
use oxc_span::Span;

use crate::options::Options;
use crate::transpile::{Import, ImportedName};

/// A rule the script tier broke, before positions are resolved.
#[derive(Debug)]
pub struct RawFinding {
    /// Stable diagnostic code, e.g. `ts.eval_not_allowed`.
    pub code: &'static str,
    /// Where in the source the construct starts.
    pub span: Span,
    /// What to do instead.
    pub message: String,
}

/// Everything one pass over the AST gathers.
#[derive(Debug, Default)]
pub struct Collection {
    /// Constructs the tier forbids, in visit order.
    pub findings: Vec<RawFinding>,
    /// The script's imports, in source order.
    pub imports: Vec<Import>,
    /// The script's exported names, in source order.
    pub exports: Vec<String>,
}

impl Collection {
    /// Collapse nested duplicates of one violation, keeping the outermost:
    /// `eval(code)` is visited as a call *and* as a reference to the callee
    /// identifier inside it — the author sees one report, at the construct.
    pub fn dedup_nested(&mut self) {
        self.findings
            .sort_by_key(|finding| (finding.span.start, finding.span.end, finding.code));
        let mut kept: Vec<RawFinding> = Vec::new();
        for finding in self.findings.drain(..) {
            // The neighbour with the same code is either nested in it, the
            // same construct, or nests it; keep exactly the outermost report.
            let mut nested = false;
            while let Some(last) = kept.last() {
                if last.code != finding.code {
                    break;
                }
                if finding.span.start >= last.span.start && finding.span.end <= last.span.end {
                    // The new finding sits inside the reported one.
                    nested = true;
                    break;
                }
                if finding.span.start <= last.span.start && finding.span.end >= last.span.end {
                    // The new finding wraps the reported one; prefer it.
                    kept.pop();
                    continue;
                }
                break;
            }
            if !nested {
                kept.push(finding);
            }
        }
        self.findings = kept;
    }
}

/// Walks the parsed TypeScript and collects links (`import`/`export`) and
/// every construct the script tier does not allow.
pub struct Collector<'a> {
    /// The allow-list the import rules are checked against.
    allow_imports: &'a [String],
    /// What the walk gathered.
    pub collection: Collection,
}

impl<'a> Collector<'a> {
    /// A collector enforcing `options.allow_imports`.
    pub fn new(options: &'a Options) -> Self {
        Self {
            allow_imports: &options.allow_imports,
            collection: Collection::default(),
        }
    }

    fn reject(&mut self, code: &'static str, span: Span, message: &str) {
        self.collection.findings.push(RawFinding {
            code,
            span,
            message: message.to_string(),
        });
    }

    /// Does the allow-list admit this module specifier?
    ///
    /// (One truth for the rule: [`Options::allows_import`] and this agree, and
    /// both are exercised by tests.)
    fn allows(&self, module: &str) -> bool {
        self.allow_imports
            .iter()
            .any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => module.starts_with(prefix),
                None => module == pattern.as_str(),
            })
    }

    /// A `Value`-kind linkage of `module` from `span`: allowed by the list or
    /// rejected as `ts.import_not_allowed`.
    fn check_module(&mut self, module: &str, span: Span) {
        if !self.allows(module) {
            self.reject(
                "ts.import_not_allowed",
                span,
                &format!(
                    "module {module:?} is not on the import allow-list; the engine only \
                     resolves modules supplied in it (default: spot:* virtual modules)"
                ),
            );
        }
    }
}

impl<'a> Visit<'a> for Collector<'a> {
    fn visit_import_declaration(&mut self, it: &ImportDeclaration<'a>) {
        // `import type` never links at runtime and is stripped; only value
        // imports need to name an allow-listed module, and they are the only
        // ones that reach the engine's import list.
        let links = it.import_kind == ImportOrExportKind::Value;
        if links {
            self.check_module(it.source.value.as_str(), it.span);
        }
        let mut import = Import {
            module: it.source.value.to_string(),
            named: Vec::new(),
            default: None,
            namespace: None,
        };
        for specifier in it.specifiers.iter().flatten() {
            match specifier {
                ImportDeclarationSpecifier::ImportSpecifier(spec) => {
                    import.named.push(ImportedName {
                        imported: exported_name(&spec.imported),
                        local: spec.local.name.to_string(),
                    });
                }
                ImportDeclarationSpecifier::ImportDefaultSpecifier(spec) => {
                    import.default = Some(spec.local.name.to_string());
                }
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(spec) => {
                    import.namespace = Some(spec.local.name.to_string());
                }
            }
        }
        if links {
            self.collection.imports.push(import);
        }
        walk::walk_import_declaration(self, it);
    }

    fn visit_export_declaration(&mut self, it: &ExportDeclaration<'a>) {
        // `export const five = 5`, `export function pick() {}`, `export enum
        // Color {}` — the declared bindings are what the module exports.
        declared_names(&it.declaration, &mut self.collection.exports);
        walk::walk_export_declaration(self, it);
    }

    fn visit_export_named_declaration(&mut self, it: &ExportNamedDeclaration<'a>) {
        // `export type { .. }` is stripped; only value exports link names.
        if it.export_kind == ImportOrExportKind::Value {
            for spec in &it.specifiers {
                if spec.export_kind == ImportOrExportKind::Value {
                    self.collection.exports.push(exported_name(&spec.exported));
                }
            }
        }
        walk::walk_export_named_declaration(self, it);
    }

    fn visit_export_from_declaration(&mut self, it: &ExportFromDeclaration<'a>) {
        // A re-export links the module exactly like an import does. Type-kind
        // re-exports (`export type { .. } from`) are stripped instead.
        if it.export_kind == ImportOrExportKind::Value {
            self.check_module(it.source.value.as_str(), it.span);
        }
        walk::walk_export_from_declaration(self, it);
    }

    fn visit_export_all_declaration(&mut self, it: &ExportAllDeclaration<'a>) {
        self.check_module(it.source.value.as_str(), it.span);
        walk::walk_export_all_declaration(self, it);
    }

    fn visit_export_default_declaration(&mut self, it: &ExportDefaultDeclaration<'a>) {
        self.collection.exports.push("default".to_string());
        walk::walk_export_default_declaration(self, it);
    }

    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        self.reject(
            "ts.dynamic_import_not_allowed",
            it.span,
            "dynamic import() is not allowed in scripts; use a static import of an \
             allow-listed module",
        );
        walk::walk_import_expression(self, it);
    }

    fn visit_call_expression(&mut self, it: &CallExpression<'a>) {
        // `eval(...)`, `require(...)` and `Function(...)` as calls.
        if callee_name(&it.callee) == Some("eval") {
            self.reject(
                "ts.eval_not_allowed",
                it.span,
                "eval is not allowed in scripts; the engine never provides it",
            );
        } else if callee_name(&it.callee) == Some("require") {
            self.reject(
                "ts.require_not_allowed",
                it.span,
                "require is not available in scripts; use a static import",
            );
        } else if callee_name(&it.callee) == Some("Function") {
            self.reject(
                "ts.function_constructor_not_allowed",
                it.span,
                "calling Function directly is not allowed in scripts; write the \
                 statements you would have passed in",
            );
        }
        walk::walk_call_expression(self, it);
    }

    fn visit_new_expression(&mut self, it: &NewExpression<'a>) {
        // `new Function(...)`.
        if callee_name(&it.callee) == Some("Function") {
            self.reject(
                "ts.function_constructor_not_allowed",
                it.span,
                "new Function(...) is not allowed in scripts; write the statements \
                 you would have passed in",
            );
        }
        walk::walk_new_expression(self, it);
    }

    fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
        // A bare reference to `eval` (even without the call) must not survive.
        if it.name == "eval" {
            self.reject(
                "ts.eval_not_allowed",
                it.span,
                "eval is not allowed in scripts; the engine never provides it",
            );
        } else if it.name == "require" {
            self.reject(
                "ts.require_not_allowed",
                it.span,
                "require is not available in scripts; use a static import",
            );
        }
        walk::walk_identifier_reference(self, it);
    }

    fn visit_with_statement(&mut self, it: &WithStatement<'a>) {
        self.reject(
            "ts.with_not_allowed",
            it.span,
            "with is not allowed in scripts (it is also invalid in strict mode)",
        );
        walk::walk_with_statement(self, it);
    }

    fn visit_decorator(&mut self, it: &Decorator<'a>) {
        self.reject(
            "ts.decorator_not_supported",
            it.span,
            "decorators are not supported in scripts; write the wiring as plain statements",
        );
        walk::walk_decorator(self, it);
    }

    fn visit_ts_namespace_declaration(&mut self, it: &TSNamespaceDeclaration<'a>) {
        self.reject(
            "ts.namespace_not_supported",
            it.span,
            "namespaces and module Foo {} blocks are not supported in scripts; group \
             under an object, or use modules supplied by the allow-list",
        );
        walk::walk_ts_namespace_declaration(self, it);
    }

    fn visit_ts_import_equals_declaration(&mut self, it: &TSImportEqualsDeclaration<'a>) {
        match &it.module_reference {
            TSModuleReference::ExternalModuleReference(reference) => self.reject(
                "ts.require_not_allowed",
                it.span,
                &format!(
                    "import ... = require({:?}) is not allowed; use `import` of an \
                     allow-listed module",
                    reference.expression.value,
                ),
            ),
            _ => self.reject(
                "ts.namespace_not_supported",
                it.span,
                "import ... = Namespace is not supported in scripts",
            ),
        }
        walk::walk_ts_import_equals_declaration(self, it);
    }
}

/// The callee's plain name when it is an unqualified identifier, else `None`.
fn callee_name<'a>(callee: &oxc_ast::ast::Expression<'a>) -> Option<&'a str> {
    match callee {
        oxc_ast::ast::Expression::Identifier(identifier) => Some(identifier.name.as_str()),
        _ => None,
    }
}

/// The exported name carried by a [`ModuleExportName`] (identifier or string).
fn exported_name(name: &ModuleExportName<'_>) -> String {
    match name {
        ModuleExportName::IdentifierName(identifier) => identifier.name.to_string(),
        ModuleExportName::IdentifierReference(identifier) => identifier.name.to_string(),
        ModuleExportName::StringLiteral(literal) => literal.value.to_string(),
    }
}

/// The bindings a declaration exports (`export const { a } = o`, `export
/// function pick`, `export enum Color`). Type-only declarations are stripped
/// by the transformer and contribute nothing.
fn declared_names(declaration: &Declaration<'_>, out: &mut Vec<String>) {
    match declaration {
        Declaration::VariableDeclaration(variables) => {
            for declarator in &variables.declarations {
                pattern_names(&declarator.id, out);
            }
        }
        Declaration::FunctionDeclaration(function) => {
            if let Some(id) = &function.id {
                out.push(id.name.to_string());
            }
        }
        Declaration::ClassDeclaration(class) => {
            if let Some(id) = &class.id {
                out.push(id.name.to_string());
            }
        }
        _ => {}
    }
}

/// The names one binding pattern binds (`const { a: b } = o` binds `b`;
/// holes, defaults and resting recurse).
fn pattern_names(pattern: &BindingPattern<'_>, out: &mut Vec<String>) {
    match pattern {
        BindingPattern::BindingIdentifier(identifier) => {
            out.push(identifier.name.to_string());
        }
        BindingPattern::ObjectPattern(object) => {
            for property in &object.properties {
                pattern_names(&property.value, out);
            }
            if let Some(rest) = &object.rest {
                pattern_names(&rest.argument, out);
            }
        }
        BindingPattern::ArrayPattern(array) => {
            for element in array.elements.iter().flatten() {
                pattern_names(element, out);
            }
            if let Some(rest) = &array.rest {
                pattern_names(&rest.argument, out);
            }
        }
        BindingPattern::AssignmentPattern(assignment) => {
            pattern_names(&assignment.left, out);
        }
    }
}
