//! Server-side printing for one site: PDF pages from a document, through the template engine
//! and the embedded Typst renderer.
//!
//! # Formats and templates
//!
//! A DocType's print formats are template files in the site's template folder
//! (`services.print.template_root`, `templates` by default, inside the site folder):
//!
//! - `<doctype>.typ` is the **default** format;
//! - `<doctype>.<format>.typ` is the format called `<format>` (`?format=<format>`).
//!
//! A DocType with no default template of its own prints with the built-in layout: its title and
//! one row per field that has a value. A named format that has no file is
//! `print.format_unknown`. Templates are Typst markup with MiniJinja tags; every value they
//! print is escaped into a Typst string, so document data can never inject markup or code. Money
//! stays an exact decimal: a template can format it and never compute with it.
//!
//! The context a template sees: `doctype` (the label), `id`, `title`, `printed_at`, `doc` (every
//! readable field, typed), `tables` (child rows, typed), and `fields` (label and text of each
//! field that has a value, in display order, which is what the built-in layout prints).
//!
//! # Who may print what
//!
//! The document is loaded **as the caller** through the engine, so read permission, hidden
//! fields and masked fields apply exactly as for `GET /api/v1/resource/...`: the page shows what
//! the caller could read. A missing document, one the caller may not read and an unknown
//! DocType are the same `404`. Template names are validated and read from the template folder
//! only (no parent step, no symlink, a size limit), and nothing from a template or a document
//! ever appears in an error body or a log line.
//!
//! [`Printing::render_as_platform`] prints as the platform itself, for job and report code that
//! has no caller; it is the one function that skips the read check, and nothing routes to it.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_app_api::{DocTypeDef, Document, FieldKind};
use spot_core::{Clock, SiteId};
use spot_http::api::print::{PdfBytes, PrintPort, PrintPortError};
use spot_http::api::service::{BoxFuture, Credential, RequestCtx};
use spot_http::request::RequestId;
use spot_print::{AssetMap, FontSet, PdfRenderer, RenderLimits, RenderOptions};
use spot_template::{Context, CtxValue, TemplateEngine, TemplateSource, Version};
use tokio::sync::watch;

use crate::audit::PathClass;
use crate::live::SiteLive;
use crate::services::{CheckFuture, Level, RunFuture, Service};

/// The print routes' path (the port is not handed the request path); see `PathClass::of_path`.
const PRINT_ROUTE: &str = "/api/v1/print";

/// The built-in layout's template name. It starts with `__`, which no site file may.
const BUILT_IN: &str = "__default.typ";

/// The longest template the folder source reads.
const MAX_TEMPLATE_BYTES: u64 = 1024 * 1024;

const BUILT_IN_SOURCE: &str = r#"#set document(title: [{{ title }}])
#set text(font: ("Noto Sans",), size: 10pt)
#set page(paper: "a4", margin: 18mm)
#text(16pt)[*{{ doctype }}*] \
#text(12pt)[{{ title }}]
#v(4mm)
#table(columns: (auto, 1fr), inset: 5pt, stroke: 0.4pt + luma(170),
{%- for f in fields %}
  [*{{ f.label }}*], [{{ f.value }}],
{%- endfor %}
)
"#;

/// The template files of one site, read from its folder.
#[derive(Clone)]
pub struct Templates {
    root: PathBuf,
}

impl Templates {
    /// A source over the folder `root`.
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Whether `name` is a template file name this source will read: letters, digits and `_ - .`,
    /// no leading dot, no `..`, ending in `.typ`.
    fn safe(name: &str) -> bool {
        !name.is_empty()
            && name.len() <= 128
            && name.ends_with(".typ")
            && !name.starts_with('.')
            && !name.starts_with("__")
            && !name.contains("..")
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    }

    /// Whether the site has a template file called `name`.
    pub fn exists(&self, name: &str) -> bool {
        self.read(name).is_some()
    }

    fn read(&self, name: &str) -> Option<(String, Version)> {
        if !Self::safe(name) {
            return None;
        }
        let path = self.root.join(name);
        // A symlink is never followed: the folder is the whole of what a template may read.
        let meta = std::fs::symlink_metadata(&path).ok()?;
        if !meta.file_type().is_file() || meta.len() > MAX_TEMPLATE_BYTES {
            return None;
        }
        let text = std::fs::read_to_string(&path).ok()?;
        let mut hasher = DefaultHasher::new();
        meta.len().hash(&mut hasher);
        meta.modified()
            .ok()
            .and_then(|at| at.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .hash(&mut hasher);
        Some((text, Version::new(hasher.finish())))
    }
}

impl TemplateSource for Templates {
    fn load(&self, name: &str) -> Option<(String, Version)> {
        if name == BUILT_IN {
            return Some((BUILT_IN_SOURCE.to_owned(), Version::new(1)));
        }
        self.read(name)
    }
}

/// Why a page could not be made.
#[derive(Debug, PartialEq, Eq)]
pub enum PrintFailure {
    /// The DocType has no such print format.
    FormatUnknown,
    /// The renderer or the template failed. The cause is for the log only.
    Render(String),
}

/// The host's print service: the template folder, the font set and the renderer.
pub struct PrintService {
    templates: Templates,
    renderer: PdfRenderer<Templates>,
}

impl PrintService {
    /// A service over the template folder `root`, with the bundled fonts.
    pub fn new(root: PathBuf) -> Self {
        let templates = Templates::new(root);
        let engine = Arc::new(TemplateEngine::new(templates.clone()));
        Self {
            templates,
            renderer: PdfRenderer::new(engine, FontSet::bundled(), RenderLimits::default()),
        }
    }

    /// The template that prints `doctype` in `format` (the default format when `None` or
    /// `default`).
    ///
    /// # Errors
    ///
    /// [`PrintFailure::FormatUnknown`] for a named format with no template file.
    pub fn template_for(
        &self,
        doctype: &str,
        format: Option<&str>,
    ) -> Result<String, PrintFailure> {
        match format {
            None | Some("default") => {
                let own = format!("{doctype}.typ");
                Ok(if self.templates.exists(&own) {
                    own
                } else {
                    BUILT_IN.to_owned()
                })
            }
            Some(format) => {
                let valid = !format.is_empty()
                    && format.len() <= 64
                    && format.bytes().all(|b| {
                        b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'-')
                    });
                let name = format!("{doctype}.{format}.typ");
                if valid && self.templates.exists(&name) {
                    Ok(name)
                } else {
                    Err(PrintFailure::FormatUnknown)
                }
            }
        }
    }

    /// Renders `doc` (already read for whoever asked) in `format`.
    ///
    /// # Errors
    ///
    /// [`PrintFailure`]: an unknown format, or a template or renderer failure.
    pub async fn render_document(
        &self,
        def: &DocTypeDef,
        doc: &Document,
        format: Option<&str>,
        now: time::OffsetDateTime,
    ) -> Result<Vec<u8>, PrintFailure> {
        let template = self.template_for(def.name.as_str(), format)?;
        let context = context_of(def, doc, now);
        self.renderer
            .render(
                &template,
                &context,
                &AssetMap::new(),
                RenderOptions::new(now),
            )
            .await
            .map(|pdf| pdf.bytes)
            .map_err(|e| PrintFailure::Render(e.to_string()))
    }

    /// Renders the built-in layout over a one-row table: proves fonts and renderer work.
    pub async fn self_test(&self, now: time::OffsetDateTime) -> Result<(), String> {
        let context = Context::new()
            .insert("title", "self-test")
            .insert("doctype", "Self Test")
            .insert(
                "fields",
                CtxValue::List(vec![CtxValue::Map(vec![
                    ("label".into(), "Check".into()),
                    ("value".into(), "ok".into()),
                ])]),
            );
        self.renderer
            .render(
                BUILT_IN,
                &context,
                &AssetMap::new(),
                RenderOptions::new(now),
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

fn typed(value: &Value) -> CtxValue {
    match value {
        // ADR-087: a geo point or range has no print form yet, like bytes.
        Value::Null | Value::Bytes(_) | Value::GeoPoint(_) | Value::Range(_) => CtxValue::Null,
        Value::Bool(b) => CtxValue::Bool(*b),
        Value::Int(i) => CtxValue::Int(*i),
        Value::Decimal(d) => CtxValue::Decimal(*d),
        Value::Text(t) => CtxValue::Str(t.clone()),
        Value::Date(d) => CtxValue::Date(*d),
        Value::DateTime(t) => CtxValue::DateTime(*t),
        Value::Time(t) => CtxValue::Str(t.to_string()),
        Value::Id(id) => CtxValue::Str(id.to_string()),
        Value::Json(j) => CtxValue::Str(j.clone()),
    }
}

/// A value as the built-in layout shows it. Money keeps its exact digits.
fn shown(value: &Value) -> Option<String> {
    match value {
        Value::Null | Value::Bytes(_) | Value::GeoPoint(_) | Value::Range(_) => None,
        Value::Bool(b) => Some(if *b { "Yes" } else { "No" }.to_owned()),
        Value::Int(i) => Some(i.to_string()),
        Value::Decimal(d) => Some(d.to_string()),
        Value::Text(t) => (!t.is_empty()).then(|| t.clone()),
        Value::Date(d) => Some(d.to_string()),
        Value::DateTime(t) => Some(t.to_string()),
        Value::Time(t) => Some(t.to_string()),
        Value::Id(id) => Some(id.to_string()),
        Value::Json(j) => Some(j.clone()),
    }
}

fn context_of(def: &DocTypeDef, doc: &Document, now: time::OffsetDateTime) -> Context {
    let mut doc_map = Vec::new();
    let mut fields = Vec::new();
    let mut tables = Vec::new();
    for field in &def.fields {
        match &field.kind {
            // A secret never travels in a document, and a table is printed from its rows.
            FieldKind::Secret => {}
            FieldKind::Table { .. } | FieldKind::TableMultiselect { .. } => {
                let rows: Vec<CtxValue> = doc
                    .rows(field.name.as_str())
                    .iter()
                    .map(|row| {
                        CtxValue::Map(
                            row.fields
                                .iter()
                                .map(|(name, value)| (name.as_str().to_owned(), typed(value)))
                                .collect(),
                        )
                    })
                    .collect();
                tables.push((field.name.as_str().to_owned(), CtxValue::List(rows)));
            }
            _ => {
                let value = doc.get(field.name.as_str());
                doc_map.push((field.name.as_str().to_owned(), typed(value)));
                if let Some(text) = shown(value) {
                    fields.push(CtxValue::Map(vec![
                        ("label".to_owned(), CtxValue::Str(field.label.clone())),
                        (
                            "name".to_owned(),
                            CtxValue::Str(field.name.as_str().to_owned()),
                        ),
                        ("value".to_owned(), CtxValue::Str(text)),
                    ]));
                }
            }
        }
    }
    let title = def
        .title_field
        .as_ref()
        .and_then(|name| shown(doc.get(name.as_str())))
        .unwrap_or_else(|| doc.id().to_string());
    Context::new()
        .insert("doctype", def.label.clone())
        .insert("id", doc.id().to_string())
        .insert("title", title)
        .insert("printed_at", now)
        .insert("doc", CtxValue::Map(doc_map))
        .insert("tables", CtxValue::Map(tables))
        .insert("fields", CtxValue::List(fields))
}

/// Settings of a site's print service.
pub struct Settings {
    /// The site.
    pub site: SiteId,
    /// What the site serves right now.
    pub live: Arc<SiteLive>,
    /// The source of `now`.
    pub clock: Arc<dyn Clock>,
    /// The site's template folder.
    pub templates: PathBuf,
}

/// The print service as `spot-http`'s [`PrintPort`].
pub struct Printing {
    site: SiteId,
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    service: PrintService,
}

fn failure_to_port(failure: &PrintFailure) -> PrintPortError {
    match failure {
        PrintFailure::FormatUnknown => PrintPortError::format_unknown(),
        PrintFailure::Render(cause) => {
            tracing::warn!(cause = %cause, "a print render failed");
            PrintPortError::render_failed()
        }
    }
}

impl Printing {
    /// Builds the service: the fonts are parsed once, here.
    pub fn new(settings: &Settings) -> Arc<Self> {
        Arc::new(Self {
            site: settings.site.clone(),
            live: Arc::clone(&settings.live),
            clock: Arc::clone(&settings.clock),
            service: PrintService::new(settings.templates.clone()),
        })
    }

    /// The renderer behind the port.
    pub fn service(&self) -> &PrintService {
        &self.service
    }

    fn invocation(&self, actor: Actor) -> Invocation {
        Invocation {
            site: self.site.clone(),
            actor,
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        }
    }

    /// Prints `doctype`/`id` as the platform itself: for job and report code that has no
    /// caller. Nothing routes here, and it reads with the platform's rights, so the caller of
    /// this function decides who may ask.
    ///
    /// # Errors
    ///
    /// `document.not_found` for a missing document, `print.format_unknown`, or
    /// `print.render_failed`.
    pub async fn render_as_platform(
        &self,
        doctype: &str,
        id: RecordId,
        format: Option<&str>,
    ) -> Result<PdfBytes, PrintPortError> {
        let component = Name::new("print").map_err(|_| PrintPortError::render_failed())?;
        self.render_with(
            self.invocation(Actor::System { component }),
            doctype,
            id,
            format,
        )
        .await
    }

    async fn render_with(
        &self,
        inv: Invocation,
        doctype: &str,
        id: RecordId,
        format: Option<&str>,
    ) -> Result<PdfBytes, PrintPortError> {
        let engine = self
            .live
            .engine()
            .ok_or_else(PrintPortError::render_failed)?;
        // The read comes first: an unreadable document never reveals its formats.
        let doc = engine
            .load(&inv, doctype, id)
            .await
            .map_err(|_| PrintPortError::not_found())?
            .value;
        let def = engine
            .meta(doctype)
            .await
            .map_err(|_| PrintPortError::not_found())?;
        self.service
            .render_document(&def, &doc, format, inv.now)
            .await
            .map_err(|f| failure_to_port(&f))
    }
}

impl PrintPort for Printing {
    fn render<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        format: Option<&'a str>,
    ) -> BoxFuture<'a, Result<PdfBytes, PrintPortError>> {
        Box::pin(async move {
            let (Some(auth), Some(token)) = (
                self.live.auth(),
                ctx.credential.as_ref().map(|c| c.token.clone()),
            ) else {
                return Err(PrintPortError::not_found());
            };
            let request = RequestCtx {
                site: ctx.site.clone(),
                credential: Some(Credential::new(token)),
                idempotency_key: None,
                language: None,
                request_id: RequestId::generate(),
                peer: None,
            };
            let who = auth
                .authenticate_on(&request, PathClass::of_path(PRINT_ROUTE))
                .await
                .map_err(|_| PrintPortError::not_found())?;
            if auth.is_restricted(&who) {
                return Err(PrintPortError::not_found());
            }
            let actor = Actor::User {
                id: who.user_id,
                via: None,
            };
            self.render_with(self.invocation(actor), doctype, id, format)
                .await
        })
    }
}

/// The start-up check of printing: the renderer lays out and exports a page.
pub struct PrintCheck {
    printing: Option<Arc<Printing>>,
    clock: Arc<dyn Clock>,
}

impl PrintCheck {
    /// The service for `printing`, or one that reports print disabled when it is `None`.
    pub fn new(printing: Option<Arc<Printing>>, clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self { printing, clock })
    }
}

impl Service for PrintCheck {
    fn name(&self) -> &'static str {
        "print"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            match &self.printing {
                None => Err("printing is disabled in services.print".to_owned()),
                Some(printing) => printing
                    .service()
                    .self_test(self.clock.now_utc())
                    .await
                    .map_err(|e| format!("the renderer cannot make a page: {e}")),
            }
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
}
