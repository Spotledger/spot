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

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

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
