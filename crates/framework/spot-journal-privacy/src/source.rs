//! The personal-field source the shipper reads per batch, replaceable at runtime.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};

use spot_journal_pii::PersonalFields;
use spot_journal_protect::{PersonalFieldsSource, SourceError, StaticFields};

use crate::decl::{Declared, FieldDecl, build};
use crate::error::PrivacyError;

struct State {
    fields: StaticFields,
    declared: Vec<Declared>,
}

/// The declared personal fields of a site. [`CatalogueFields::replace`] swaps the whole set
/// atomically (a metadata swap), so a field flagged personal at runtime is protected from the
/// next shipped batch on: the shipper asks [`PersonalFieldsSource::fields_for`] per line.
pub struct CatalogueFields {
    state: RwLock<Arc<State>>,
}

fn read(lock: &RwLock<Arc<State>>) -> RwLockReadGuard<'_, Arc<State>> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write(lock: &RwLock<Arc<State>>) -> RwLockWriteGuard<'_, Arc<State>> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl CatalogueFields {
    /// The source for `decls`; `forbidden` are the retention classes that forbid erasing.
    ///
    /// # Errors
    /// As [`build`]: a field without a subject refuses the whole set.
    pub fn new(
        decls: &[FieldDecl],
        forbidden: &BTreeSet<String>,
    ) -> Result<Arc<Self>, PrivacyError> {
        Ok(Arc::new(Self {
            state: RwLock::new(Arc::new(state_of(decls, forbidden)?)),
        }))
    }

    /// Replaces every declaration. Nothing changes when the new set is refused.
    ///
    /// # Errors
    /// As [`build`].
    pub fn replace(
        &self,
        decls: &[FieldDecl],
        forbidden: &BTreeSet<String>,
    ) -> Result<(), PrivacyError> {
        let state = Arc::new(state_of(decls, forbidden)?);
        *write(&self.state) = state;
        Ok(())
    }

    /// Whether any personal field is declared.
    #[must_use]
    pub fn has_personal_fields(&self) -> bool {
        !read(&self.state).declared.is_empty()
    }

    /// What is declared, with whether erasure reaches each field.
    #[must_use]
    pub fn declared(&self) -> Vec<Declared> {
        read(&self.state).declared.clone()
    }
}

fn state_of(decls: &[FieldDecl], forbidden: &BTreeSet<String>) -> Result<State, PrivacyError> {
    let built = build(decls, forbidden)?;
    let mut fields = StaticFields::new();
    for entity in built.entities {
        fields = fields.with(entity);
    }
    Ok(State {
        fields,
        declared: built.declared,
    })
}

impl PersonalFieldsSource for CatalogueFields {
    fn fields_for(&self, entity: &str) -> Result<Option<Arc<PersonalFields>>, SourceError> {
        let state = Arc::clone(&read(&self.state));
        state.fields.fields_for(entity)
    }
}
