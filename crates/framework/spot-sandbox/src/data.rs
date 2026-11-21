//! The tenant surfaces a sandbox masks and scans, and the store port that owns them.
//!
//! A real store is the restored tenant database. The port is deliberately coarse — one read
//! and one replace per surface — because masking happens on a closed clone and is followed by
//! a scan of the same surfaces. [`SandboxData`] is the in-memory store the tests and the
//! fixture use; a PostgreSQL store implements the same trait (the generic `*-pg` gating).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::Result;

/// One document of one DocType.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Document {
    /// The document's DocType.
    pub doctype: String,
    /// The record id (a UUIDv7 as text in these fixtures); ids are never masked.
    pub id: String,
    /// The field values by storage name.
    pub fields: BTreeMap<String, Value>,
}

impl Document {
    /// A document with no fields.
    #[must_use]
    pub fn new(doctype: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            doctype: doctype.into(),
            id: id.into(),
            fields: BTreeMap::new(),
        }
    }

    /// Sets one field, for fixtures.
    #[must_use]
    pub fn with(mut self, field: impl Into<String>, value: Value) -> Self {
        self.fields.insert(field.into(), value);
        self
    }
}

/// One journal (gl) row: a field value as it was written.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JournalRow {
    /// Monotonic sequence.
    pub seq: u64,
    /// The document's DocType.
    pub doctype: String,
    /// The document's id.
    pub doc_id: String,
    /// The field the row recorded.
    pub field: String,
    /// The value.
    pub value: Value,
}

/// One audit row: a field change with both sides.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AuditRow {
    /// Monotonic sequence.
    pub seq: u64,
    /// The document's DocType.
    pub doctype: String,
    /// The document's id.
    pub doc_id: String,
    /// The field that changed.
    pub field: String,
    /// The value before the change.
    pub before: Value,
    /// The value after the change.
    pub after: Value,
}

/// One search-index row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SearchRow {
    /// The document's DocType.
    pub doctype: String,
    /// The document's id.
    pub doc_id: String,
    /// The indexed text.
    pub text: String,
    /// A quantised embedding (integers, never floats).
    pub embedding: Vec<i32>,
}

/// One attachment and its metadata.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    /// The attachment record id.
    pub id: String,
    /// The DocType the attachment belongs to.
    pub doctype: String,
    /// The document it belongs to.
    pub doc_id: String,
    /// The field that references it.
    pub field: String,
    /// The original file name.
    pub filename: String,
    /// The SHA-256 the file service recorded.
    pub content_hash: String,
    /// Text the fixture carries in place of bytes; a real store reads the file service.
    pub content: String,
    /// Size in bytes.
    pub size: u64,
}

/// One cache entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CacheEntry {
    /// The cache key.
    pub key: String,
    /// The cached text.
    pub value: String,
}

/// One secret reference and its value, as the secrets service holds it.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct SecretRow {
    /// The secret's name or path.
    pub name: String,
    /// The secret itself. Never logged, never returned, stripped from the sandbox.
    pub value: String,
}

/// One integration configuration.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Integration {
    /// The integration's id.
    pub id: String,
    /// Its kind (`webhook`, `connector`, `bank`, ...).
    pub kind: String,
    /// Its endpoint (a URL or a host).
    pub endpoint: String,
    /// Its credential reference; stripped from the sandbox.
    pub credential: String,
}

impl std::fmt::Debug for SecretRow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretRow")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .finish()
    }
}

impl std::fmt::Debug for Integration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Integration")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("endpoint", &self.endpoint)
            .field("credential", &"<redacted>")
            .finish()
    }
}

/// The surfaces of a sandbox. Every read returns a copy and every replace writes the whole
/// surface; masking runs on a closed clone, so the coarse shape is intentional.
pub trait SandboxStore {
    /// Every document.
    fn documents(&self) -> Vec<Document>;

    /// Replaces every document.
    ///
    /// # Errors
    /// [`Error::Store`] when the store cannot write.
    fn replace_documents(&mut self, documents: Vec<Document>) -> Result<()>;

    /// Every journal row.
    fn journal(&self) -> Vec<JournalRow>;

    /// Replaces every journal row.
    ///
    /// # Errors
    /// [`Error::Store`] when the store cannot write.
    fn replace_journal(&mut self, rows: Vec<JournalRow>) -> Result<()>;

    /// Every audit row.
    fn audit(&self) -> Vec<AuditRow>;

    /// Replaces every audit row.
    ///
    /// # Errors
    /// [`Error::Store`] when the store cannot write.
    fn replace_audit(&mut self, rows: Vec<AuditRow>) -> Result<()>;

    /// Every search row.
    fn search(&self) -> Vec<SearchRow>;

    /// Replaces every search row.
    ///
    /// # Errors
    /// [`Error::Store`] when the store cannot write.
    fn replace_search(&mut self, rows: Vec<SearchRow>) -> Result<()>;

    /// Every attachment.
    fn attachments(&self) -> Vec<Attachment>;

    /// Replaces every attachment.
    ///
    /// # Errors
    /// [`Error::Store`] when the store cannot write.
    fn replace_attachments(&mut self, rows: Vec<Attachment>) -> Result<()>;

    /// Every cache entry.
    fn caches(&self) -> Vec<CacheEntry>;

    /// Replaces every cache entry.
    ///
    /// # Errors
    /// [`Error::Store`] when the store cannot write.
    fn replace_caches(&mut self, rows: Vec<CacheEntry>) -> Result<()>;

    /// Every secret row.
    fn secrets(&self) -> Vec<SecretRow>;

    /// Replaces every secret row.
    ///
    /// # Errors
    /// [`Error::Store`] when the store cannot write.
    fn replace_secrets(&mut self, rows: Vec<SecretRow>) -> Result<()>;

    /// Every integration.
    fn integrations(&self) -> Vec<Integration>;

    /// Replaces every integration.
    ///
    /// # Errors
    /// [`Error::Store`] when the store cannot write.
    fn replace_integrations(&mut self, rows: Vec<Integration>) -> Result<()>;

    /// The tenant this store is bound to. Required (no default): empty means "unbound", and
    /// the workflow refuses an unbound store for masking and scanning.
    fn tenant_id(&self) -> String;
}

/// An in-memory sandbox: the reference store and the fixture.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SandboxData {
    /// Documents by DocType.
    pub documents: Vec<Document>,
    /// Journal rows.
    pub journal: Vec<JournalRow>,
    /// Audit rows.
    pub audit: Vec<AuditRow>,
    /// Search-index rows.
    pub search: Vec<SearchRow>,
    /// Attachments.
    pub attachments: Vec<Attachment>,
    /// Cache entries.
    pub caches: Vec<CacheEntry>,
    /// Secret rows.
    pub secrets: Vec<SecretRow>,
    /// Integrations.
    pub integrations: Vec<Integration>,
    /// The tenant this store belongs to. Empty means "unbound"; the workflow refuses an unbound store.
    pub tenant: String,
}

impl SandboxData {
    /// An empty sandbox.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The first document of `doctype`, if any.
    #[must_use]
    pub fn document(&self, doctype: &str) -> Option<&Document> {
        self.documents.iter().find(|doc| doc.doctype == doctype)
    }

    /// The document with `id`, if any.
    #[must_use]
    pub fn by_id(&self, id: &str) -> Option<&Document> {
        self.documents.iter().find(|doc| doc.id == id)
    }

    /// Whether the store holds nothing at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
            && self.journal.is_empty()
            && self.audit.is_empty()
            && self.search.is_empty()
            && self.attachments.is_empty()
            && self.caches.is_empty()
            && self.secrets.is_empty()
            && self.integrations.is_empty()
    }
}

impl SandboxStore for SandboxData {
    fn documents(&self) -> Vec<Document> {
        self.documents.clone()
    }

    fn replace_documents(&mut self, documents: Vec<Document>) -> Result<()> {
        self.documents = documents;
        Ok(())
    }

    fn journal(&self) -> Vec<JournalRow> {
        self.journal.clone()
    }

    fn replace_journal(&mut self, rows: Vec<JournalRow>) -> Result<()> {
        self.journal = rows;
        Ok(())
    }

    fn audit(&self) -> Vec<AuditRow> {
        self.audit.clone()
    }

    fn replace_audit(&mut self, rows: Vec<AuditRow>) -> Result<()> {
        self.audit = rows;
        Ok(())
    }

    fn search(&self) -> Vec<SearchRow> {
        self.search.clone()
    }

    fn replace_search(&mut self, rows: Vec<SearchRow>) -> Result<()> {
        self.search = rows;
        Ok(())
    }

    fn attachments(&self) -> Vec<Attachment> {
        self.attachments.clone()
    }

    fn replace_attachments(&mut self, rows: Vec<Attachment>) -> Result<()> {
        self.attachments = rows;
        Ok(())
    }

    fn caches(&self) -> Vec<CacheEntry> {
        self.caches.clone()
    }

    fn replace_caches(&mut self, rows: Vec<CacheEntry>) -> Result<()> {
        self.caches = rows;
        Ok(())
    }

    fn secrets(&self) -> Vec<SecretRow> {
        self.secrets.clone()
    }

    fn replace_secrets(&mut self, rows: Vec<SecretRow>) -> Result<()> {
        self.secrets = rows;
        Ok(())
    }

    fn integrations(&self) -> Vec<Integration> {
        self.integrations.clone()
    }

    fn replace_integrations(&mut self, rows: Vec<Integration>) -> Result<()> {
        self.integrations = rows;
        Ok(())
    }

    fn tenant_id(&self) -> String {
        self.tenant.clone()
    }
}
