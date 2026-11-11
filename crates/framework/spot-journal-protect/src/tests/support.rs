//! Shared fixtures: the personal-field description, one tenant's keys, scripted envelopes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use spot_events_contract::envelope::Position;
use spot_journal_pii::{
    CounterNonces, FieldSpec, KeyHandle, KeyStoreError, MemoryKeys, PersonalFields, SubjectKeys,
};

use crate::{
    ProtectTransform, ProtectedParts, SharedFields, SharedKeys, SharedNonces, StaticFields,
};

pub(crate) const SOURCE: &str = "acme.example";

/// The `contact` entity: an erasable email, erasable phone numbers in child rows, a legal-retention
/// tax id and a secret token.
pub(crate) fn contact_fields() -> PersonalFields {
    PersonalFields::new("contact")
        .unwrap()
        .with_field(FieldSpec::new("email", "person", "notifications", "contact_5y").unwrap())
        .with_field(
            FieldSpec::new("phones.*.number", "person", "notifications", "contact_5y").unwrap(),
        )
        .with_field(
            FieldSpec::new("tax_id", "person", "tax_filing", "tax_10y")
                .unwrap()
                .with_erasable(false),
        )
        .with_field(
            FieldSpec::new("api_token", "person", "integration", "contact_5y")
                .unwrap()
                .with_secret(true),
        )
}

pub(crate) fn fields() -> SharedFields {
    Arc::new(StaticFields::new().with(contact_fields()))
}

/// One tenant's keys. `seed` makes the tenant journal key and every subject key differ per tenant.
pub(crate) fn tenant_keys(seed: u8, subjects: &[&str]) -> Arc<MemoryKeys> {
    let keys = MemoryKeys::new(KeyHandle::new([seed; 32]));
    for (index, subject) in subjects.iter().enumerate() {
        let byte = seed.wrapping_add(u8::try_from(index + 1).unwrap());
        keys.insert(subject, KeyHandle::new([byte; 32]));
    }
    Arc::new(keys)
}

pub(crate) fn shared(keys: &Arc<MemoryKeys>) -> SharedKeys {
    Arc::<MemoryKeys>::clone(keys)
}

pub(crate) fn nonces() -> SharedNonces {
    Arc::new(Mutex::new(CounterNonces::new()))
}

pub(crate) fn transform(keys: &Arc<MemoryKeys>, nonces: &SharedNonces) -> ProtectTransform {
    ProtectTransform::new(fields(), shared(keys), Arc::clone(nonces))
}

pub(crate) fn parts(keys: &Arc<MemoryKeys>, nonces: &SharedNonces) -> ProtectedParts {
    ProtectedParts::new(fields(), shared(keys), Arc::clone(nonces))
}

/// A key store that can be switched into an outage.
pub(crate) struct FlakyKeys {
    pub(crate) inner: Arc<MemoryKeys>,
    pub(crate) down: AtomicBool,
}

impl FlakyKeys {
    pub(crate) fn new(inner: Arc<MemoryKeys>) -> Arc<Self> {
        Arc::new(Self {
            inner,
            down: AtomicBool::new(false),
        })
    }

    pub(crate) fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    fn check(&self) -> Result<(), KeyStoreError> {
        if self.down.load(Ordering::SeqCst) {
            Err(KeyStoreError::Unavailable(
                "the key store is down".to_owned(),
            ))
        } else {
            Ok(())
        }
    }
}

impl SubjectKeys for FlakyKeys {
    fn key_for(&self, subject: &str) -> Result<Option<KeyHandle>, KeyStoreError> {
        self.check()?;
        self.inner.key_for(subject)
    }
    fn destroy(&self, subject: &str) -> Result<(), KeyStoreError> {
        self.check()?;
        self.inner.destroy(subject)
    }
    fn retained_key(&self) -> Result<KeyHandle, KeyStoreError> {
        self.check()?;
        self.inner.retained_key()
    }
}

/// A record id in canonical UUID form.
pub(crate) fn uuid(n: u64) -> String {
    format!("01900000-0000-7000-8000-{n:012x}")
}

/// A contact body for `person`, with values that are easy to look for.
pub(crate) fn contact_body(person: &str, tag: &str) -> Value {
    json!({
        "person": person,
        "name": format!("Name {tag}"),
        "email": format!("{tag}@mail.example"),
        "phones": [
            {"label": "home", "number": format!("+44 20 7946 {tag}1")},
            {"label": "work", "number": format!("+44 20 7946 {tag}2")},
        ],
        "tax_id": format!("TAX-{tag}"),
        "api_token": format!("sealed:v1:{tag}"),
        "city": "Leeds",
        "company": "co-1",
    })
}

/// The personal values of `contact_body(person, tag)`: what must never appear in the journal.
pub(crate) fn clear_values(tag: &str) -> Vec<String> {
    vec![
        format!("{tag}@mail.example"),
        format!("+44 20 7946 {tag}1"),
        format!("+44 20 7946 {tag}2"),
        format!("TAX-{tag}"),
    ]
}

/// The envelope line of a state event for `entity` record `n`.
pub(crate) fn state_line(
    entity: &str,
    n: u64,
    op: &str,
    version: u64,
    body: &Value,
    seq: u64,
) -> String {
    envelope(
        "doc_state",
        entity,
        n,
        Some(version),
        json!({"op": op, "entity_schema": 1, "image": body}),
        seq,
    )
}

/// A domain event line.
pub(crate) fn domain_line(n: u64, data: &Value, seq: u64) -> String {
    envelope("contact_emailed", "contact", n, None, data.clone(), seq)
}

/// An audit event line.
pub(crate) fn audit_line(seq: u64) -> String {
    envelope(
        "sys_role_changed",
        "role",
        8_000,
        None,
        json!({"role": "r1"}),
        seq,
    )
}

pub(crate) fn envelope(
    topic: &str,
    entity: &str,
    n: u64,
    version: Option<u64>,
    data: Value,
    seq: u64,
) -> String {
    let mut value = json!({
        "event_id": uuid(1_000_000 + seq),
        "topic": topic,
        "schema_version": 1,
        "source_id": SOURCE,
        "position": {"generation": 1, "seq": seq},
        "transaction": uuid(2_000_000 + seq),
        "ordinal": 0,
        "subject": {"entity": entity, "id": uuid(n)},
        "meta": {
            "occurred_at": "2026-01-01T00:00:00Z",
            "actor": {"kind": "system", "component": "scheduler"},
            "correlation_id": uuid(3_000_000 + seq),
        },
        "data": data,
    });
    if let Some(version) = version {
        value["subject_version"] = json!(version);
    }
    value.to_string()
}

pub(crate) fn position(seq: u64) -> Position {
    Position { generation: 1, seq }
}

/// The image of a state line.
pub(crate) fn image_of(line: &[u8]) -> Value {
    let value: Value = serde_json::from_slice(line).unwrap();
    value["data"]["image"].clone()
}
