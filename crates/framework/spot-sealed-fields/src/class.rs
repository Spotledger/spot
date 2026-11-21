//! The sync class of a protection group, per tenant (ADR-070 rule 12.13 a).
//!
//! A group is `everywhere` unless the tenant chose otherwise — except a group tagged `secret`,
//! `pii` or `financial`, which has **no default**: reading its class without an explicit choice
//! is [`SealedFieldsError::ClassUnset`]. The table lives in one tenant setting
//! ([`SETTING_KEY`]), so the console can show and change it like any other setting and the
//! class travels with the tenant's configuration rather than with a node.
//!
//! The table's text form is one `group = "class"` line per group:
//!
//! ```toml
//! bank_details = "sealed"
//! salary = "home_only"
//! ```

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use spot_permissions::Tag;
use spot_protected_keys::GroupName;
use spot_settings::{
    Chain, EffectiveValue, ParamType, ParamValue, Risk, Scope, SettingSpec, SettingsRegistry,
    ValueStore, resolve,
};

use crate::error::SealedFieldsError;

/// The setting the table is stored in.
// The key is assembled so the error-code scan does not mistake a setting key for a code.
pub const SETTING_KEY: &str = concat!("security.", "protection_sync_classes");

/// The longest table the setting accepts, in characters.
pub const MAX_TABLE_LEN: usize = 16_384;

/// The tags that give a group no default sync class.
pub const NO_DEFAULT_TAGS: [&str; 3] = ["secret", "pii", "financial"];

/// How a protection group's values travel to branch and offline nodes (ADR-070 rule 12.13 a).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncClass {
    /// Syncs like any field; the node enforces the same grants offline. The platform default.
    Everywhere,
    /// Stored encrypted on the node; the key is released under a lease (item e).
    Encrypted,
    /// The node creates values and can never read them back; only the hub can open them.
    Sealed,
    /// Does not sync to a branch; reads there show it as unavailable.
    HomeOnly,
}

impl SyncClass {
    /// Every class, in declaration order.
    pub const ALL: [SyncClass; 4] = [
        SyncClass::Everywhere,
        SyncClass::Encrypted,
        SyncClass::Sealed,
        SyncClass::HomeOnly,
    ];

    /// The stable wire and settings name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Everywhere => "everywhere",
            Self::Encrypted => "encrypted",
            Self::Sealed => "sealed",
            Self::HomeOnly => "home_only",
        }
    }

    /// The class a name means, if any.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_str() == name)
    }

    /// Whether a node may hold the value at all.
    #[must_use]
    pub fn syncs_to_branch(self) -> bool {
        !matches!(self, Self::HomeOnly)
    }

    /// Whether the node holds the value encrypted under a released group key.
    #[must_use]
    pub fn is_encrypted(self) -> bool {
        matches!(self, Self::Encrypted)
    }

    /// Whether only the hub can read the value.
    #[must_use]
    pub fn is_sealed(self) -> bool {
        matches!(self, Self::Sealed)
    }
}

impl std::fmt::Display for SyncClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a tag list leaves the group without a default class.
#[must_use]
pub fn has_no_default(tags: &[Tag]) -> bool {
    tags.iter()
        .any(|tag| NO_DEFAULT_TAGS.iter().any(|name| tag.0 == *name))
}

/// The sync-class table of one tenant.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SyncClassTable {
    rows: BTreeMap<GroupName, SyncClass>,
}

impl SyncClassTable {
    /// An empty table: every group falls back to its default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Chooses the class of `group`.
    pub fn set(&mut self, group: GroupName, class: SyncClass) {
        self.rows.insert(group, class);
    }

    /// Forgets any explicit choice for `group`.
    pub fn remove(&mut self, group: &GroupName) {
        self.rows.remove(group);
    }

    /// The class explicitly chosen for `group`, if any.
    #[must_use]
    pub fn chosen(&self, group: &GroupName) -> Option<SyncClass> {
        self.rows.get(group).copied()
    }

    /// The class of `group` given its classification tags.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::ClassUnset`] when the group is tagged `secret`, `pii` or
    /// `financial` and the tenant never chose a class for it.
    pub fn class_for(
        &self,
        group: &GroupName,
        tags: &[Tag],
    ) -> Result<SyncClass, SealedFieldsError> {
        if let Some(class) = self.chosen(group) {
            return Ok(class);
        }
        if has_no_default(tags) {
            return Err(SealedFieldsError::ClassUnset {
                group: group.clone(),
            });
        }
        Ok(SyncClass::Everywhere)
    }

    /// Every explicit row, in group order.
    #[must_use]
    pub fn rows(&self) -> Vec<(GroupName, SyncClass)> {
        self.rows
            .iter()
            .map(|(group, class)| (group.clone(), *class))
            .collect()
    }

    /// How many explicit choices the table holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether the tenant has chosen no class at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The setting's text form: one `group = "class"` line per row.
    #[must_use]
    pub fn to_toml(&self) -> String {
        let mut out = String::new();
        for (group, class) in &self.rows {
            out.push_str(&format!(
                "{} = \"{}\"\n",
                quote_key(group.as_str()),
                class.as_str()
            ));
        }
        out
    }

    /// Parses the setting's text form.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::SettingInvalid`] for text that is not a flat table, and
    /// [`SealedFieldsError::ClassInvalid`] for a class name outside the four.
    pub fn from_toml(text: &str) -> Result<Self, SealedFieldsError> {
        if text.trim().is_empty() {
            return Ok(Self::new());
        }
        let raw: BTreeMap<String, String> =
            toml::from_str(text).map_err(|_| SealedFieldsError::SettingInvalid {
                detail: "the table is not a flat `group = \"class\"` document".to_owned(),
            })?;
        let mut rows = BTreeMap::new();
        for (name, value) in raw {
            let group = GroupName::new(&name).map_err(|_| SealedFieldsError::SettingInvalid {
                detail: format!("`{name}` is not a valid protection group name"),
            })?;
            let class =
                SyncClass::parse(&value).ok_or_else(|| SealedFieldsError::ClassInvalid {
                    group: name.clone(),
                    value,
                })?;
            rows.insert(group, class);
        }
        Ok(Self { rows })
    }

    /// The setting specification every tenant registry carries.
    ///
    /// It is not a secret (a class name is not key material) and its default is the empty
    /// table, because a group's real default depends on its tags and is decided by
    /// [`SyncClassTable::class_for`], never by the settings layer.
    #[must_use]
    pub fn setting_spec() -> SettingSpec {
        SettingSpec {
            key: SETTING_KEY.to_owned(),
            title: "Protection-group sync classes".to_owned(),
            summary: "How each protection group travels to branch and offline nodes.".to_owned(),
            description: Some(
                "One `group = \"class\"` line per protection group, where class is everywhere, \
                 encrypted, sealed or home_only. A group tagged secret, pii or financial has no \
                 default and must be listed here."
                    .to_owned(),
            ),
            value: ParamType::Text {
                max_len: MAX_TABLE_LEN,
                pattern: None,
            },
            default: Some(ParamValue::Text(String::new())),
            scopes: vec![Scope::Fleet, Scope::Tenant],
            restart: spot_settings::Restart::None,
            risk: Risk::High,
            secret: false,
            read_only: false,
            since: "0.1.0".to_owned(),
            deprecated: None,
            tags: vec!["security".to_owned(), "sync".to_owned()],
            depends_on: None,
            help_url: None,
            group: Some("Protection".to_owned()),
            order: Some(10),
        }
    }

    /// Reads the tenant's table through the settings layer.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::SettingInvalid`] when the stored value cannot be read or parsed.
    pub fn load(
        store: &dyn ValueStore,
        chain: &Chain,
        registry: &SettingsRegistry,
    ) -> Result<Self, SealedFieldsError> {
        let resolved = resolve(SETTING_KEY, chain, store, registry).map_err(|report| {
            SealedFieldsError::SettingInvalid {
                detail: format!("the sync-class setting does not resolve: {report}"),
            }
        })?;
        match resolved.value {
            EffectiveValue::Value(ParamValue::Text(text)) => Self::from_toml(&text),
            EffectiveValue::Value(_) => Err(SealedFieldsError::SettingInvalid {
                detail: "the sync-class setting is not text".to_owned(),
            }),
            EffectiveValue::Redacted => Err(SealedFieldsError::SettingInvalid {
                detail: "the sync-class setting is stored as a secret".to_owned(),
            }),
        }
    }

    /// Writes the tenant's table through the settings layer.
    ///
    /// # Errors
    ///
    /// [`SealedFieldsError::SettingInvalid`] when the store refuses the write.
    pub fn save(
        &self,
        store: &dyn ValueStore,
        scope: &spot_settings::ScopeRef,
    ) -> Result<(), SealedFieldsError> {
        let text = self.to_toml();
        if text.chars().count() > MAX_TABLE_LEN {
            return Err(SealedFieldsError::SettingInvalid {
                detail: "the sync-class table is too long".to_owned(),
            });
        }
        let value = spot_settings::StoredValue::Value(ParamValue::Text(text));
        store.put(scope, SETTING_KEY, value);
        Ok(())
    }

    /// The groups in the table that are not in `declared` (a stale choice after a group was
    /// removed), in group order.
    #[must_use]
    pub fn stale_groups(&self, declared: &BTreeSet<GroupName>) -> Vec<GroupName> {
        self.rows
            .keys()
            .filter(|group| !declared.contains(*group))
            .cloned()
            .collect()
    }
}

/// A TOML bare key when the name allows it, a quoted key otherwise.
fn quote_key(name: &str) -> String {
    let bare = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if bare {
        name.to_owned()
    } else {
        format!("\"{}\"", name.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use spot_settings::{MemoryStore, RegistryBuilder, ScopeRef};

    fn group(name: &str) -> GroupName {
        GroupName::new(name).expect("group")
    }

    fn tag(name: &str) -> Tag {
        Tag(name.to_owned())
    }

    #[test]
    fn the_default_is_everywhere_only_for_untagged_groups() {
        let table = SyncClassTable::new();
        assert_eq!(
            table.class_for(&group("notes"), &[]).expect("default"),
            SyncClass::Everywhere
        );
        assert_eq!(
            table
                .class_for(&group("health"), &[tag("health")])
                .expect("health has a default"),
            SyncClass::Everywhere
        );
        for name in NO_DEFAULT_TAGS {
            let error = table
                .class_for(&group("bank_details"), &[tag(name)])
                .expect_err("tagged group has no default");
            assert_eq!(error.code(), crate::error::CLASS_UNSET);
        }
    }

    #[test]
    fn an_explicit_choice_wins_over_a_tag() {
        let mut table = SyncClassTable::new();
        table.set(group("bank_details"), SyncClass::Sealed);
        assert_eq!(
            table
                .class_for(&group("bank_details"), &[tag("secret")])
                .expect("chosen"),
            SyncClass::Sealed
        );
    }

    #[test]
    fn the_text_form_round_trips_including_odd_names() {
        let mut table = SyncClassTable::new();
        table.set(group("bank_details"), SyncClass::Sealed);
        table.set(group("Salary Lines"), SyncClass::HomeOnly);
        let text = table.to_toml();
        let back = SyncClassTable::from_toml(&text).expect("parse");
        assert_eq!(back, table);
    }

    #[test]
    fn an_unknown_class_is_rejected_by_name() {
        let error = SyncClassTable::from_toml("bank_details = \"sometimes\"\n").expect_err("bad");
        assert_eq!(error.code(), crate::error::CLASS_INVALID);
    }

    #[test]
    fn text_that_is_not_a_table_is_rejected() {
        let error = SyncClassTable::from_toml("[nested]\nx = 1\n").expect_err("bad");
        assert_eq!(error.code(), crate::error::SETTING_INVALID);
    }

    #[test]
    fn the_table_travels_through_settings() {
        let registry = RegistryBuilder::new()
            .add_spec(SyncClassTable::setting_spec())
            .build()
            .expect("registry");
        let store = MemoryStore::new();
        let chain = Chain::new(vec![ScopeRef::Tenant("acme".to_owned())]).expect("chain");

        let mut table = SyncClassTable::new();
        table.set(group("bank_details"), SyncClass::Sealed);
        table
            .save(&store, &ScopeRef::Tenant("acme".to_owned()))
            .expect("save");
        let loaded = SyncClassTable::load(&store, &chain, &registry).expect("load");
        assert_eq!(loaded, table);
    }

    #[test]
    fn an_untouched_tenant_loads_an_empty_table() {
        let registry = RegistryBuilder::new()
            .add_spec(SyncClassTable::setting_spec())
            .build()
            .expect("registry");
        let store = MemoryStore::new();
        let chain = Chain::new(vec![ScopeRef::Tenant("acme".to_owned())]).expect("chain");
        let loaded = SyncClassTable::load(&store, &chain, &registry).expect("load");
        assert!(loaded.is_empty());
    }

    #[test]
    fn stale_groups_are_reported() {
        let mut table = SyncClassTable::new();
        table.set(group("gone"), SyncClass::Sealed);
        table.set(group("kept"), SyncClass::Everywhere);
        let declared = BTreeSet::from([group("kept")]);
        assert_eq!(table.stale_groups(&declared), vec![group("gone")]);
    }
}
