//! The value store trait, the batch types, and the in-memory store.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::error::SettingsError;
use crate::model::{ScopeRef, StoredValue};

/// One write in a batch: set `value` at `scope` for `key`, or remove it when `value` is
/// `None`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SettingWrite {
    /// The scope to write at.
    pub scope: ScopeRef,
    /// The setting key.
    pub key: String,
    /// The value to set, or `None` to remove any value at this scope.
    pub value: Option<StoredValue>,
}

impl SettingWrite {
    /// A write that sets `value`.
    #[must_use]
    pub fn set(scope: ScopeRef, key: impl Into<String>, value: StoredValue) -> Self {
        Self {
            scope,
            key: key.into(),
            value: Some(value),
        }
    }

    /// A write that removes any value at the scope.
    #[must_use]
    pub fn remove(scope: ScopeRef, key: impl Into<String>) -> Self {
        Self {
            scope,
            key: key.into(),
            value: None,
        }
    }
}

/// The outcome of one write in a batch: the value before and after.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedWrite {
    /// The scope written.
    pub scope: ScopeRef,
    /// The setting key.
    pub key: String,
    /// The value before the write, `None` when unset.
    pub before: Option<StoredValue>,
    /// The value after the write, `None` when removed.
    pub after: Option<StoredValue>,
}

/// Reads and writes setting values by scope.
///
/// Object-safe and synchronous. A store holds the raw [`StoredValue`]s, secrets included; the
/// resolution, diff, screen, summary and hash paths never expose a secret's text.
pub trait ValueStore: Send + Sync {
    /// The value stored for `key` at `scope`, if any.
    fn get(&self, scope: &ScopeRef, key: &str) -> Option<StoredValue>;

    /// Stores `value` for `key` at `scope`.
    fn put(&self, scope: &ScopeRef, key: &str, value: StoredValue);

    /// Removes any value for `key` at `scope`.
    fn remove(&self, scope: &ScopeRef, key: &str);

    /// Every key with a value at `scope`, in key order.
    fn list(&self, scope: &ScopeRef) -> Vec<String>;

    /// Applies a batch of writes and returns each one's before and after value.
    ///
    /// The default applies the writes in order and is **not** atomic; a store that can apply
    /// them all-or-nothing (such as [`MemoryStore`]) overrides it.
    ///
    /// # Errors
    ///
    /// Returns `settings.change_set_invalid` when a store cannot apply the batch.
    fn apply(&self, writes: &[SettingWrite]) -> Result<Vec<AppliedWrite>, SettingsError> {
        let mut outcomes = Vec::with_capacity(writes.len());
        for write in writes {
            let before = self.get(&write.scope, &write.key);
            match &write.value {
                Some(value) => self.put(&write.scope, &write.key, value.clone()),
                None => self.remove(&write.scope, &write.key),
            }
            let after = self.get(&write.scope, &write.key);
            outcomes.push(AppliedWrite {
                scope: write.scope.clone(),
                key: write.key.clone(),
                before,
                after,
            });
        }
        Ok(outcomes)
    }
}

/// An in-memory [`ValueStore`].
///
/// Cheap to clone (the data is shared), and [`MemoryStore::apply`] is atomic: it takes the
/// write lock once, so a batch either fully applies or not at all.
#[derive(Clone, Debug, Default)]
pub struct MemoryStore {
    data: Arc<RwLock<BTreeMap<(ScopeRef, String), StoredValue>>>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A store seeded with the given entries.
    #[must_use]
    pub fn with_entries(
        entries: impl IntoIterator<Item = (ScopeRef, String, StoredValue)>,
    ) -> Self {
        let store = Self::new();
        for (scope, key, value) in entries {
            store.put(&scope, &key, value);
        }
        store
    }

    /// Every entry, in `(scope, key)` order.
    #[must_use]
    pub fn entries(&self) -> Vec<(ScopeRef, String, StoredValue)> {
        let data = self.read();
        data.iter()
            .map(|((scope, key), value)| (scope.clone(), key.clone(), value.clone()))
            .collect()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, BTreeMap<(ScopeRef, String), StoredValue>> {
        self.data
            .read()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, BTreeMap<(ScopeRef, String), StoredValue>> {
        self.data
            .write()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

impl ValueStore for MemoryStore {
    fn get(&self, scope: &ScopeRef, key: &str) -> Option<StoredValue> {
        self.read().get(&(scope.clone(), key.to_owned())).cloned()
    }

    fn put(&self, scope: &ScopeRef, key: &str, value: StoredValue) {
        self.write().insert((scope.clone(), key.to_owned()), value);
    }

    fn remove(&self, scope: &ScopeRef, key: &str) {
        self.write().remove(&(scope.clone(), key.to_owned()));
    }

    fn list(&self, scope: &ScopeRef) -> Vec<String> {
        self.read()
            .keys()
            .filter(|(entry_scope, _)| entry_scope == scope)
            .map(|(_, key)| key.clone())
            .collect()
    }

    fn apply(&self, writes: &[SettingWrite]) -> Result<Vec<AppliedWrite>, SettingsError> {
        let mut data = self.write();
        let mut outcomes = Vec::with_capacity(writes.len());
        for write in writes {
            let slot = (write.scope.clone(), write.key.clone());
            let before = data.get(&slot).cloned();
            match &write.value {
                Some(value) => {
                    data.insert(slot.clone(), value.clone());
                }
                None => {
                    data.remove(&slot);
                }
            }
            let after = data.get(&slot).cloned();
            outcomes.push(AppliedWrite {
                scope: write.scope.clone(),
                key: write.key.clone(),
                before,
                after,
            });
        }
        Ok(outcomes)
    }
}
