//! The JSON log layer: one object per line with a fixed set of leading keys.
//!
//! Keys: `ts` (RFC 3339 UTC), `level`, `target`, `message`, then the fields of every span the
//! event is inside (innermost wins; this includes `site`, `request_id` and `trace_id` when the
//! request span has them), `span_id` (hex id of the innermost span, unless a span field of that
//! name exists), then the event's own fields, which win over span fields of the same name.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::io::Write as _;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Record};
use tracing::{Event, Id, Subscriber};
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::{Context, Layer};
use tracing_subscriber::registry::LookupSpan;

/// Keys the layer writes itself; a field of the same name is renamed with a `field_` prefix.
const RESERVED: [&str; 4] = ["ts", "level", "target", "message"];

/// Appends `s` as a JSON string literal.
fn push_json_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Collects fields as `name -> rendered JSON value`.
#[derive(Default)]
struct Collect {
    fields: BTreeMap<String, String>,
    message: Option<String>,
}

impl Collect {
    fn put(&mut self, field: &Field, rendered: String) {
        self.fields.insert(field.name().to_owned(), rendered);
    }
}

impl Visit for Collect {
    fn record_str(&mut self, field: &Field, value: &str) {
        let mut out = String::new();
        push_json_string(&mut out, value);
        if field.name() == "message" {
            self.message = Some(out);
        } else {
            self.put(field, out);
        }
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.put(field, value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.put(field, value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.put(field, value.to_string());
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        if value.is_finite() {
            self.put(field, value.to_string());
        } else {
            self.put(field, "null".to_owned());
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        let mut out = String::new();
        push_json_string(&mut out, &format!("{value:?}"));
        if field.name() == "message" {
            self.message = Some(out);
        } else {
            self.put(field, out);
        }
    }
}

/// Span fields, stored in the span's extensions.
struct SpanFields(BTreeMap<String, String>);

/// The JSON layer. Build it with a `MakeWriter`; see [`crate::init`].
pub(crate) struct JsonLayer<W> {
    make_writer: W,
}

impl<W> JsonLayer<W> {
    pub(crate) fn new(make_writer: W) -> Self {
        Self { make_writer }
    }
}

impl<S, W> Layer<S> for JsonLayer<W>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    W: for<'a> MakeWriter<'a> + 'static,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut collect = Collect::default();
        attrs.record(&mut collect);
        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(SpanFields(collect.fields));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else { return };
        let mut collect = Collect::default();
        values.record(&mut collect);
        let mut ext = span.extensions_mut();
        if let Some(SpanFields(fields)) = ext.get_mut::<SpanFields>() {
            fields.extend(collect.fields);
        }
    }

    fn on_event(&self, event: &Event<'_>, ctx: Context<'_, S>) {
        let mut merged: BTreeMap<String, String> = BTreeMap::new();
        let mut innermost_span_id = None;
        if let Some(scope) = ctx.event_scope(event) {
            // `from_root`: later (inner) spans overwrite earlier ones.
            for span in scope.from_root() {
                innermost_span_id = Some(span.id().into_u64());
                if let Some(SpanFields(fields)) = span.extensions().get::<SpanFields>() {
                    merged.extend(fields.iter().map(|(k, v)| (k.clone(), v.clone())));
                }
            }
        }
        if let Some(id) = innermost_span_id {
            merged
                .entry("span_id".to_owned())
                .or_insert_with(|| format!("\"{id:016x}\""));
        }
        let mut collect = Collect::default();
        event.record(&mut collect);
        merged.extend(collect.fields);

        let meta = event.metadata();
        let ts = OffsetDateTime::now_utc()
            .format(&Rfc3339)
            .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z"));
        let mut line = String::with_capacity(256);
        line.push_str("{\"ts\":");
        push_json_string(&mut line, &ts);
        line.push_str(",\"level\":");
        push_json_string(&mut line, &meta.level().as_str().to_ascii_lowercase());
        line.push_str(",\"target\":");
        push_json_string(&mut line, meta.target());
        line.push_str(",\"message\":");
        line.push_str(collect.message.as_deref().unwrap_or("\"\""));
        for (key, value) in &merged {
            line.push(',');
            if RESERVED.contains(&key.as_str()) {
                push_json_string(&mut line, &format!("field_{key}"));
            } else {
                push_json_string(&mut line, key);
            }
            line.push(':');
            line.push_str(value);
        }
        line.push_str("}\n");
        // A failed log write has nowhere to be reported.
        let _ = self.make_writer.make_writer().write_all(line.as_bytes());
    }
}
