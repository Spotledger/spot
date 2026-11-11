//! What the BI views must never show: fields in a protection group, fields of secret type and
//! system audit internals. The caller supplies this model; the storage catalogue does not carry
//! protection membership, so nothing here is inferred from names.

use std::collections::{BTreeMap, BTreeSet};

use spot_permissions::GroupName;
use spot_storage_contract::{Name, schema::system};

use crate::error::{BiError, POLICY_INVALID};
use crate::plan::ExclusionReason;

/// The protected fields of a catalogue. Anything listed here is excluded from every BI view
/// whatever the policy says.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Protection {
    groups: BTreeMap<(Name, Name), String>,
    secret: BTreeSet<(Name, Name)>,
}

impl Protection {
    /// No protected fields.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Declares `entity.field` as a member of protection group `group`.
    ///
    /// # Errors
    ///
    /// `bi.policy_invalid` when the entity or field is not a valid storage name.
    pub fn group_field(
        mut self,
        entity: &str,
        field: &str,
        group: &GroupName,
    ) -> Result<Self, BiError> {
        let key = key(entity, field)?;
        self.groups.insert(key, group.as_str().to_owned());
        Ok(self)
    }

    /// Declares `entity.field` as a secret or password field.
    ///
    /// # Errors
    ///
    /// `bi.policy_invalid` when the entity or field is not a valid storage name.
    pub fn secret_field(mut self, entity: &str, field: &str) -> Result<Self, BiError> {
        let key = key(entity, field)?;
        self.secret.insert(key);
        Ok(self)
    }

    /// Why `entity.field` is protected, or `None` when it is not.
    pub(crate) fn exclusion(&self, entity: &Name, field: &Name) -> Option<ExclusionReason> {
        let key = (entity.clone(), field.clone());
        if self.secret.contains(&key) {
            return Some(ExclusionReason::Secret);
        }
        self.groups
            .get(&key)
            .map(|group| ExclusionReason::ProtectionGroup(group.clone()))
    }
}

/// Whether `field` is a system column the views never expose.
pub(crate) fn is_system_internal(field: &str) -> bool {
    field == system::PARENT_FIELD
}

fn key(entity: &str, field: &str) -> Result<(Name, Name), BiError> {
    let invalid = |text: &str| {
        BiError::new(
            POLICY_INVALID,
            format!("`{text}` is not a valid storage name"),
        )
    };
    let entity_name = Name::new(entity).map_err(|_| invalid(entity))?;
    let field_name = Name::new(field).map_err(|_| invalid(field))?;
    Ok((entity_name, field_name))
}
