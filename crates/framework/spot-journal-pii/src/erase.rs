//! Erasure: destroying a subject's key, and the receipt that says what that made unreadable.
//!
//! Erasure is one key operation. What the receipt can say about *where* the subject's values
//! were depends on what the caller recorded: the platform's index of subject-to-location is
//! built by the writer (a later task), so this crate only reads it. Without an index the
//! receipt names the subject and nothing else, which is still a true and complete account of
//! the erasure itself.

use std::collections::BTreeMap;

use crate::error::PiiError;
use crate::key::SubjectKeys;

/// One position a subject's envelopes were written to, as the caller's index recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexEntry {
    entity: String,
    path: String,
    erasable: bool,
    count: u64,
}

impl IndexEntry {
    /// The entity the field belongs to.
    #[must_use]
    pub fn entity(&self) -> &str {
        &self.entity
    }

    /// The declared field path, with `*` where an array was walked.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Whether erasure reaches this field; `false` marks a legal-retention field.
    #[must_use]
    pub fn erasable(&self) -> bool {
        self.erasable
    }

    /// How many envelopes were written at this position.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }
}

/// The caller's record of where one subject's personal values were written.
///
/// The crate never builds this index: writing an image is the writer's business and the index
/// outlives any single image. An entry is (entity, field path, whether the field is erasable),
/// and [`SubjectIndex::record`] counts one envelope each time it is called, so an index can be
/// assembled by scanning the images a subject appears in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SubjectIndex {
    entries: Vec<IndexEntry>,
}

impl SubjectIndex {
    /// An empty index.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Counts one envelope at `entity`'s field `path`.
    pub fn record(&mut self, entity: &str, path: &str, erasable: bool) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| {
            entry.entity == entity && entry.path == path && entry.erasable == erasable
        }) {
            entry.count = entry.count.saturating_add(1);
            return;
        }
        self.entries.push(IndexEntry {
            entity: entity.to_owned(),
            path: path.to_owned(),
            erasable,
            count: 1,
        });
    }

    /// The recorded entries, in the order they were first recorded.
    #[must_use]
    pub fn entries(&self) -> &[IndexEntry] {
        &self.entries
    }

    /// True when nothing was recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// One field of one entity that erasure made unreadable, and how many envelopes it held.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldErasure {
    path: String,
    count: u64,
}

impl FieldErasure {
    /// The declared field path, with `*` where an array was walked.
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// How many envelopes this field held.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.count
    }
}

/// Every field of one entity that erasure made unreadable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityErasure {
    entity: String,
    fields: Vec<FieldErasure>,
}

impl EntityErasure {
    /// The entity.
    #[must_use]
    pub fn entity(&self) -> &str {
        &self.entity
    }

    /// The fields of this entity, sorted by path.
    #[must_use]
    pub fn fields(&self) -> &[FieldErasure] {
        &self.fields
    }

    /// How many envelopes of this entity erasure made unreadable.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.fields.iter().map(FieldErasure::count).sum()
    }
}

/// What an erasure made unreadable, grouped by entity and then by field.
///
/// A legal-retention field (`erasable = false`) is sealed under the tenant journal key, so it
/// is *not* in this detail: it is still readable and the receipt must not claim otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasureDetail {
    entities: Vec<EntityErasure>,
    total: u64,
}

impl ErasureDetail {
    /// The entities, sorted by name, each with its fields sorted by path.
    #[must_use]
    pub fn entities(&self) -> &[EntityErasure] {
        &self.entities
    }

    /// How many envelopes erasure made unreadable in total.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total
    }

    /// Builds the detail from an index, dropping every entry erasure does not reach.
    fn from_index(index: &SubjectIndex) -> Self {
        let mut grouped: BTreeMap<&str, BTreeMap<&str, u64>> = BTreeMap::new();
        for entry in index.entries().iter().filter(|entry| entry.erasable) {
            *grouped
                .entry(entry.entity())
                .or_default()
                .entry(entry.path())
                .or_default() += entry.count();
        }
        let mut total = 0;
        let mut entities = Vec::with_capacity(grouped.len());
        for (entity, fields) in grouped {
            let fields: Vec<FieldErasure> = fields
                .into_iter()
                .map(|(path, count)| {
                    total += count;
                    FieldErasure {
                        path: path.to_owned(),
                        count,
                    }
                })
                .collect();
            entities.push(EntityErasure {
                entity: entity.to_owned(),
                fields,
            });
        }
        Self { entities, total }
    }
}

/// The record of one erasure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErasureReceipt {
    subject: String,
    detail: Option<ErasureDetail>,
}

impl ErasureReceipt {
    /// The data subject whose key was destroyed.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Where the subject's values were, when the caller supplied an index; `None` otherwise.
    #[must_use]
    pub fn detail(&self) -> Option<&ErasureDetail> {
        self.detail.as_ref()
    }
}

/// Destroys `subject`'s key.
///
/// Every copy of that subject's data — the live image, every journal entry and every snapshot —
/// then reveals the documented placeholder, because none of them can be opened without the key.
/// A field declared `erasable = false` is sealed under the tenant journal key and still opens.
///
/// `index`, when the caller has one, turns the receipt's `detail` from `None` into the entities,
/// fields and envelope counts erasure made unreadable. Without it the receipt names the subject
/// only: the erasure is complete either way, only the account of where differs.
///
/// # Errors
///
/// `journal_pii.subject_invalid` when the subject is empty, and `journal_pii.key_store` when the
/// key store fails to destroy the key (in which case nothing is claimed as erased).
pub fn erase(
    subject: &str,
    keys: &dyn SubjectKeys,
    index: Option<&SubjectIndex>,
) -> Result<ErasureReceipt, PiiError> {
    if subject.is_empty() {
        return Err(PiiError::SubjectInvalid("the data-subject id is empty"));
    }
    keys.destroy(subject)?;
    Ok(ErasureReceipt {
        subject: subject.to_owned(),
        detail: index.map(ErasureDetail::from_index),
    })
}
