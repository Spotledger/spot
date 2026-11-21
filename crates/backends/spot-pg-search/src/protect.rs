//! Protection groups, as the search projection needs them: which fields must never enter a
//! projection.
//!
//! The storage catalogue deliberately does not carry protection membership (it is a permission
//! concept, ADR-068/ADR-070), so the layer that compiles metadata passes it in — the same shape
//! the BI views take (`spot-bi-views::Protection`). This model is intentionally small: field →
//! group, for the fields of one DocType, with a group name that must not be blank.
//!
//! Two rules use it, and both are enforced in [`ProjectionPlan::from_entity`](crate::ProjectionPlan::from_entity):
//!
//! - a searchable field in a protection group is refused, because a protected field must never
//!   influence a match or a rank (ADR-087 section 7.7 point 1);
//! - a faceted field in a protection group is refused, because a count over it leaks that its
//!   values exist (ADR-087 section 7.7 point 4).
//!
//! A caller that searches or facets a protected field through a scope is refused at read time
//! with [`permission.field_denied`](crate::FIELD_DENIED); the projection never holds the text,
//! so nothing in the index can match it either way.

use std::collections::BTreeMap;

use spot_storage_contract::{ErrorKind, Name, StorageError};

/// The protection groups of one DocType's fields.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Protection {
    groups: BTreeMap<(Name, Name), String>,
}

impl Protection {
    /// Nothing protected.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares `entity.field` a member of `group`.
    ///
    /// # Errors
    ///
    /// `InvalidName` when the group name is blank or contains control characters.
    pub fn group_field(
        mut self,
        entity: &Name,
        field: &Name,
        group: &str,
    ) -> Result<Self, StorageError> {
        let trimmed = group.trim();
        if trimmed.is_empty() || trimmed.chars().any(char::is_control) {
            return Err(StorageError::new(
                ErrorKind::InvalidName,
                format!("`{group}` is not a usable protection-group name"),
            ));
        }
        self.groups
            .insert((entity.clone(), field.clone()), trimmed.to_owned());
        Ok(self)
    }

    /// The group `entity.field` belongs to, or `None`.
    #[must_use]
    pub fn group_of(&self, entity: &Name, field: &Name) -> Option<&str> {
        self.groups
            .get(&(entity.clone(), field.clone()))
            .map(String::as_str)
    }

    /// Whether `entity.field` is in a protection group.
    #[must_use]
    pub fn is_protected(&self, entity: &Name, field: &Name) -> bool {
        self.groups.contains_key(&(entity.clone(), field.clone()))
    }

    /// How many fields are protected (for logs and tests).
    #[must_use]
    pub fn len(&self) -> usize {
        self.groups.len()
    }

    /// Whether no field is protected.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(text: &str) -> Name {
        Name::new(text).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn a_declared_field_is_protected_and_names_its_group() {
        let protection = Protection::new()
            .group_field(&name("listing"), &name("owner_note"), "owner")
            .unwrap_or_else(|e| panic!("{e}"));
        assert!(protection.is_protected(&name("listing"), &name("owner_note")));
        assert_eq!(
            protection.group_of(&name("listing"), &name("owner_note")),
            Some("owner")
        );
        assert!(!protection.is_protected(&name("listing"), &name("title")));
        assert_eq!(protection.len(), 1);
        assert!(!protection.is_empty());
    }

    #[test]
    fn a_blank_group_name_is_refused() {
        let error = match Protection::new().group_field(&name("listing"), &name("title"), "  ") {
            Ok(_) => panic!("a blank group name must be refused"),
            Err(error) => error,
        };
        assert_eq!(error.kind, ErrorKind::InvalidName);
    }
}
