//! Building a prompt without leaking what must not leave: the permission-checked read as the
//! invoking user, the stripping of protected and sealed fields, the redaction of what remains,
//! and the manifest of what went in.
//!
//! # The rule
//!
//! A document enters a prompt only through a [`DocumentReader`], which reads **as the invoking
//! user** (the engine's permission-checked read; this module never reads storage itself). The
//! reader labels every field with a [`FieldClass`]. The builder then:
//!
//! 1. drops every `Protected` field (a protection group such as `confidential`) and every
//!    `Sealed` one (a secret field), and `Personal` fields too unless the policy includes them;
//! 2. runs the remaining text, and all free text the caller adds, through the [`Redactor`] (keys,
//!    tokens, card numbers, connection strings);
//! 3. records in the [`Manifest`] which DocTypes were included and how many fields of each class
//!    were included or stripped, never a value.
//!
//! A reader that fails, or refuses, adds nothing: the document is not in the prompt and the
//! caller learns that with a fixed code. With no reader wired the host uses [`DenyAllReader`], so
//! a document cannot enter a prompt until the engine-backed reader is installed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use spot_llm::{BoxFuture, Message, Request};
use spot_redact::Redactor;

/// What a field is, to the prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum FieldClass {
    /// An ordinary field.
    Plain,
    /// A personal-data field (names, contact details): stripped unless the policy includes it.
    Personal,
    /// A field in a protection group: always stripped.
    Protected,
    /// A secret (sealed) field: always stripped.
    Sealed,
}

impl FieldClass {
    /// The stable name a manifest uses.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FieldClass::Plain => "plain",
            FieldClass::Personal => "personal",
            FieldClass::Protected => "protected",
            FieldClass::Sealed => "sealed",
        }
    }
}

/// One field a reader returns.
#[derive(Clone, PartialEq, Eq)]
pub struct FieldValue {
    /// The field name.
    pub name: String,
    /// What the field is.
    pub class: FieldClass,
    /// The value as text.
    pub value: String,
}

impl std::fmt::Debug for FieldValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The value never prints.
        f.debug_struct("FieldValue")
            .field("name", &self.name)
            .field("class", &self.class)
            .finish_non_exhaustive()
    }
}

/// One document as the invoking user may see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentView {
    /// The DocType.
    pub doctype: String,
    /// The record name.
    pub name: String,
    /// The fields the user may read, each labelled.
    pub fields: Vec<FieldValue>,
}

/// Why a reader gave nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadRefusal {
    /// The user may not read the document (or it does not exist: the two are not told apart).
    Denied,
    /// The read failed; nothing is known.
    Unavailable,
}

/// The permission-checked read, as the invoking user. The engine-backed implementation belongs to
/// the host that wires the agent runtime; this crate defines the seam and refuses by default.
pub trait DocumentReader: Send + Sync {
    /// Reads `doctype`/`name` as `user`: the read the user could make through the API, with its
    /// field-level permissions applied.
    fn read<'a>(
        &'a self,
        user: &'a str,
        doctype: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, Result<DocumentView, ReadRefusal>>;
}

/// The reader of a host with no engine-backed reader: every read is denied.
#[derive(Clone, Copy, Debug, Default)]
pub struct DenyAllReader;

impl DocumentReader for DenyAllReader {
    fn read<'a>(
        &'a self,
        _user: &'a str,
        _doctype: &'a str,
        _name: &'a str,
    ) -> BoxFuture<'a, Result<DocumentView, ReadRefusal>> {
        Box::pin(async { Err(ReadRefusal::Denied) })
    }
}

/// What a prompt holds, as counts and names: the field classes and DocTypes, never a value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Manifest {
    /// The DocTypes at least one field of which was included.
    pub doctypes: BTreeSet<String>,
    /// Included fields by class name, and stripped fields as `stripped_<class>`.
    pub classes: BTreeMap<String, u64>,
}

impl Manifest {
    fn bump(&mut self, key: String) {
        let slot = self.classes.entry(key).or_insert(0);
        *slot = slot.saturating_add(1);
    }
}

/// What the policy lets into a prompt beyond plain fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PromptPolicy {
    /// Include `Personal` fields. Off by default.
    pub include_personal: bool,
}

/// Why adding a document failed. The text is fixed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PromptError(pub &'static str);

impl std::fmt::Display for PromptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for PromptError {}

/// Builds one user message from free text and documents read as the invoking user.
pub struct PromptBuilder {
    redactor: Arc<Redactor>,
    reader: Arc<dyn DocumentReader>,
    user: String,
    policy: PromptPolicy,
    system: Option<String>,
    sections: Vec<String>,
    manifest: Manifest,
}

impl PromptBuilder {
    /// A builder for `user`, reading through `reader` and redacting with `redactor`.
    #[must_use]
    pub fn new(
        redactor: Arc<Redactor>,
        reader: Arc<dyn DocumentReader>,
        user: impl Into<String>,
        policy: PromptPolicy,
    ) -> Self {
        Self {
            redactor,
            reader,
            user: user.into(),
            policy,
            system: None,
            sections: Vec::new(),
            manifest: Manifest::default(),
        }
    }

    /// Sets the system prompt (redacted).
    #[must_use]
    pub fn system(mut self, text: &str) -> Self {
        self.system = Some(self.redactor.redact_str(text).into_owned());
        self
    }

    /// Adds free text (redacted).
    #[must_use]
    pub fn text(mut self, text: &str) -> Self {
        self.sections
            .push(self.redactor.redact_str(text).into_owned());
        self
    }

    /// Reads `doctype`/`name` as the invoking user and adds its permitted fields.
    ///
    /// # Errors
    ///
    /// [`PromptError`] when the reader denies the read or fails: nothing of the document is added.
    pub async fn document(mut self, doctype: &str, name: &str) -> Result<Self, PromptError> {
        let view = match self.reader.read(&self.user, doctype, name).await {
            Ok(view) => view,
            Err(ReadRefusal::Denied) => {
                return Err(PromptError("the invoking user may not read the document"));
            }
            Err(ReadRefusal::Unavailable) => {
                return Err(PromptError("the document could not be read"));
            }
        };
        let mut lines: Vec<String> = Vec::new();
        for field in &view.fields {
            let include = match field.class {
                FieldClass::Plain => true,
                FieldClass::Personal => self.policy.include_personal,
                FieldClass::Protected | FieldClass::Sealed => false,
            };
            if include {
                self.manifest.bump(field.class.as_str().to_owned());
                self.manifest.doctypes.insert(view.doctype.clone());
                let value = self.redactor.redact_str(&field.value);
                lines.push(format!("{}: {}", field.name, value));
            } else {
                self.manifest
                    .bump(format!("stripped_{}", field.class.as_str()));
            }
        }
        if !lines.is_empty() {
            self.sections.push(format!(
                "[{} {}]\n{}",
                view.doctype,
                view.name,
                lines.join("\n")
            ));
        }
        Ok(self)
    }

    /// The request for `model` with id `request_id` (the gateway rewrites the id) and the
    /// manifest of what went in.
    #[must_use]
    pub fn build(self, model: &str, request_id: &str) -> (Request, Manifest) {
        let mut request = Request::new(model, request_id);
        if let Some(system) = self.system {
            request = request.with_system(system);
        }
        if !self.sections.is_empty() {
            request = request.with_message(Message::user(self.sections.join("\n\n")));
        }
        (request, self.manifest)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_redact::RedactionPolicy;

    struct Fixed(DocumentView);

    impl DocumentReader for Fixed {
        fn read<'a>(
            &'a self,
            _user: &'a str,
            _doctype: &'a str,
            _name: &'a str,
        ) -> BoxFuture<'a, Result<DocumentView, ReadRefusal>> {
            Box::pin(async move { Ok(self.0.clone()) })
        }
    }

    fn redactor() -> Arc<Redactor> {
        Arc::new(Redactor::from_policy(&RedactionPolicy::default()).unwrap())
    }

    fn view() -> DocumentView {
        let field = |name: &str, class, value: &str| FieldValue {
            name: name.into(),
            class,
            value: value.into(),
        };
        DocumentView {
            doctype: "Task".into(),
            name: "T-1".into(),
            fields: vec![
                field("subject", FieldClass::Plain, "fix the printer"),
                field("details", FieldClass::Protected, "PROTECTED-VALUE"),
                field("api_secret", FieldClass::Sealed, "SEALED-VALUE"),
                field("owner", FieldClass::Personal, "PERSONAL-VALUE"),
            ],
        }
    }

    #[tokio::test]
    async fn protected_sealed_and_personal_fields_never_enter_the_prompt() {
        let builder = PromptBuilder::new(
            redactor(),
            Arc::new(Fixed(view())),
            "u1",
            PromptPolicy::default(),
        )
        .document("Task", "T-1")
        .await
        .unwrap();
        let (request, manifest) = builder.build("m", "r");
        let text = serde_json::to_string(&request).unwrap();
        assert!(text.contains("fix the printer"));
        for hidden in ["PROTECTED-VALUE", "SEALED-VALUE", "PERSONAL-VALUE"] {
            assert!(!text.contains(hidden), "{hidden} leaked");
        }
        assert_eq!(manifest.classes.get("plain"), Some(&1));
        assert_eq!(manifest.classes.get("stripped_protected"), Some(&1));
        assert_eq!(manifest.classes.get("stripped_sealed"), Some(&1));
        assert_eq!(manifest.classes.get("stripped_personal"), Some(&1));
        assert!(manifest.doctypes.contains("Task"));
    }

    #[tokio::test]
    async fn personal_fields_enter_only_when_the_policy_says_so() {
        let builder = PromptBuilder::new(
            redactor(),
            Arc::new(Fixed(view())),
            "u1",
            PromptPolicy {
                include_personal: true,
            },
        )
        .document("Task", "T-1")
        .await
        .unwrap();
        let (request, manifest) = builder.build("m", "r");
        assert!(
            serde_json::to_string(&request)
                .unwrap()
                .contains("PERSONAL-VALUE")
        );
        assert_eq!(manifest.classes.get("personal"), Some(&1));
    }

    #[tokio::test]
    async fn a_denied_read_adds_nothing() {
        let result = PromptBuilder::new(
            redactor(),
            Arc::new(DenyAllReader),
            "u1",
            PromptPolicy::default(),
        )
        .document("Task", "T-1")
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn free_text_is_redacted() {
        let (request, _) = PromptBuilder::new(
            redactor(),
            Arc::new(DenyAllReader),
            "u1",
            PromptPolicy::default(),
        )
        .text("use Bearer abcdefghijklmnop1234 to call")
        .build("m", "r");
        let text = serde_json::to_string(&request).unwrap();
        assert!(!text.contains("abcdefghijklmnop1234"));
    }
}
