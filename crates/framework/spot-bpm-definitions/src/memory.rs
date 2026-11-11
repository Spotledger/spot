//! The in-memory reference store, for tests and for hosts that need no durability.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, MutexGuard};

use spot_app_api::prelude::OffsetDateTime;

use crate::error::DefinitionError;
use crate::store::{DefFuture, DefinitionStore};
use crate::types::{
    MAX_LABEL_BYTES, MAX_PAGE, StoredVersion, VersionState, check_name, check_publish,
};

#[derive(Debug, Default)]
struct Inner {
    versions: BTreeMap<String, Vec<StoredVersion>>,
    pins: BTreeMap<String, BTreeSet<u32>>,
}

/// A [`DefinitionStore`] in memory. It serves one tenant by construction (one instance, one
/// tenant), exactly as the PostgreSQL store does.
#[derive(Debug, Default)]
pub struct MemoryDefinitionStore {
    inner: Mutex<Inner>,
}

impl MemoryDefinitionStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records which versions of `name` running Cases are pinned to (the memory store has no Case
    /// table to read them from; a host or a test says).
    pub fn set_running_pins(&self, name: &str, versions: impl IntoIterator<Item = u32>) {
        self.lock()
            .pins
            .insert(name.to_owned(), versions.into_iter().collect());
    }
}

impl DefinitionStore for MemoryDefinitionStore {
    fn publish<'a>(
        &'a self,
        name: &'a str,
        text: &'a str,
        expected_previous: Option<u32>,
        publisher: &'a str,
        note: Option<&'a str>,
        now: OffsetDateTime,
    ) -> DefFuture<'a, StoredVersion> {
        Box::pin(async move {
            let content_hash = check_publish(name, text, publisher, note)?;
            let mut inner = self.lock();
            let count = inner.versions.get(name).map_or(0, Vec::len);
            let latest = u32::try_from(count).ok().filter(|n| *n > 0);
            if latest != expected_previous {
                return Err(DefinitionError::VersionConflict {
                    name: name.to_owned(),
                    expected: expected_previous,
                    found: latest,
                });
            }
            let version = latest.unwrap_or(0).saturating_add(1);
            let stored = StoredVersion {
                name: name.to_owned(),
                version,
                text: text.to_owned(),
                content_hash,
                state: VersionState::Published,
                published_by: publisher.to_owned(),
                published_at: now,
                note: note.map(str::to_owned),
                retired_at: None,
                retired_by: None,
            };
            inner
                .versions
                .entry(name.to_owned())
                .or_default()
                .push(stored.clone());
            Ok(stored)
        })
    }

    fn get<'a>(&'a self, name: &'a str, version: u32) -> DefFuture<'a, Option<StoredVersion>> {
        Box::pin(async move {
            check_name(name)?;
            let inner = self.lock();
            Ok(version
                .checked_sub(1)
                .and_then(|index| inner.versions.get(name)?.get(index as usize))
                .cloned())
        })
    }

    fn latest_published<'a>(&'a self, name: &'a str) -> DefFuture<'a, Option<StoredVersion>> {
        Box::pin(async move {
            check_name(name)?;
            let inner = self.lock();
            Ok(inner.versions.get(name).and_then(|list| {
                list.iter()
                    .rev()
                    .find(|v| v.state == VersionState::Published)
                    .cloned()
            }))
        })
    }

    fn list<'a>(
        &'a self,
        name: &'a str,
        after: Option<u32>,
        limit: usize,
    ) -> DefFuture<'a, Vec<StoredVersion>> {
        Box::pin(async move {
            check_name(name)?;
            let after = after.unwrap_or(0);
            let inner = self.lock();
            Ok(inner
                .versions
                .get(name)
                .map(|list| {
                    list.iter()
                        .filter(|v| v.version > after)
                        .take(limit.min(MAX_PAGE))
                        .cloned()
                        .collect()
                })
                .unwrap_or_default())
        })
    }

    fn names<'a>(&'a self) -> DefFuture<'a, Vec<String>> {
        Box::pin(async move { Ok(self.lock().versions.keys().cloned().collect()) })
    }

    fn retire<'a>(
        &'a self,
        name: &'a str,
        version: u32,
        by: &'a str,
        now: OffsetDateTime,
    ) -> DefFuture<'a, StoredVersion> {
        Box::pin(async move {
            check_name(name)?;
            if by.is_empty() || by.contains('\0') {
                return Err(DefinitionError::InputInvalid { field: "publisher" });
            }
            if by.len() > MAX_LABEL_BYTES {
                return Err(DefinitionError::TooLarge {
                    field: "publisher",
                    len: by.len(),
                    max: MAX_LABEL_BYTES,
                });
            }
            let mut inner = self.lock();
            let stored = version
                .checked_sub(1)
                .and_then(|index| inner.versions.get_mut(name)?.get_mut(index as usize))
                .ok_or_else(|| DefinitionError::NotFound {
                    name: name.to_owned(),
                    version,
                })?;
            if stored.state == VersionState::Published {
                stored.state = VersionState::Retired;
                stored.retired_at = Some(now);
                stored.retired_by = Some(by.to_owned());
            }
            Ok(stored.clone())
        })
    }

    fn running_pins<'a>(&'a self, name: &'a str) -> DefFuture<'a, Vec<u32>> {
        Box::pin(async move {
            check_name(name)?;
            Ok(self
                .lock()
                .pins
                .get(name)
                .map(|set| set.iter().copied().collect())
                .unwrap_or_default())
        })
    }
}
