//! The sandboxed MiniJinja environment and the public engine type.
//!
//! Defaults are hard sandbox limits: a fuel budget of 1,000,000 engine
//! instructions per render, a recursion depth limit of 100 (MiniJinja's hard
//! ceiling without the `stacker` feature is 500) and a rendered output size
//! of at most 10 MiB. Strict undefined values turn any missing variable,
//! method or attribute into a [`TemplateError::Undefined`] that names the
//! template, the line and the offending source text.

use std::collections::HashMap;
use std::sync::{
    Mutex, MutexGuard,
    atomic::{AtomicU64, Ordering},
};

use minijinja::{AutoEscape, Environment, Error as JinError, ErrorKind, UndefinedBehavior};

use crate::context::Context;
use crate::error::TemplateError;
use crate::filters;
use crate::format::{self, typst::AUTO_ESCAPE_NAME};

/// The default fuel budget in engine instructions.
const DEFAULT_FUEL: u64 = 1_000_000;

/// The default recursion depth limit.
const DEFAULT_RECURSION_LIMIT: usize = 100;

/// The default rendered-output size limit, 10 MiB.
const DEFAULT_MAX_OUTPUT_BYTES: usize = 10_485_760;

/// The prefix engine filters put on typed error texts.
const SPOT_TEMPLATE_PREFIX: &str = "spot-template: ";

/// How a template's output is escaped, chosen by the template name's
/// extension (case-sensitive, lowercase).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// HTML escaping for `.html`/`.htm` templates.
    Html,
    /// Typst string-literal output for `.typ` templates: every value prints
    /// as a Typst string literal and stays inert against markup or code
    /// injection (SPK-03).
    Typst,
    /// Text for everything else: nothing escapes.
    Text,
}

impl OutputKind {
    /// Chooses the output kind from a template name's extension:
    /// `invoice.html` → Html, `invoice.typ` → Typst, anything else
    /// (including names without a dot) → Text.
    #[must_use]
    pub fn from_name(name: &str) -> Self {
        let extension = name.rsplit('.').next().unwrap_or("");
        match extension {
            "html" | "htm" => Self::Html,
            "typ" => Self::Typst,
            _ => Self::Text,
        }
    }

    /// The MiniJinja auto-escape marker for this kind.
    pub(crate) const fn to_auto_escape(self) -> AutoEscape {
        match self {
            Self::Html => AutoEscape::Html,
            Self::Typst => AutoEscape::Custom(AUTO_ESCAPE_NAME),
            Self::Text => AutoEscape::None,
        }
    }
}

/// Where the engine reads template sources from.
pub trait TemplateSource {
    /// Loads a template: `Some((source, version))`, or `None` when the name
    /// is unknown. The version must change whenever the source changes so
    /// the engine can recompile.
    fn load(&self, name: &str) -> Option<(String, Version)>;
}

/// A template version marker; a changed version recompiles the template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version(u64);

impl Version {
    /// A version marker from a raw counter.
    #[must_use]
    pub const fn new(repr: u64) -> Self {
        Self(repr)
    }

    /// The raw counter.
    #[must_use]
    pub const fn repr(self) -> u64 {
        self.0
    }
}

impl From<u64> for Version {
    fn from(repr: u64) -> Self {
        Self(repr)
    }
}

/// A [`TemplateSource`] backed by an in-memory map; useful for tests and
/// small services. Re-inserting a name bumps its version, which triggers a
/// recompile on the engine.
#[derive(Debug, Default, Clone)]
pub struct MemorySource {
    templates: HashMap<String, (String, u64)>,
    next_version: u64,
}

impl MemorySource {
    /// An empty source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Inserts or replaces a template.
    pub fn insert(&mut self, name: &str, source: &str) {
        self.next_version = self.next_version.saturating_add(1);
        let version = self.next_version;
        match self.templates.get_mut(name) {
            Some(slot) => {
                slot.0 = source.to_owned();
                slot.1 = version;
            }
            None => {
                self.templates
                    .insert(name.to_owned(), (source.to_owned(), version));
            }
        }
    }
}

impl TemplateSource for MemorySource {
    fn load(&self, name: &str) -> Option<(String, Version)> {
        self.templates
            .get(name)
            .map(|(source, version)| (source.clone(), Version::new(*version)))
    }
}

/// Counters since engine creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EngineStatistics {
    /// How often a template source was loaded (once per render).
    pub source_loads: u64,
    /// How often a template was (re)compiled; unchanged versions reuse the
    /// compiled form.
    pub template_compiles: u64,
}

struct Compiled {
    source: String,
    version: Version,
}

struct EngineState {
    env: Environment<'static>,
    compiled: HashMap<String, Compiled>,
}

/// The platform's template engine: a hardened, shareable MiniJinja
/// environment with typed errors, exact money objects and output-specific
/// escaping.
pub struct TemplateEngine<S: TemplateSource> {
    state: Mutex<EngineState>,
    loader: S,
    source_loads: AtomicU64,
    template_compiles: AtomicU64,
    max_output_bytes: usize,
}

impl<S: TemplateSource> TemplateEngine<S> {
    /// Creates an engine over `loader` with the default sandbox limits.
    #[must_use]
    pub fn new(loader: S) -> Self {
        Self {
            state: Mutex::new(EngineState {
                env: fresh_env(),
                compiled: HashMap::new(),
            }),
            loader,
            source_loads: AtomicU64::new(0),
            template_compiles: AtomicU64::new(0),
            max_output_bytes: DEFAULT_MAX_OUTPUT_BYTES,
        }
    }

    /// Overrides the fuel budget (engine instructions per render).
    #[must_use]
    pub fn with_fuel(self, fuel: u64) -> Self {
        self.lock_engine().env.set_fuel(Some(fuel));
        self
    }

    /// Overrides the recursion depth limit; `0` is coerced to `1` (MiniJinja
    /// requires at least one frame).
    #[must_use]
    pub fn with_recursion_limit(self, limit: usize) -> Self {
        self.lock_engine().env.set_recursion_limit(limit.max(1));
        self
    }

    /// Overrides the rendered-output size limit in bytes.
    #[must_use]
    pub fn with_max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes;
        self
    }

    /// Renders `name` with `context`.
    ///
    /// The source is loaded on every call; unchanged versions reuse the
    /// compiled template, changed versions recompile in place.
    ///
    /// # Errors
    /// Typed [`TemplateError`] for unknown templates, parse failures, strict
    /// undefined values (with template, line and source text), sandbox limit
    /// violations (fuel, recursion, output size), rejected computations and
    /// filter failures.
    pub fn render(&self, name: &str, context: &Context) -> Result<String, TemplateError> {
        let mut state = self.lock_engine();
        self.source_loads.fetch_add(1, Ordering::Relaxed);
        let (source, version) = match self.loader.load(name) {
            Some(loaded) => loaded,
            None => {
                return Err(TemplateError::NotFound {
                    name: name.to_owned(),
                });
            }
        };
        let needs_compile = match state.compiled.get(name) {
            Some(compiled) => compiled.version != version,
            None => true,
        };
        if needs_compile {
            self.template_compiles.fetch_add(1, Ordering::Relaxed);
            state.env.remove_template(name);
            if let Err(error) = state
                .env
                .add_template_owned(name.to_owned(), source.clone())
            {
                return Err(map_engine_error(&error, name, &state.compiled));
            }
            state
                .compiled
                .insert(name.to_owned(), Compiled { source, version });
        }
        let template = match state.env.get_template(name) {
            Ok(template) => template,
            Err(error) => return Err(map_engine_error(&error, name, &state.compiled)),
        };
        // Stream into a size-limited buffer so a runaway template aborts at
        // the limit instead of building a huge string first.
        let mut sink = SizeLimitedSink {
            buf: Vec::new(),
            max: self.max_output_bytes,
            exceeded: None,
        };
        match template.render_captured_to(context.as_value(), &mut sink) {
            Ok(_captured) => match sink.exceeded {
                Some(actual) => Err(TemplateError::OutputLimitExceeded {
                    limit: self.max_output_bytes,
                    actual,
                }),
                None => match String::from_utf8(sink.buf) {
                    Ok(output) => Ok(output),
                    Err(_) => Err(TemplateError::Render {
                        name: name.to_owned(),
                        line: 0,
                        message: "template produced invalid UTF-8".to_owned(),
                    }),
                },
            },
            Err(error) => {
                if let Some(actual) = sink.exceeded {
                    return Err(TemplateError::OutputLimitExceeded {
                        limit: self.max_output_bytes,
                        actual,
                    });
                }
                Err(map_engine_error(&error, name, &state.compiled))
            }
        }
    }

    /// Counters since engine creation.
    #[must_use]
    pub fn statistics(&self) -> EngineStatistics {
        EngineStatistics {
            source_loads: self.source_loads.load(Ordering::Relaxed),
            template_compiles: self.template_compiles.load(Ordering::Relaxed),
        }
    }

    /// The underlying source.
    #[must_use]
    pub fn loader(&self) -> &S {
        &self.loader
    }

    /// Mutable access to the underlying source for callers that own the
    /// engine outright; updating the source between renders (with a bumped
    /// version) triggers recompiles.
    pub fn loader_mut(&mut self) -> &mut S {
        &mut self.loader
    }

    /// Locks the engine state. A panicked renderer poisons the mutex, but
    /// the state itself stays consistent (like the repo clock crate, the
    /// lock is a scheduling convenience, not an invariant carrier), so
    /// poison is unwrapped instead of failing every future render.
    fn lock_engine(&self) -> MutexGuard<'_, EngineState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

impl TemplateEngine<MemorySource> {
    /// Builds an engine over a small set of `(name, source)` templates.
    /// Useful for tests and small services.
    #[must_use]
    pub fn memory(templates: &[(&str, &str)]) -> Self {
        let mut loader = MemorySource::new();
        for (name, source) in templates {
            loader.insert(name, source);
        }
        Self::new(loader)
    }
}

/// A writer that accepts at most `max` bytes and records the first write
/// that crossed the limit; crossing it fails the write, which stops the
/// render without buffering runaway output.
struct SizeLimitedSink {
    buf: Vec<u8>,
    max: usize,
    exceeded: Option<usize>,
}

impl std::io::Write for SizeLimitedSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() + bytes.len() > self.max {
            self.exceeded = Some(self.buf.len() + bytes.len());
            return Err(std::io::Error::other("template output limit exceeded"));
        }
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn fresh_env() -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    env.set_fuel(Some(DEFAULT_FUEL));
    env.set_recursion_limit(DEFAULT_RECURSION_LIMIT);
    env.set_auto_escape_callback(|name| OutputKind::from_name(name).to_auto_escape());
    env.set_formatter(|out, state, value| {
        if state.auto_escape() == AutoEscape::Custom(AUTO_ESCAPE_NAME) {
            format::typst::format_value(out, state, value)
        } else {
            minijinja::escape_formatter(out, state, value)
        }
    });
    filters::register(&mut env);
    env
}

fn map_engine_error(
    error: &JinError,
    fallback_name: &str,
    compiled: &HashMap<String, Compiled>,
) -> TemplateError {
    let name = error.name().unwrap_or(fallback_name).to_owned();
    // MiniJinja reports 1-based lines (its lexer starts at line 1).
    let line = error.line().unwrap_or(0);
    let detail = error.detail().map(str::to_owned);
    let detail_text = detail
        .clone()
        .unwrap_or_else(|| format!("{:?}", error.kind()));
    match error.kind() {
        ErrorKind::TemplateNotFound => TemplateError::NotFound { name },
        // An include failure wraps the real error in `source()`: unwrap it
        // so a missing variable inside an included template is still an
        // `Undefined` naming that template, line and variable.
        ErrorKind::BadInclude => {
            match <dyn std::error::Error>::source(error).and_then(|s| s.downcast_ref::<JinError>())
            {
                Some(inner) => {
                    map_engine_error(inner, inner.name().unwrap_or(fallback_name), compiled)
                }
                None => TemplateError::Render {
                    name,
                    line,
                    message: detail_text,
                },
            }
        }
        ErrorKind::SyntaxError => TemplateError::Syntax {
            name: name.clone(),
            line,
            message: detail_text,
        },
        ErrorKind::UndefinedError => TemplateError::Undefined {
            name: name.clone(),
            line,
            variable: undefined_variable(error, compiled, &name, line),
            line_text: source_line(compiled, &name, line),
        },
        ErrorKind::OutOfFuel => TemplateError::FuelExhausted { name, line },
        ErrorKind::InvalidOperation if detail_text.contains("types undefined") => {
            TemplateError::Undefined {
                name: name.clone(),
                line,
                variable: undefined_variable(error, compiled, &name, line),
                line_text: source_line(compiled, &name, line),
            }
        }
        ErrorKind::InvalidOperation if detail_text.starts_with(SPOT_TEMPLATE_PREFIX) => {
            TemplateError::Filter {
                message: detail_text[SPOT_TEMPLATE_PREFIX.len()..].to_owned(),
            }
        }
        ErrorKind::InvalidOperation if detail.as_deref() == Some("recursion limit exceeded") => {
            TemplateError::RecursionLimitExceeded { name, line }
        }
        ErrorKind::InvalidOperation => TemplateError::NotComputable {
            message: format!(
                "templates do not compute money: {detail_text} — format \
                 amounts with the 'money', 'number' or 'amount_in_words' \
                 filters (ADR-12)"
            ),
        },
        _ => TemplateError::Render {
            name,
            line,
            message: detail_text,
        },
    }
}

/// The name of the variable the failing expression starts with.
///
/// The engine keeps the template source, so the expression text comes from
/// the error's byte range; the root identifier (the part before the first
/// `.`, `[` or space) names the variable. Without a range the line's first
/// `{{ … }}` expression is used; failing that, `"unknown"`.
fn undefined_variable(
    error: &JinError,
    compiled: &HashMap<String, Compiled>,
    name: &str,
    line: usize,
) -> String {
    let expr = match (error.range(), compiled.get(name)) {
        (Some(range), Some(entry)) => entry.source.get(range).map(str::to_owned),
        _ => None,
    }
    .or_else(|| {
        let text = source_line(compiled, name, line);
        let open = text.find("{{")?;
        let rest = &text[open + 2..];
        let close = rest.find("}}")?;
        Some(rest[..close].to_owned())
    });
    match expr {
        Some(expr) => {
            let trimmed = expr.trim();
            let root: String = trimmed
                .chars()
                .take_while(|c| c.is_alphanumeric() || matches!(c, '_' | '.'))
                .collect();
            if root.is_empty() {
                "<expression>".to_owned()
            } else {
                root
            }
        }
        None => "unknown".to_owned(),
    }
}

fn source_line(compiled: &HashMap<String, Compiled>, name: &str, line: usize) -> String {
    if line == 0 {
        return String::new();
    }
    match compiled.get(name) {
        Some(compiled) => compiled
            .source
            .lines()
            .nth(line - 1)
            .map(str::trim)
            .unwrap_or("")
            .to_owned(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::context::CtxValue;
    use rust_decimal::Decimal;
    use time::{Date, Month};

    const INVOICE_TXT: &str = "Invoice {{ numbers }} for {{ customer }}\n\
         Amount: {{ total | money('PKR') }} due {{ due | date(format='medium') }}\n\
         {% if total > threshold %}High value{% else %}Standard{% endif %}";

    fn due_date() -> Date {
        match Date::from_calendar_date(2025, Month::January, 15) {
            Ok(date) => date,
            Err(error) => panic!("bad date: {error}"),
        }
    }

    fn invoice_context() -> Context {
        Context::new()
            .insert("numbers", "SPK-001")
            .insert("customer", "Muhammad & Sons")
            .insert("total", Decimal::new(123_456, 2))
            .insert("threshold", Decimal::new(100_000, 2))
            .insert("due", CtxValue::Date(due_date()))
            .insert(
                "issued",
                CtxValue::DateTime(
                    due_date()
                        .with_hms(9, 30, 0)
                        .unwrap_or_else(|e| panic!("{e}"))
                        .assume_utc(),
                ),
            )
            .insert("note", "<b>Bold</b> \"quoted\"")
            .insert("item_count", 3_u32)
    }

    fn rendered_or_panic(
        engine: &TemplateEngine<MemorySource>,
        name: &str,
        context: &Context,
    ) -> String {
        match engine.render(name, context) {
            Ok(text) => text,
            Err(error) => panic!("render failed: {error}"),
        }
    }

    #[test]
    fn plain_text_golden_invoice() {
        let engine = TemplateEngine::memory(&[("invoice.txt", INVOICE_TXT)]);
        assert_eq!(
            rendered_or_panic(&engine, "invoice.txt", &invoice_context()),
            "Invoice SPK-001 for Muhammad & Sons\nAmount: Rs 1,234.56 due Jan 15, 2025\nHigh value"
        );
    }

    #[test]
    fn html_output_escapes_data() {
        let engine = TemplateEngine::memory(&[("note.html", "{{ note }} {{ note | safe }}")]);
        assert_eq!(
            rendered_or_panic(&engine, "note.html", &invoice_context()),
            "&lt;b&gt;Bold&lt;&#x2f;b&gt; &quot;quoted&quot; <b>Bold</b> \"quoted\""
        );
    }

    #[test]
    fn typst_output_is_data_inert() {
        let engine = TemplateEngine::memory(&[(
            "note.typ",
            "Total: {{ total | money('PKR') }} note {{ note }}",
        )]);
        let note = "<b>Bold</b> \"quoted\"";
        let expected = format!(
            "Total: {} note {}",
            crate::format::typst::escape_markup("Rs 1,234.56"),
            crate::format::typst::escape_markup(note),
        );
        assert_eq!(
            rendered_or_panic(&engine, "note.typ", &invoice_context()),
            expected
        );
        let injection = "x\n#let evil = 1\n*evil*";
        let context = Context::new()
            .insert("note", injection)
            .insert("total", Decimal::new(123_456, 2));
        let rendered = rendered_or_panic(&engine, "note.typ", &context);
        // the data survives, but every markup-trigger character is escaped
        let expected = crate::format::typst::escape_markup(injection);
        assert!(rendered.contains(&expected), "{rendered:?}");
        // no raw `#` may remain: every hash is preceded by a backslash
        let hashes = rendered.bytes().filter(|b| *b == b'#').count();
        let escaped_hashes = rendered.matches("\\#").count();
        assert_eq!(hashes, escaped_hashes, "raw code marker in {rendered:?}");
    }

    #[test]
    fn strict_undefined_reports_name_line_and_text() {
        let engine = TemplateEngine::memory(&[(
            "t.txt",
            "Dear {{ customer }}\nTotal {{ invoice.missing_field }}",
        )]);
        let context = Context::new()
            .insert("customer", "Acme")
            .insert("invoice", CtxValue::Map(vec![]));
        match engine.render("t.txt", &context) {
            Err(TemplateError::Undefined {
                ref name,
                line,
                ref variable,
                ref line_text,
            }) => {
                assert_eq!(name.as_str(), "t.txt");
                assert_eq!(line, 2);
                assert_eq!(variable.as_str(), "invoice.missing_field");
                assert_eq!(line_text.as_str(), "Total {{ invoice.missing_field }}");
            }
            Err(other) => panic!("expected Undefined, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
    }

    #[test]
    fn strict_undefined_names_a_bare_variable() {
        let engine = TemplateEngine::memory(&[("t.txt", "a\nb\n{{ nope }}")]);
        match engine.render("t.txt", &Context::new()) {
            Err(TemplateError::Undefined { line, variable, .. }) => {
                assert_eq!(line, 3);
                assert_eq!(variable.as_str(), "nope");
            }
            Err(other) => panic!("expected Undefined, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
    }

    #[test]
    fn missing_templates_are_not_found() {
        let engine = TemplateEngine::memory(&[]);
        match engine.render("nope.txt", &Context::new()) {
            Err(TemplateError::NotFound { name }) => assert_eq!(name.as_str(), "nope.txt"),
            Err(other) => panic!("expected NotFound, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
    }

    #[test]
    fn syntax_errors_name_the_template() {
        let engine = TemplateEngine::memory(&[("broken.txt", "{% if x %}unclosed")]);
        match engine.render("broken.txt", &Context::new()) {
            Err(TemplateError::Syntax {
                name,
                line,
                message,
            }) => {
                assert_eq!(name.as_str(), "broken.txt");
                assert!(line >= 1, "{line}");
                assert!(!message.is_empty(), "parser message required");
            }
            Err(other) => panic!("expected Syntax, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
    }

    #[test]
    fn fuel_exhausts_with_a_typed_error() {
        let engine = TemplateEngine::memory(&[(
            "loop.txt",
            "{% for i in range(0, 1000) %}{{ i }}{% endfor %}",
        )])
        .with_fuel(100);
        match engine.render("loop.txt", &Context::new()) {
            Err(TemplateError::FuelExhausted { name, .. }) => {
                assert_eq!(name.as_str(), "loop.txt");
            }
            Err(other) => panic!("expected FuelExhausted, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
    }

    #[test]
    fn recursion_is_capped() {
        let engine = TemplateEngine::memory(&[(
            "rec.txt",
            "{% macro go(x) %}{{ go(x) }}{% endmacro %}{{ go(1) }}",
        )]);
        match engine.render("rec.txt", &Context::new()) {
            Err(TemplateError::RecursionLimitExceeded { name, .. }) => {
                assert_eq!(name.as_str(), "rec.txt");
            }
            Err(other) => panic!("expected RecursionLimitExceeded, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
    }

    #[test]
    fn output_is_size_limited() {
        let engine =
            TemplateEngine::memory(&[("big.txt", "{% for i in range(0, 1000) %}xx{% endfor %}")])
                .with_max_output_bytes(10);
        match engine.render("big.txt", &Context::new()) {
            Err(TemplateError::OutputLimitExceeded { limit, actual }) => {
                assert_eq!(limit, 10);
                assert!(actual > 10, "{actual}");
            }
            Err(other) => panic!("expected OutputLimitExceeded, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
    }

    #[test]
    fn decimal_arithmetic_is_not_computable() {
        let engine = TemplateEngine::memory(&[("t.txt", "{{ total + total }}")]);
        match engine.render("t.txt", &invoice_context()) {
            Err(TemplateError::NotComputable { message }) => {
                assert!(message.contains("do not compute money"), "{message}");
            }
            Err(other) => panic!("expected NotComputable, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
        let engine = TemplateEngine::memory(&[("t.txt", "{{ total * 2 }}")]);
        match engine.render("t.txt", &invoice_context()) {
            Err(TemplateError::NotComputable { .. }) => {}
            Err(other) => panic!("expected NotComputable, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
    }

    #[test]
    fn undefined_in_arithmetic_stays_undefined() {
        // Mapping order: a missing variable in an expression reports the
        // undefined name, not a computation rejection.
        let engine = TemplateEngine::memory(&[("t.txt", "{{ missing + 1 }}")]);
        match engine.render("t.txt", &invoice_context()) {
            Err(TemplateError::Undefined { .. }) => {}
            Err(other) => panic!("expected Undefined, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
    }

    #[test]
    fn decimal_methods_are_not_computable() {
        let engine = TemplateEngine::memory(&[("t.txt", "{{ total.round() }}")]);
        match engine.render("t.txt", &invoice_context()) {
            Err(TemplateError::NotComputable { message }) => {
                assert!(message.contains("round"), "{message}");
            }
            Err(other) => panic!("expected NotComputable, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
    }

    #[test]
    fn money_numbers_and_words_flow_through_filters() {
        let engine = TemplateEngine::memory(&[(
            "t.txt",
            "{{ total | money('PKR', locale='ur-PK') }} {{ total | number(4) }} \
             {{ total | amount_in_words('PKR') }} {{ item_count | number(0) }}",
        )]);
        assert_eq!(
            rendered_or_panic(&engine, "t.txt", &invoice_context()),
            "Rs 1,234.56 1,234.5600 One Thousand Two Hundred Thirty-Four \
             Rupees and Fifty-Six Paisa Only 3"
        );
    }

    #[test]
    fn filter_rejections_surface_typed() {
        let engine = TemplateEngine::memory(&[("t.txt", "{{ total | money('XYZ') }}")]);
        match engine.render("t.txt", &invoice_context()) {
            Err(TemplateError::Filter { message }) => {
                assert!(message.contains("unknown currency 'XYZ'"), "{message}");
            }
            Err(other) => panic!("expected Filter, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
        let engine =
            TemplateEngine::memory(&[("t.txt", "{{ total | money('PKR', locale='xx') }}")]);
        match engine.render("t.txt", &invoice_context()) {
            Err(TemplateError::Filter { message }) => {
                assert!(message.contains("unknown locale 'xx'"), "{message}");
            }
            Err(other) => panic!("expected Filter, got {other}"),
            Ok(text) => panic!("rendered: {text}"),
        }
    }

    #[test]
    fn dates_and_datetimes_format_and_shift() {
        let engine = TemplateEngine::memory(&[(
            "t.txt",
            "{{ due | date(format='long') }} \
             {{ issued | datetime(format='medium', tz='+05:00') }}",
        )]);
        assert_eq!(
            rendered_or_panic(&engine, "t.txt", &invoice_context()),
            "15 January 2025 (Wednesday) Jan 15, 2025 14:30"
        );
    }

    #[test]
    fn money_comparisons_order_numerically() {
        // Textual ordering would say "9.90" > "10.00"; the numeric ordering
        // on decimals must not.
        let engine = TemplateEngine::memory(&[(
            "t.txt",
            "{% if total > threshold %}Big{% else %}Small{% endif %}",
        )]);
        let context = Context::new()
            .insert("total", Decimal::new(990, 2))
            .insert("threshold", Decimal::new(1000, 2));
        assert_eq!(rendered_or_panic(&engine, "t.txt", &context), "Small");
    }

    #[test]
    fn recompilation_follows_version_changes() {
        let mut engine = TemplateEngine::memory(&[("t.txt", "one {{ x }}")]);
        let context = Context::new().insert("x", 1_u32);
        assert_eq!(rendered_or_panic(&engine, "t.txt", &context), "one 1");
        engine.loader_mut().insert("t.txt", "two {{ x }}");
        assert_eq!(rendered_or_panic(&engine, "t.txt", &context), "two 1");
        let statistics = engine.statistics();
        assert_eq!(statistics.source_loads, 2);
        assert_eq!(statistics.template_compiles, 2);
    }

    #[test]
    fn unchanged_versions_are_not_recompiled() {
        let engine = TemplateEngine::memory(&[("t.txt", "static")]);
        let context = Context::new();
        assert_eq!(rendered_or_panic(&engine, "t.txt", &context), "static");
        assert_eq!(rendered_or_panic(&engine, "t.txt", &context), "static");
        let statistics = engine.statistics();
        assert_eq!(statistics.source_loads, 2);
        assert_eq!(statistics.template_compiles, 1);
    }
}
