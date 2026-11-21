//! End-to-end tests over the public API: sources with load counting, strict
//! undefined through includes, golden renders in each output kind, and the
//! JSON intake rules the platform relies on.

use std::sync::atomic::{AtomicU64, Ordering};

use rust_decimal::Decimal;
use spot_template::{
    Context, CtxValue, MemorySource, OutputKind, TemplateEngine, TemplateError, TemplateSource,
    Version,
};
use time::{Date, Month};

/// Unwrap for tests without tripping the workspace's `unwrap_used` lint.
#[track_caller]
fn got<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(err) => panic!("expected Ok, got {err:?}"),
    }
}

/// A source that counts `load` calls, like an app asking its database.
#[derive(Default)]
struct CountingSource {
    loads: AtomicU64,
    body: MemorySource,
}

impl TemplateSource for CountingSource {
    fn load(&self, name: &str) -> Option<(String, Version)> {
        self.loads.fetch_add(1, Ordering::SeqCst);
        self.body.load(name)
    }
}

fn issued() -> Date {
    got(Date::from_calendar_date(2026, Month::March, 10))
}

fn invoice_context() -> Context {
    Context::new()
        .insert("number", "INV-2026-004")
        .insert("customer", "Khan Traders <Lahore>")
        .insert("total", Decimal::new(-1_234_550, 2))
        .insert("subtotal", Decimal::new(1_100_000, 2))
        .insert("issued", CtxValue::Date(issued()))
        .insert(
            "due",
            CtxValue::DateTime(got(issued().with_hms(14, 45, 0)).assume_utc()),
        )
        .insert(
            "items",
            CtxValue::List(vec![CtxValue::from(
                Context::new()
                    .insert("desc", "Widget *A*")
                    .insert("qty", 2_u32)
                    .insert("price", Decimal::new(550_000, 2)),
            )]),
        )
}

#[test]
fn output_kind_selection_by_extension() {
    assert_eq!(OutputKind::from_name("a.html"), OutputKind::Html);
    assert_eq!(OutputKind::from_name("invoice.typ"), OutputKind::Typst);
    assert_eq!(OutputKind::from_name("receipt"), OutputKind::Text);
    assert_eq!(OutputKind::from_name("x.txt"), OutputKind::Text);
}

#[test]
fn counting_source_loads_per_render_and_compiles_once_per_version() {
    let mut body = MemorySource::new();
    body.insert("t.txt", "static {{ 1 + 1 }}");
    let source = CountingSource {
        loads: AtomicU64::new(0),
        body,
    };
    let engine = TemplateEngine::new(source);
    let context = Context::new();
    assert_eq!(got(engine.render("t.txt", &context)), "static 2");
    assert_eq!(got(engine.render("t.txt", &context)), "static 2");
    // loaded once per render, compiled once in total
    let stats = engine.statistics();
    assert_eq!(stats.source_loads, 2);
    assert_eq!(stats.template_compiles, 1);
    assert_eq!(engine.loader().loads.load(Ordering::SeqCst), 2);
}

#[test]
fn version_change_recompiles() {
    let mut engine = TemplateEngine::memory(&[("t.txt", "one")]);
    assert_eq!(got(engine.render("t.txt", &Context::new())), "one");
    engine.loader_mut().insert("t.txt", "two");
    assert_eq!(got(engine.render("t.txt", &Context::new())), "two");
    let stats = engine.statistics();
    assert_eq!(stats.template_compiles, 2, "new version must recompile");
}

#[test]
fn strict_undefined_in_a_later_line_names_template_line_and_variable() {
    let engine = TemplateEngine::memory(&[("line.txt", "one\ntwo {{ order.missing }}")]);
    let context = Context::new().insert("order", CtxValue::Map(Vec::new()));
    match engine.render("line.txt", &context) {
        Err(TemplateError::Undefined {
            name,
            line,
            variable,
            line_text,
        }) => {
            assert_eq!(name, "line.txt");
            assert_eq!(line, 2);
            assert_eq!(variable, "order.missing");
            assert_eq!(line_text, "two {{ order.missing }}");
        }
        Err(other) => panic!("expected Undefined, got {other}"),
        Ok(text) => panic!("rendered: {text:?}"),
    }
}

#[test]
fn golden_render_in_each_output_kind() {
    let template = "{{ customer }} — {{ total | money('PKR') }} on {{ issued | date(format='medium') }} at {{ due | datetime(format='medium', tz='+05:00') }}";
    let engine = TemplateEngine::memory(&[
        ("inv.txt", template),
        ("inv.html", template),
        ("inv.typ", template),
    ]);
    let context = invoice_context();
    // Text: raw data, exactly as given.
    assert_eq!(
        got(engine.render("inv.txt", &context)),
        "Khan Traders <Lahore> — -Rs 12,345.50 on Mar 10, 2026 at Mar 10, 2026 19:45"
    );
    // HTML: data escaped.
    assert_eq!(
        got(engine.render("inv.html", &context)),
        "Khan Traders &lt;Lahore&gt; — -Rs 12,345.50 on Mar 10, 2026 at Mar 10, 2026 19:45"
    );
    // Typst: every markup-trigger character escaped, including a leading
    // minus (`\-` renders as a plain minus in Typst).
    assert_eq!(
        got(engine.render("inv.typ", &context)),
        "Khan Traders \\<Lahore\\> — \\-Rs 12,345.50 on Mar 10, 2026 at Mar 10, 2026 19:45"
    );
}

#[test]
fn from_json_money_must_arrive_as_decimal_not_float() {
    let json = serde_json::json!({ "total": 1.5, "name": "x" });
    match Context::from_json(json) {
        Err(TemplateError::InvalidInput { message }) => {
            assert!(message.contains("fraction"), "{message}");
        }
        other => panic!("expected rejection, got {other:?}"),
    }
    // an exponent number is also a float
    let json = serde_json::json!({ "total": 1e2 });
    assert!(Context::from_json(json).is_err());
    // a quoted decimal string stays text, never a number
    let json = serde_json::json!({ "price": "1.50" });
    let context = got(Context::from_json(json));
    let engine = TemplateEngine::memory(&[("t.txt", "{{ price }}")]);
    assert_eq!(got(engine.render("t.txt", &context)), "1.50");
    // money() on that text is an error: money arrives as a Decimal
    let engine = TemplateEngine::memory(&[("m.txt", "{{ price | money('PKR') }}")]);
    match engine.render("m.txt", &context) {
        Err(TemplateError::Filter { message }) => {
            assert!(message.contains("exact decimal"), "{message}");
        }
        other => panic!("expected a typed rejection, got {other:?}"),
    }
}

#[test]
fn list_and_nested_map_contexts_render() {
    let engine = TemplateEngine::memory(&[(
        "rows.txt",
        "{% for i in items %}{{ i.desc }} x{{ i.qty }} {{ i.price | money('USD') }}\n{% endfor %}total {{ total | money('USD') }}",
    )]);
    let rendered = got(engine.render("rows.txt", &invoice_context()));
    assert_eq!(rendered, "Widget *A* x2 $5,500.00\ntotal -$12,345.50");
}
