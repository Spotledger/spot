//! The print seam of `GET /api/v1/print/{doctype}/{id}` (`print_get`, contract 0.7.0,
//! WP-PLT-13.T2).
//!
//! `spot-http` does not depend on `spot-print`. It defines [`PrintPort`], the one operation in
//! the shape the router needs, and the host (WP-PRD-01.T8) writes a thin adapter from the print
//! service onto it. The port is responsible for loading the document **as the caller** (read
//! permission and field rights are enforced there, through the engine, never re-implemented in
//! the router) and for rendering it. [`MemoryPrintPort`] is the in-memory fake the tests (and a
//! host's smoke tests) use; it renders a tiny, valid-looking PDF.
//!
//! A port never returns a path, a template or document content in an error: the router maps
//! [`PrintPortError`] to the registry's default message for its code and discards the port's
//! own text.
//!
//! # The field guard (WP-SEC-04.T12, `Channel::Print`)
//!
//! A printed page is a data channel: the print service renders whatever context it is handed, so
//! the context must be redacted first. [`GuardedPrintPort`] is the adapter shape that guarantees
//! it. It splits the port into two seams the host implements: [`PrintSource`] loads the document
//! **as the caller** (read permission is the source's, and a refusal is the one `not_found`), and
//! [`PrintRenderer`] renders a document it is given (the host's wrapper over `spot-print`). Between
//! them [`guard_document`] applies `FieldGuard::redact(Channel::Print, ..)` to the loaded document:
//! a hidden field is absent, a masked field is masked (a print degrades to the masked form instead
//! of refusing), a child-table row is judged column by column, and a guard error fails closed (the
//! document is emptied and the print is a failed render). The renderer never sees an unredacted
//! value, so no template can reach one.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use std::sync::Arc;

use spot_app_api::Document;
use spot_field_guard::{Channel, FieldGuard};
use spot_permissions::{DocFacts, Principal};
use spot_storage_contract::RecordId;

use super::registry::Code;
use super::service::{ApiError, BoxFuture, RequestCtx};

/// The rendered PDF. A printed document is small, so it crosses the port whole.
pub type PdfBytes = Vec<u8>;

/// A failure at the port, in terms of HTTP and the registry. The router honours the `code`
/// (and the registry's status and `retryable` for it) and ignores `message`, which exists for
/// the port's own logs: an error body never carries template text or document content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrintPortError {
    /// The HTTP status the service intends (the registry's status wins for a known code).
    pub status: u16,
    /// The stable dotted code, for example `print.format_unknown`.
    pub code: String,
    /// Plain-language message for logs; never shown to the caller.
    pub message: String,
    /// Whether the same request may succeed later.
    pub retryable: bool,
}

impl PrintPortError {
    /// An error with the given parts.
    pub fn new(
        status: u16,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        PrintPortError {
            status,
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }

    /// The unknown-or-forbidden answer of `print_get`: a missing document, a document the
    /// caller may not read and an unknown DocType are the same error.
    pub fn not_found() -> Self {
        PrintPortError::from_code(Code::DocumentNotFound)
    }

    /// The DocType defines no print format with the requested name.
    pub fn format_unknown() -> Self {
        PrintPortError::from_code(Code::PrintFormatUnknown)
    }

    /// The render failed.
    pub fn render_failed() -> Self {
        PrintPortError::from_code(Code::PrintRenderFailed)
    }

    fn from_code(code: Code) -> Self {
        PrintPortError::new(
            code.status().as_u16(),
            code.as_str(),
            code.default_message(),
            code.retryable(),
        )
    }
}

impl From<ApiError> for PrintPortError {
    fn from(error: ApiError) -> Self {
        PrintPortError {
            status: error.status().as_u16(),
            code: error.code.as_str().to_owned(),
            message: error.message.clone(),
            retryable: error.retryable(),
        }
    }
}

/// The print service behind `print_get`. Object-safe with a boxed future like the other
/// services; the port decides permission itself from the request context.
pub trait PrintPort: Send + Sync + 'static {
    /// Renders document `id` of `doctype` through the print format named `format` (the
    /// DocType's default format when `None`), as the caller.
    ///
    /// The permission check comes first: a missing document, a document the caller may not
    /// read and an unknown DocType all fail as [`PrintPortError::not_found`], and only then is
    /// the format looked up (`print.format_unknown`). The page shows exactly what
    /// `resource_get` would return for the caller (absent fields absent, masked fields masked).
    fn render<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        format: Option<&'a str>,
    ) -> BoxFuture<'a, Result<PdfBytes, PrintPortError>>;
}

struct StoredDocument {
    /// Print formats of the DocType; the first is the default.
    formats: Vec<String>,
    /// Bearer tokens that may read the document.
    readers: BTreeSet<String>,
}

#[derive(Default)]
struct MemoryInner {
    documents: BTreeMap<(String, RecordId), StoredDocument>,
    fail_next: Option<PrintPortError>,
    renders: usize,
}

/// An in-memory [`PrintPort`]. A document is readable only by the bearer tokens it was
/// [`allow`](MemoryPrintPort::allow)ed to, unless the port is [`open`](MemoryPrintPort::open).
pub struct MemoryPrintPort {
    open: bool,
    inner: Mutex<MemoryInner>,
}

/// The format an [`open`](MemoryPrintPort::open) port knows.
pub const OPEN_FORMAT: &str = "standard";

impl MemoryPrintPort {
    /// An empty port: nothing is readable until a document is added and allowed.
    pub fn new() -> Self {
        MemoryPrintPort {
            open: false,
            inner: Mutex::new(MemoryInner::default()),
        }
    }

    /// A port that prints any document id of any DocType for any authenticated caller, with
    /// the one format [`OPEN_FORMAT`] (for conformance walks that only need a PDF).
    pub fn open() -> Self {
        MemoryPrintPort {
            open: true,
            inner: Mutex::new(MemoryInner::default()),
        }
    }

    /// Adds a document whose DocType has `formats` (the first is the default). Nobody may read
    /// it until [`allow`](MemoryPrintPort::allow).
    pub fn add_document(&self, doctype: &str, id: RecordId, formats: &[&str]) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.documents.insert(
                (doctype.to_owned(), id),
                StoredDocument {
                    formats: formats.iter().map(|f| (*f).to_owned()).collect(),
                    readers: BTreeSet::new(),
                },
            );
        }
    }

    /// Lets the bearer `token` read (and so print) the document.
    pub fn allow(&self, token: &str, doctype: &str, id: RecordId) {
        if let Ok(mut inner) = self.inner.lock()
            && let Some(document) = inner.documents.get_mut(&(doctype.to_owned(), id))
        {
            document.readers.insert(token.to_owned());
        }
    }

    /// The next render fails with `error`, after the permission check.
    pub fn fail_next(&self, error: PrintPortError) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.fail_next = Some(error);
        }
    }

    /// How many renders succeeded or failed past the permission check.
    pub fn render_count(&self) -> usize {
        self.inner.lock().map_or(0, |inner| inner.renders)
    }
}

impl Default for MemoryPrintPort {
    fn default() -> Self {
        MemoryPrintPort::new()
    }
}

/// A tiny PDF whose text names the DocType, id and format, so a test can tell what was
/// rendered. (Not a layout engine: only the magic bytes and the trailer matter to the router.)
fn fake_pdf(doctype: &str, id: RecordId, format: &str) -> PdfBytes {
    format!(
        "%PDF-1.4\n% memory print port\n1 0 obj << /Type /Catalog >> endobj\n\
         % doctype={doctype} id={id} format={format}\ntrailer << /Root 1 0 R >>\n%%EOF\n"
    )
    .into_bytes()
}

impl PrintPort for MemoryPrintPort {
    fn render<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        format: Option<&'a str>,
    ) -> BoxFuture<'a, Result<PdfBytes, PrintPortError>> {
        Box::pin(async move {
            let Some(credential) = &ctx.credential else {
                return Err(PrintPortError::from(ApiError::from_code(
                    Code::AuthRequired,
                )));
            };
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| PrintPortError::render_failed())?;
            // Permission first: an unreadable document never reveals whether the format exists.
            let formats: Vec<String> = if self.open {
                vec![OPEN_FORMAT.to_owned()]
            } else {
                let document = inner
                    .documents
                    .get(&(doctype.to_owned(), id))
                    .filter(|d| d.readers.contains(&credential.token))
                    .ok_or_else(PrintPortError::not_found)?;
                document.formats.clone()
            };
            let chosen = match format {
                Some(name) => formats
                    .iter()
                    .find(|f| f.as_str() == name)
                    .ok_or_else(PrintPortError::format_unknown)?,
                None => formats.first().ok_or_else(PrintPortError::format_unknown)?,
            };
            inner.renders += 1;
            if let Some(error) = inner.fail_next.take() {
                return Err(error);
            }
            Ok(fake_pdf(doctype, id, chosen))
        })
    }
}

// ----------------------------------------------------------------------------------- the guard

/// A document loaded as the caller, with what the guard needs to judge it.
pub struct PrintInput {
    /// The document exactly as the caller's read returned it.
    pub document: Document,
    /// The caller, as the permission engine knows them.
    pub principal: Principal,
    /// The facts of the document the permission rules look at (its owner, state, ...).
    pub facts: DocFacts,
}

/// Loads the document to print **as the caller**.
pub trait PrintSource: Send + Sync + 'static {
    /// Loads document `id` of `doctype` for the caller of `ctx`. A missing document, a document
    /// the caller may not read and an unknown DocType all fail as [`PrintPortError::not_found`].
    fn load<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, Result<PrintInput, PrintPortError>>;
}

/// Renders a document that has already been through the field guard.
pub trait PrintRenderer: Send + Sync + 'static {
    /// Renders `document` through the print format named `format` (the DocType's default when
    /// `None`). Errors: `print.format_unknown`, `print.render_failed`.
    fn render<'a>(
        &'a self,
        document: &'a Document,
        format: Option<&'a str>,
    ) -> BoxFuture<'a, Result<PdfBytes, PrintPortError>>;
}

/// Redacts `input` for the print channel.
///
/// # Errors
/// `print.render_failed` when the guard cannot judge the document (an unknown DocType, a full
/// trace queue): the document is emptied first, so a caller that ignores the error prints nothing.
pub fn guard_document(guard: &FieldGuard, input: &mut PrintInput) -> Result<(), PrintPortError> {
    let doctype = input.document.doctype().as_str().to_owned();
    let facts = DocFacts {
        doctype: doctype.clone(),
        ..input.facts.clone()
    };
    guard
        .redact(
            Channel::Print,
            &input.principal,
            &doctype,
            &facts,
            &mut input.document,
        )
        .map_err(|_| PrintPortError::render_failed())
}

/// The [`PrintPort`] that loads as the caller, redacts through the field guard and only then
/// renders.
pub struct GuardedPrintPort {
    source: Arc<dyn PrintSource>,
    renderer: Arc<dyn PrintRenderer>,
    guard: FieldGuard,
}

impl GuardedPrintPort {
    /// A port over a source, a renderer and the site's field guard.
    pub fn new(
        source: Arc<dyn PrintSource>,
        renderer: Arc<dyn PrintRenderer>,
        guard: FieldGuard,
    ) -> Self {
        GuardedPrintPort {
            source,
            renderer,
            guard,
        }
    }
}

impl PrintPort for GuardedPrintPort {
    fn render<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        doctype: &'a str,
        id: RecordId,
        format: Option<&'a str>,
    ) -> BoxFuture<'a, Result<PdfBytes, PrintPortError>> {
        Box::pin(async move {
            // Permission first: an unreadable document never reveals whether the format exists.
            let mut input = self.source.load(ctx, doctype, id).await?;
            guard_document(&self.guard, &mut input)?;
            self.renderer.render(&input.document, format).await
        })
    }
}

#[cfg(test)]
mod guard_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::atomic::{AtomicUsize, Ordering};

    use spot_app_api::document::ChildRow;
    use spot_app_api::prelude::{Name as FieldName, Value as FieldValue};
    use spot_core::{ManualClock, SiteId};
    use spot_field_guard::{
        AuditRecord, BoundedQueue, FieldAccessRecord, MetaProtection, ProtectionSource,
    };
    use spot_meta::{
        BuildOptions, Definition, FieldSpec, FieldType, MaskSpec, ProtectionGroupSpec, Registry,
    };
    use spot_permissions::{ModelTexts, PermissionEngine, Snapshot, load};

    use super::*;
    use crate::request::RequestId;

    const CARD: &str = "4111111111111111";
    const IBAN: &str = "PK36SCBL0000001123456702";

    fn grouped(name: &str, group: &str) -> FieldSpec {
        let mut field = FieldSpec::new(name, FieldType::Text);
        field.protection_group = Some(group.to_owned());
        field.masking = Some(MaskSpec::placeholder());
        field
    }

    fn guard() -> FieldGuard {
        let mut invoice = Definition::new("invoice");
        let mut lines = FieldSpec::new("lines", FieldType::Table);
        lines.options = Some("invoice_line".into());
        invoice.fields = vec![
            FieldSpec::new("number", FieldType::Text),
            grouped("card", "pci"),
            lines,
        ];
        invoice.protection_groups = vec![ProtectionGroupSpec::new("pci", "Cards")];
        let mut line = Definition::new("invoice_line");
        line.is_child = true;
        line.fields = vec![
            FieldSpec::new("memo", FieldType::Text),
            grouped("iban", "bank"),
        ];
        line.protection_groups = vec![ProtectionGroupSpec::new("bank", "Bank")];
        let meta = Registry::with_options(vec![invoice, line], BuildOptions::new())
            .unwrap_or_else(|e| panic!("meta: {e:?}"))
            .effective();
        let source = Arc::new(MetaProtection::new(&meta));
        let (mut grants, mut field_grants) = (String::new(), String::new());
        for role in ["Reader", "Masked", "Full"] {
            for doctype in ["invoice", "invoice_line"] {
                grants.push_str(&format!(
                    "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\nrights = [\"read\", \"report\"]\n"
                ));
            }
        }
        for (role, access) in [
            ("Masked", r#"["read_masked", "report"]"#),
            ("Full", r#"["read", "report"]"#),
        ] {
            for (doctype, group) in [("invoice", "pci"), ("invoice_line", "bank")] {
                field_grants.push_str(&format!(
                    "[[entries]]\nrole = \"{role}\"\ndoctype = \"{doctype}\"\ntarget = {{ group = \"{group}\" }}\naccess = {access}\ndeny = false\n"
                ));
            }
        }
        let model = load(&ModelTexts {
            grants,
            field_grants,
            groups: "[[entries]]\nname = \"pci\"\n[[entries]]\nname = \"bank\"\n".to_owned(),
            shares: String::new(),
        })
        .unwrap_or_else(|e| panic!("model: {:?}", e.as_slice()));
        let mut snapshot =
            Snapshot::build(&meta, &model).unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
        for doctype in ["invoice", "invoice_line"] {
            let facts = source.protection(doctype).unwrap();
            snapshot = snapshot
                .with_field_facts(doctype, facts.fields.clone())
                .unwrap_or_else(|e| panic!("{:?}", e.as_slice()));
        }
        FieldGuard::new(
            Arc::new(PermissionEngine::new(Arc::new(snapshot))),
            source,
            Arc::new(BoundedQueue::<FieldAccessRecord>::new(64)),
            Arc::new(BoundedQueue::<AuditRecord>::new(64)),
            Arc::new(ManualClock::new(
                spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
            )),
        )
    }

    fn name(text: &str) -> FieldName {
        FieldName::new(text).unwrap()
    }

    fn input(doctype: &str, role: &str) -> PrintInput {
        let mut document = Document::new(name(doctype), RecordId::new_v7());
        document.set(name("number"), FieldValue::Text("INV-1".to_owned()));
        document.set(name("card"), FieldValue::Text(CARD.to_owned()));
        document.rows_mut(name("lines")).push(ChildRow {
            id: RecordId::new_v7(),
            fields: [
                (name("memo"), FieldValue::Text("first".to_owned())),
                (name("iban"), FieldValue::Text(IBAN.to_owned())),
            ]
            .into(),
        });
        PrintInput {
            document,
            principal: Principal::with_roles(role, &[role]),
            facts: DocFacts::default(),
        }
    }

    fn dump(input: &PrintInput) -> String {
        format!("{:?}", input.document)
    }

    #[test]
    fn hidden_is_absent_masked_is_masked_and_a_value_needs_the_right() {
        let guard = guard();
        let mut full = input("invoice", "Full");
        guard_document(&guard, &mut full).unwrap();
        assert!(dump(&full).contains(CARD) && dump(&full).contains(IBAN));

        let mut masked = input("invoice", "Masked");
        guard_document(&guard, &mut masked).unwrap();
        assert!(!dump(&masked).contains(CARD) && !dump(&masked).contains(IBAN));
        assert!(
            dump(&masked).contains("INV-1"),
            "a plain field still prints"
        );
        assert_eq!(
            masked.document.get("card"),
            &FieldValue::Text("***".to_owned())
        );

        let mut reader = input("invoice", "Reader");
        guard_document(&guard, &mut reader).unwrap();
        assert!(!dump(&reader).contains(CARD) && !dump(&reader).contains(IBAN));
        assert_eq!(
            reader.document.get("card"),
            &FieldValue::Null,
            "a hidden field is absent"
        );
        assert!(
            dump(&reader).contains("first"),
            "a plain child column still prints"
        );
    }

    struct FakeSource {
        role: &'static str,
        doctype: &'static str,
        refuse: bool,
    }

    impl PrintSource for FakeSource {
        fn load<'a>(
            &'a self,
            _ctx: &'a RequestCtx,
            _doctype: &'a str,
            _id: RecordId,
        ) -> BoxFuture<'a, Result<PrintInput, PrintPortError>> {
            Box::pin(async move {
                if self.refuse {
                    return Err(PrintPortError::not_found());
                }
                Ok(input(self.doctype, self.role))
            })
        }
    }

    #[derive(Default)]
    struct Seen {
        calls: AtomicUsize,
        text: Mutex<String>,
    }

    struct Recording(Arc<Seen>);

    impl PrintRenderer for Recording {
        fn render<'a>(
            &'a self,
            document: &'a Document,
            _format: Option<&'a str>,
        ) -> BoxFuture<'a, Result<PdfBytes, PrintPortError>> {
            Box::pin(async move {
                self.0.calls.fetch_add(1, Ordering::SeqCst);
                let text = format!("{document:?}");
                *self.0.text.lock().unwrap() = text.clone();
                Ok(format!("%PDF-1.4\n{text}\n%%EOF\n").into_bytes())
            })
        }
    }

    fn ctx() -> RequestCtx {
        RequestCtx {
            site: SiteId::new("print.example").unwrap(),
            credential: None,
            idempotency_key: None,
            language: None,
            request_id: RequestId::generate(),
            peer: None,
        }
    }

    fn port(
        role: &'static str,
        doctype: &'static str,
        refuse: bool,
    ) -> (GuardedPrintPort, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let port = GuardedPrintPort::new(
            Arc::new(FakeSource {
                role,
                doctype,
                refuse,
            }),
            Arc::new(Recording(seen.clone())),
            guard(),
        );
        (port, seen)
    }

    #[tokio::test]
    async fn the_renderer_only_ever_sees_the_redacted_document() {
        let (port, seen) = port("Reader", "invoice", false);
        let pdf = port
            .render(&ctx(), "invoice", RecordId::new_v7(), None)
            .await
            .unwrap();
        let text = String::from_utf8(pdf).unwrap();
        assert!(!text.contains(CARD) && !text.contains(IBAN));
        assert_eq!(seen.calls.load(Ordering::SeqCst), 1);
        assert!(!seen.text.lock().unwrap().contains(CARD));
    }

    #[tokio::test]
    async fn a_guard_error_fails_closed_and_a_refused_load_never_renders() {
        // The DocType is unknown to the protection source: nothing is rendered.
        let (port, seen) = port("Full", "ghost_doctype", false);
        let error = port
            .render(&ctx(), "ghost_doctype", RecordId::new_v7(), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, "print.render_failed");
        assert_eq!(seen.calls.load(Ordering::SeqCst), 0);

        let (refused_port, seen) = self::port("Full", "invoice", true);
        let error = refused_port
            .render(&ctx(), "invoice", RecordId::new_v7(), None)
            .await
            .unwrap_err();
        assert_eq!(error, PrintPortError::not_found());
        assert_eq!(seen.calls.load(Ordering::SeqCst), 0);
    }
}
