//! Where the personal-field description of an entity comes from.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_journal_pii::PersonalFields;

/// The source could not answer. The reason is a fixed phrase, never a value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceError {
    /// What failed.
    pub reason: String,
}

impl SourceError {
    /// An error with `reason`.
    #[must_use]
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

/// Entity to personal-field description. The metadata layer implements it later; the declaration
/// of which fields are personal is its business (out of scope here).
///
/// `Ok(None)` means "this entity has no personal fields" and lets its images through untouched.
/// An error means the source could not say, and the transforms fail closed on it.
pub trait PersonalFieldsSource: Send + Sync {
    /// The description of `entity`'s personal fields.
    ///
    /// # Errors
    /// [`SourceError`] when the source cannot answer.
    fn fields_for(&self, entity: &str) -> Result<Option<Arc<PersonalFields>>, SourceError>;
}

/// A fixed map of descriptions, for tests and for hosts whose descriptions do not change.
#[derive(Debug, Clone, Default)]
pub struct StaticFields {
    by_entity: BTreeMap<String, Arc<PersonalFields>>,
}

impl StaticFields {
    /// An empty map: no entity has personal fields.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The same map with `fields` registered under its own entity name.
    #[must_use]
    pub fn with(mut self, fields: PersonalFields) -> Self {
        self.by_entity
            .insert(fields.entity().to_owned(), Arc::new(fields));
        self
    }
}

impl PersonalFieldsSource for StaticFields {
    fn fields_for(&self, entity: &str) -> Result<Option<Arc<PersonalFields>>, SourceError> {
        Ok(self.by_entity.get(entity).cloned())
    }
}
