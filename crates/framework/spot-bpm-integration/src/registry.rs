//! The redaction pass: credentials never reach a log, an error or the execution history.
//!
//! Every handler that opens a credential registers its value with the [`SecretRegistry`] before
//! the value can reach anything observable. The registry wraps `spot-redact`'s [`Redactor`]: it
//! knows each secret's value and its base64 and URL-encoded forms, and replaces every occurrence
//! with an inert `[redacted:<label>]` marker. [`SecretRegistry::redact_json`] walks a JSON value
//! (the node's input, its output, a response body) and [`SecretRegistry::redact_text`] cleans a
//! string (an error reason, a log line).
//!
//! Two properties matter and are tested:
//!
//! - A value the registry could not register — `spot-redact` refuses a value shorter than eight
//!   bytes, because matching it would collide with ordinary text — is **not** printed at all:
//!   [`SecretRegistry::redact_text_or_placeholder`] answers with a placeholder instead, so the
//!   guarantee holds even then.
//! - The marker is inert and redaction is idempotent (`spot-redact` guarantees both), so the
//!   history can be redacted again without changing it.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value as Json;
use spot_redact::{RedactionPolicy, Redactor};
use spot_secrets::SecretString;

use crate::credential::CredentialRef;
use crate::error::IntegrationError;

/// The label length a derived label is trimmed to before a uniqueness suffix is appended, so the
/// final label stays within `spot-redact`'s twenty-character bound.
const LABEL_BASE_LEN: usize = 14;

/// The registry of the secrets one run has opened, and the redactor that scrubs them.
pub struct SecretRegistry {
    redactor: Redactor,
    labels: BTreeMap<String, String>,
    registered: BTreeSet<String>,
}

impl SecretRegistry {
    /// A registry over the default redaction policy.
    ///
    /// # Errors
    ///
    /// [`IntegrationError::ConfigInvalid`] when the redaction policy cannot be built (a
    /// compile-time pattern that failed, which cannot happen in practice).
    pub fn new() -> Result<Self, IntegrationError> {
        let redactor = Redactor::from_policy(&RedactionPolicy::default()).map_err(|e| {
            IntegrationError::ConfigInvalid {
                node: String::from("redaction"),
                detail: format!("the redaction policy did not build: {e}"),
            }
        })?;
        Ok(Self {
            redactor,
            labels: BTreeMap::new(),
            registered: BTreeSet::new(),
        })
    }

    /// Registers a secret value under a label derived from its reference. Returns whether the
    /// value could be registered for scrubbing; a value that is too short to match safely
    /// answers `false` and is never printed.
    pub fn register(&mut self, reference: &CredentialRef, secret: &SecretString) -> bool {
        let label = self.label_for(reference);
        if self.redactor.register_secret(&label, secret).is_ok() {
            self.registered.insert(reference.name.clone());
            true
        } else {
            false
        }
    }

    /// Whether the value behind `reference` was registered for scrubbing.
    #[must_use]
    pub fn is_registered(&self, reference: &CredentialRef) -> bool {
        self.registered.contains(reference.as_str())
    }

    /// How many secrets are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.registered.len()
    }

    /// Whether no secret is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.registered.is_empty()
    }

    /// Redacts a string: every registered secret's value and encodings become an inert marker.
    #[must_use]
    pub fn redact_text(&self, text: &str) -> String {
        self.redactor.redact_str(text).into_owned()
    }

    /// Redacts a string unless the value behind `reference` could not be registered, in which
    /// case the whole string is replaced by a placeholder. Use this for anything that may embed
    /// a credential.
    #[must_use]
    pub fn redact_text_or_placeholder(&self, text: &str, reference: &CredentialRef) -> String {
        if self.is_registered(reference) {
            self.redact_text(text)
        } else {
            String::from("[redacted]")
        }
    }

    /// Redacts a JSON value in place: values of sensitive keys become `[redacted]`, and every
    /// string value is scrubbed of registered secrets.
    pub fn redact_json(&self, value: &mut Json) {
        self.redactor.redact_json(value);
    }

    /// The label a reference redacts under: lower-case `a-z0-9_`, at most twenty characters, and
    /// unique among the registry's labels.
    fn label_for(&mut self, reference: &CredentialRef) -> String {
        if let Some(label) = self.labels.get(reference.as_str()) {
            return label.clone();
        }
        let mut base: String = reference
            .as_str()
            .chars()
            .map(|c| {
                if c.is_ascii_lowercase() || c.is_ascii_digit() {
                    c
                } else {
                    '_'
                }
            })
            .take(LABEL_BASE_LEN)
            .collect();
        if base.is_empty() {
            base = String::from("secret");
        }
        let mut candidate = base.clone();
        let mut suffix = 2u32;
        while self.labels.values().any(|l| l == &candidate) {
            candidate = format!("{base}{suffix}");
            suffix = suffix.saturating_add(1);
        }
        self.labels
            .insert(reference.name.clone(), candidate.clone());
        candidate
    }
}

/// The redaction pass the brief names: scrub every known secret value out of `value` in place.
pub fn redact(value: &mut Json, registry: &SecretRegistry) {
    registry.redact_json(value);
}

/// A redacted copy of `value`.
#[must_use]
pub fn redacted(value: &Json, registry: &SecretRegistry) -> Json {
    let mut copy = value.clone();
    registry.redact_json(&mut copy);
    copy
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    const CANARY: &str = "canary-secret-value-0123456789";

    #[test]
    fn a_registered_value_is_scrubbed_from_strings_and_json() {
        let mut registry = SecretRegistry::new().unwrap();
        let reference = CredentialRef::new("sales_api");
        assert!(registry.register(&reference, &SecretString::new(CANARY)));
        let text = format!("Authorization: Bearer {CANARY}");
        let clean = registry.redact_text(&text);
        assert!(!clean.contains(CANARY), "{clean}");
        assert!(clean.contains("[redacted:"), "{clean}");

        let mut value = json!({ "token": CANARY, "note": format!("sent {CANARY}") });
        registry.redact_json(&mut value);
        assert!(!value.to_string().contains(CANARY), "{value}");
    }

    #[test]
    fn a_short_value_is_refused_and_never_printed() {
        let mut registry = SecretRegistry::new().unwrap();
        let reference = CredentialRef::new("short");
        assert!(!registry.register(&reference, &SecretString::new("hunter2")));
        let text = String::from("token=hunter2");
        assert_eq!(
            registry.redact_text_or_placeholder(&text, &reference),
            "[redacted]"
        );
    }

    #[test]
    fn labels_are_unique_and_within_the_bound() {
        let mut registry = SecretRegistry::new().unwrap();
        let a = CredentialRef::new("Sales.Api");
        let b = CredentialRef::new("sales_api");
        registry.register(&a, &SecretString::new(CANARY));
        registry.register(&b, &SecretString::new("another-secret-value-123"));
        assert_eq!(registry.len(), 2);
        let mut labels: Vec<&String> = registry.labels.values().collect();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), 2, "labels must be unique");
        for label in registry.labels.values() {
            assert!(label.len() <= 20, "{label}");
            assert!(
                label
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "{label}"
            );
        }
    }

    #[test]
    fn redaction_is_idempotent() {
        let mut registry = SecretRegistry::new().unwrap();
        let reference = CredentialRef::new("api");
        registry.register(&reference, &SecretString::new(CANARY));
        let mut value = json!({ "note": format!("x {CANARY} y") });
        registry.redact_json(&mut value);
        let first = value.clone();
        registry.redact_json(&mut value);
        assert_eq!(value, first);
    }
}
