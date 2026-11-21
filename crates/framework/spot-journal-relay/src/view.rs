//! The names-only journal projection (WP-SEC-04.T9, ADR-070 section 12 rule 6).
//!
//! `Channel::Journal` **never carries a value**: field names only. The journal itself holds whole
//! record images in sealed segments (ADR-061); that is the system's own record and is not what a
//! person or another service reads. Everything that *describes* journal content to a reader that
//! is not the system (a verify or rebuild report, a snapshot or manifest listing, a status
//! endpoint, a log line, an error) is built from a [`JournalView`], and a [`JournalView`] is built
//! only here: it holds the entity, the record id, the kind of change, the feed position and the
//! **names** of the fields the image carries that the reader may know exist. No image value is
//! ever copied into it, so no later code can print one by accident.
//!
//! # Which names
//!
//! The names are the keys of the entry's `doc_state` image (child tables as `table.column`). Each
//! is passed through [`FieldGuard::visible_names`], so a field the reader may not see is not
//! named, a table the reader may not see is not opened, and a column is judged on its own child
//! DocType. An entity the protection source does not know, or a child table it cannot resolve,
//! names nothing and the view says so ([`JournalView::names_withheld`]): fail closed.
//!
//! # Errors
//!
//! A line that is not an event envelope is refused with the fixed text of
//! `journal_relay.entry_invalid`; the parser's own message is never forwarded because it may quote
//! the line. `Debug` of a view prints what the view holds, which is names and ids only.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value as Json, json};
use spot_events_contract::envelope::{EventEnvelope, Position};
use spot_events_contract::state::{DOC_STATE_TOPIC, STATE_OPS};
use spot_field_guard::{Channel, DocProtection, FieldGuard, ProtectionSource};
use spot_journal_store::JournalStore;
use spot_permissions::{DocFacts, Principal};

use crate::error::RelayError;
use crate::relay::Relay;

/// The channel every view is built for. It carries names only; see [`Channel::carries_values`].
pub const VIEW_CHANNEL: Channel = Channel::Journal;

/// What a reader that is not the system may be told about one journal entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalView {
    entity: String,
    id: String,
    kind: String,
    position: Position,
    version: Option<u64>,
    changed: Vec<String>,
    names_withheld: bool,
}

impl JournalView {
    /// The DocType storage name of the entry's subject.
    #[must_use]
    pub fn entity(&self) -> &str {
        &self.entity
    }

    /// The record id of the subject.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// `insert`, `update` or `delete` for a document image, the topic name for any other event.
    #[must_use]
    pub fn kind(&self) -> &str {
        &self.kind
    }

    /// The feed position of the entry.
    #[must_use]
    pub fn position(&self) -> Position {
        self.position
    }

    /// The subject's version, when the event carries one.
    #[must_use]
    pub fn version(&self) -> Option<u64> {
        self.version
    }

    /// The field names (`field` or `table.column`) the image carries and the reader may know,
    /// sorted. Never a value.
    #[must_use]
    pub fn changed(&self) -> &[String] {
        &self.changed
    }

    /// Whether some names were left out because the guard could not judge them (an unknown
    /// DocType or child table). A reader may not see a name that was hidden, but is not told that
    /// either; this flag only says the guard failed closed.
    #[must_use]
    pub fn names_withheld(&self) -> bool {
        self.names_withheld
    }

    /// The view as JSON: `entity`, `id`, `kind`, `position`, `version` (when present), `changed`.
    #[must_use]
    pub fn to_json(&self) -> Json {
        let mut out = json!({
            "entity": self.entity,
            "id": self.id,
            "kind": self.kind,
            "position": { "generation": self.position.generation, "seq": self.position.seq },
            "changed": self.changed,
        });
        if let (Some(version), Some(map)) = (self.version, out.as_object_mut()) {
            map.insert("version".to_owned(), Json::from(version));
        }
        out
    }
}

/// Builds [`JournalView`]s: the single constructor every journal crate must use when it describes
/// an entry to a reader.
#[derive(Clone, Copy)]
pub struct JournalViewer<'a> {
    guard: &'a FieldGuard,
    source: &'a dyn ProtectionSource,
}

impl std::fmt::Debug for JournalViewer<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("JournalViewer")
    }
}

impl<'a> JournalViewer<'a> {
    /// A viewer over the field guard and the protection facts it answers from.
    #[must_use]
    pub fn new(guard: &'a FieldGuard, source: &'a dyn ProtectionSource) -> Self {
        Self { guard, source }
    }

    /// The view of one envelope line for `principal`.
    ///
    /// # Errors
    /// `entry_invalid` when the line is not UTF-8 or not an event envelope (a fixed message; the
    /// line is never quoted).
    pub fn view_line(&self, principal: &Principal, line: &[u8]) -> Result<JournalView, RelayError> {
        let text = std::str::from_utf8(line)
            .map_err(|_| RelayError::EntryInvalid("the entry is not an event envelope"))?;
        let envelope = EventEnvelope::from_json(text)
            .map_err(|_| RelayError::EntryInvalid("the entry is not an event envelope"))?;
        Ok(self.view(principal, &envelope))
    }

    /// The view of one envelope for `principal`.
    #[must_use]
    pub fn view(&self, principal: &Principal, envelope: &EventEnvelope) -> JournalView {
        let entity = envelope.subject.entity.clone();
        let (kind, image) = classify(envelope);
        let mut names_withheld = false;
        let changed = match image {
            None => Vec::new(),
            Some(image) => match self.source.protection(&entity) {
                None => {
                    names_withheld = true;
                    Vec::new()
                }
                Some(prot) => self.names(principal, &entity, &prot, image, &mut names_withheld),
            },
        };
        JournalView {
            entity,
            id: envelope.subject.id.to_string(),
            kind,
            position: envelope.position,
            version: envelope.subject_version,
            changed,
            names_withheld,
        }
    }

    fn facts(doctype: &str) -> DocFacts {
        DocFacts {
            doctype: doctype.to_owned(),
            ..DocFacts::default()
        }
    }

    fn visible(
        &self,
        principal: &Principal,
        doctype: &str,
        names: &[String],
        withheld: &mut bool,
    ) -> Vec<String> {
        if names.is_empty() {
            return Vec::new();
        }
        match self
            .guard
            .visible_names(principal, doctype, &Self::facts(doctype), names)
        {
            Ok(seen) => seen,
            Err(_) => {
                *withheld = true;
                Vec::new()
            }
        }
    }

    fn names(
        &self,
        principal: &Principal,
        entity: &str,
        prot: &DocProtection,
        image: &Json,
        withheld: &mut bool,
    ) -> Vec<String> {
        let Some(map) = image.as_object() else {
            return Vec::new();
        };
        let mut top: Vec<String> = Vec::new();
        let mut tables: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
        for (key, value) in map {
            top.push(key.clone());
            if prot.child_doctype(key).is_some() {
                let columns = tables.entry(key.as_str()).or_default();
                for row in value.as_array().into_iter().flatten() {
                    for column in row.as_object().into_iter().flat_map(|r| r.keys()) {
                        columns.insert(column.clone());
                    }
                }
            }
        }
        let seen_top = self.visible(principal, entity, &top, withheld);
        let mut out: BTreeSet<String> = seen_top.iter().cloned().collect();
        for (table, columns) in tables {
            if !seen_top.iter().any(|name| name == table) {
                continue;
            }
            let Some(child) = prot.child_doctype(table) else {
                continue;
            };
            let columns: Vec<String> = columns.into_iter().collect();
            for column in self.visible(principal, child, &columns, withheld) {
                out.insert(format!("{table}.{column}"));
            }
        }
        out.into_iter().collect()
    }
}

/// The kind of an entry and, for a document image, the image.
fn classify(envelope: &EventEnvelope) -> (String, Option<&Json>) {
    if envelope.topic.as_str() != DOC_STATE_TOPIC {
        return (envelope.topic.as_str().to_owned(), None);
    }
    let op = envelope
        .data
        .get("op")
        .and_then(Json::as_str)
        .filter(|op| STATE_OPS.contains(op))
        .unwrap_or("unknown");
    (op.to_owned(), envelope.data.get("image"))
}

impl<S: JournalStore> Relay<S> {
    /// The names-only views of the entries that wait in the open (unsealed) segment, in position
    /// order, as `principal` may see them. An entry that is not an envelope answers
    /// `entry_invalid` in its place. Sealed entries are read from the store by the system; a
    /// reader that is not the system sees them only through [`JournalViewer`].
    #[must_use]
    pub fn open_views(
        &self,
        viewer: &JournalViewer<'_>,
        principal: &Principal,
    ) -> Vec<Result<JournalView, RelayError>> {
        self.open
            .iter()
            .map(|(_, line)| viewer.view_line(principal, line))
            .collect()
    }
}
