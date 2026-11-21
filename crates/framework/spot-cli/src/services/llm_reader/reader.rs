//! The reader itself, over a [`Seams`] trait so the real permission engine and field guard can be
//! driven by a scripted document loader in tests, and by the site's live hub in a running server
//! ([`super::live::LiveSeams`]).

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use spot_app_api::Document;
use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::{RecordId, Value};
use spot_core::SiteId;
use spot_events_contract::{PrincipalKind, PrincipalRef};
use spot_field_guard::{Channel, DocProtection, FieldGuard};
use spot_llm::BoxFuture;
use spot_permissions::{DocFacts, Principal};
use time::OffsetDateTime;

use crate::services::llm::prompt::{
    DocumentReader, DocumentView, FieldClass, FieldValue, ReadRefusal,
};
use crate::services::sealed_fields::{ENCRYPTED_PREFIX, SEALED_PREFIX};

/// The readiness code of a site that serves no metadata yet.
pub const NOT_READY: &str = "llm_reader.not_ready";

/// The readiness code of reads that failed for lack of an answer.
pub const READS_FAILED: &str = "llm_reader.reads_failed";

/// A boxed future the seams return.
pub type Fut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// What loading a record as a user came to.
pub enum Loaded {
    /// The document, as the engine returned it for that user.
    Document(Document),
    /// Not a document for this user: missing, unreadable or an unknown DocType. The engine does
    /// not tell these apart and neither does this reader.
    Absent,
    /// The engine could not answer.
    Unavailable,
}

/// What the reader reads through: the live permission core and the document engine.
pub trait Seams: Send + Sync {
    /// The site (every invocation carries it).
    fn site(&self) -> SiteId;

    /// The business time of an invocation.
    fn now(&self) -> OffsetDateTime;

    /// A field guard over the metadata served now, or `None` when none is served.
    fn guard(&self) -> Option<Arc<FieldGuard>>;

    /// The protection facts of `doctype` in the metadata served now (the same snapshot as the
    /// guard), or `None` for an unknown DocType or nothing served.
    fn protection(&self, doctype: &str) -> Option<Arc<DocProtection>>;

    /// The principal (roles) of the user `id`, or `None` when the directory cannot answer. A
    /// user with no assignments is a principal with no roles, which then reads nothing.
    fn principal(&self, id: RecordId) -> Fut<'_, Option<Principal>>;

    /// Loads `doctype`/`id` for `inv`'s actor through the permission-checked engine.
    fn load<'a>(&'a self, inv: &'a Invocation, doctype: &'a str, id: RecordId) -> Fut<'a, Loaded>;
}

/// The engine-backed [`DocumentReader`].
pub struct EngineReader {
    seams: Arc<dyn Seams>,
    max_value_chars: usize,
    /// Reads that failed for lack of an answer, since the process started.
    unavailable_total: AtomicU64,
    /// The same, since the last readiness check.
    unavailable_since_check: AtomicU64,
}

impl EngineReader {
    /// A reader over `seams`; a field value longer than `max_value_chars` characters is cut.
    #[must_use]
    pub fn new(seams: Arc<dyn Seams>, max_value_chars: usize) -> Self {
        Self {
            seams,
            max_value_chars,
            unavailable_total: AtomicU64::new(0),
            unavailable_since_check: AtomicU64::new(0),
        }
    }

    /// How many reads have failed for lack of an answer since the process started.
    #[must_use]
    pub fn unavailable_total(&self) -> u64 {
        self.unavailable_total.load(Ordering::Relaxed)
    }

    /// The readiness answer: the site must serve metadata, and no read may have failed for lack
    /// of an answer since the last check (this call resets that count).
    ///
    /// # Errors
    ///
    /// The degraded sentence.
    pub fn health(&self) -> Result<(), String> {
        let failed = self.unavailable_since_check.swap(0, Ordering::Relaxed);
        if self.seams.guard().is_none() {
            return Err(format!(
                "{NOT_READY}: the site serves no metadata yet, so no document can be read for a prompt"
            ));
        }
        if failed > 0 {
            return Err(format!(
                "{READS_FAILED}: {failed} read(s) failed since the last check because the document engine or the principal directory could not answer"
            ));
        }
        Ok(())
    }

    fn unavailable(&self) -> ReadRefusal {
        self.unavailable_total.fetch_add(1, Ordering::Relaxed);
        self.unavailable_since_check.fetch_add(1, Ordering::Relaxed);
        ReadRefusal::Unavailable
    }

    /// The reader's whole procedure for one document.
    async fn read_one(
        &self,
        user: &str,
        doctype: &str,
        name: &str,
    ) -> Result<DocumentView, ReadRefusal> {
        // Only a signed-in user (or an agent acting as one) reads: `user:<id>`. A consumer
        // principal, the platform and every other spelling are refused.
        let Ok(principal_ref) = user.parse::<PrincipalRef>() else {
            return Err(ReadRefusal::Denied);
        };
        if principal_ref.kind() != PrincipalKind::User {
            return Err(ReadRefusal::Denied);
        }
        let user_id = principal_ref.id();
        // The record name is its id; anything else is not a document of this site.
        let Some(record) = spot_http::api::codec::parse_record_id(name) else {
            return Err(ReadRefusal::Denied);
        };
        // No metadata served, or an unknown DocType: nothing is known, nothing is released.
        let (Some(guard), Some(protection)) = (self.seams.guard(), self.seams.protection(doctype))
        else {
            return Err(ReadRefusal::Denied);
        };
        let Some(principal) = self.seams.principal(user_id).await else {
            return Err(self.unavailable());
        };

        let invocation = Invocation {
            site: self.seams.site(),
            actor: Actor::User {
                id: user_id,
                via: None,
            },
            command_id: None,
            language: None,
            now: self.seams.now(),
        };
        let mut document = match self.seams.load(&invocation, doctype, record).await {
            Loaded::Document(document) => document,
            Loaded::Absent => return Err(ReadRefusal::Denied),
            Loaded::Unavailable => return Err(self.unavailable()),
        };
        if document.doctype().as_str() != doctype {
            return Err(ReadRefusal::Denied);
        }

        // Field-level permissions, on the channel retrieval uses. A hidden field is removed; a
        // masked one comes back changed, and a changed value is not what the user may read, so
        // it is dropped as well.
        let before = document.fields().clone();
        let facts = DocFacts {
            doctype: doctype.to_owned(),
            ..DocFacts::default()
        };
        if guard
            .redact(Channel::Search, &principal, doctype, &facts, &mut document)
            .is_err()
        {
            // The guard empties the document on an error; nothing is released.
            return Err(ReadRefusal::Denied);
        }

        let mut fields = Vec::new();
        for (field, original) in &before {
            if document.fields().get(field) != Some(original) {
                continue;
            }
            let Some(text) = text_of(original, self.max_value_chars) else {
                continue;
            };
            fields.push(FieldValue {
                name: field.as_str().to_owned(),
                class: class_of(&protection, field.as_str(), original),
                value: text,
            });
        }
        Ok(DocumentView {
            doctype: doctype.to_owned(),
            name: name.to_owned(),
            fields,
        })
    }
}

impl DocumentReader for EngineReader {
    fn read<'a>(
        &'a self,
        user: &'a str,
        doctype: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<DocumentView, ReadRefusal>> {
        Box::pin(self.read_one(user, doctype, name))
    }
}

/// What a field is to the prompt: from the served metadata, honestly.
fn class_of(protection: &DocProtection, field: &str, value: &Value) -> FieldClass {
    let facts = protection.field(field);
    let sealed_value = matches!(
        value,
        Value::Text(text) if text.starts_with(SEALED_PREFIX) || text.starts_with(ENCRYPTED_PREFIX)
    );
    if sealed_value || facts.tags.iter().any(|tag| tag.0 == "secret") {
        FieldClass::Sealed
    } else if !facts.groups.is_empty() {
        FieldClass::Protected
    } else if facts.tags.iter().any(|tag| tag.0 == "pii") {
        FieldClass::Personal
    } else {
        FieldClass::Plain
    }
}

/// A value as prompt text, or `None` for a null and for kinds with no text form (binary, geo,
/// range). A text longer than `max` characters is cut with an ellipsis.
fn text_of(value: &Value, max: usize) -> Option<String> {
    let text = match value {
        Value::Bool(v) => v.to_string(),
        Value::Int(v) => v.to_string(),
        Value::Decimal(v) => v.to_string(),
        Value::Text(v) | Value::Json(v) => v.clone(),
        Value::Date(v) => v.to_string(),
        Value::Time(v) => v.to_string(),
        Value::DateTime(v) => v.to_string(),
        Value::Id(v) => v.to_string(),
        _ => return None,
    };
    if text.chars().count() > max {
        let mut cut: String = text.chars().take(max).collect();
        cut.push('\u{2026}');
        Some(cut)
    } else {
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_have_a_text_form_unless_binary_or_null() {
        assert_eq!(text_of(&Value::Int(7), 10).as_deref(), Some("7"));
        assert_eq!(text_of(&Value::Bool(true), 10).as_deref(), Some("true"));
        assert_eq!(
            text_of(&Value::Text("abc".into()), 10).as_deref(),
            Some("abc")
        );
        assert!(text_of(&Value::Null, 10).is_none());
        assert!(text_of(&Value::Bytes(vec![1, 2]), 10).is_none());
    }

    #[test]
    fn a_long_value_is_cut_with_an_ellipsis() {
        let cut = text_of(&Value::Text("abcdefghij".into()), 4).unwrap_or_default();
        assert_eq!(cut, "abcd\u{2026}");
        assert_eq!(
            text_of(&Value::Text("abcd".into()), 4).as_deref(),
            Some("abcd")
        );
    }

    #[test]
    fn classes_follow_the_facts_and_the_value() {
        let protection = DocProtection {
            name: "t".into(),
            fields: vec![
                spot_permissions::FieldFacts {
                    path: "card".into(),
                    groups: vec![spot_permissions::GroupName("pci".into())],
                    tags: Vec::new(),
                    mask: None,
                },
                spot_permissions::FieldFacts {
                    path: "email".into(),
                    groups: Vec::new(),
                    tags: vec![spot_permissions::Tag("pii".into())],
                    mask: None,
                },
                spot_permissions::FieldFacts {
                    path: "token".into(),
                    groups: Vec::new(),
                    tags: vec![spot_permissions::Tag("secret".into())],
                    mask: None,
                },
            ],
            tables: Default::default(),
            trace_groups: Vec::new(),
        };
        let text = |s: &str| Value::Text(s.into());
        assert_eq!(
            class_of(&protection, "subject", &text("x")),
            FieldClass::Plain
        );
        assert_eq!(
            class_of(&protection, "card", &text("x")),
            FieldClass::Protected
        );
        assert_eq!(
            class_of(&protection, "email", &text("x")),
            FieldClass::Personal
        );
        assert_eq!(
            class_of(&protection, "token", &text("x")),
            FieldClass::Sealed
        );
        // A plain field holding a sealed envelope is sealed whatever the metadata says.
        let envelope = text(&format!("{SEALED_PREFIX}abc"));
        assert_eq!(
            class_of(&protection, "subject", &envelope),
            FieldClass::Sealed
        );
    }
}
