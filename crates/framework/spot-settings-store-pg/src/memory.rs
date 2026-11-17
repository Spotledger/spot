//! The in-memory [`SettingsBackend`].

use std::collections::BTreeMap;
use std::sync::Mutex;

use spot_settings::ScopeRef;

use crate::digest;
use crate::{
    BoxFuture, HistoryRow, SetRequest, SettingsBackend, StoreError, Stored, VERSION_CONFLICT,
};

type Key = (ScopeRef, String);

#[derive(Default)]
struct State {
    values: BTreeMap<Key, Stored>,
    history: BTreeMap<Key, Vec<HistoryRow>>,
    versions: BTreeMap<Key, u64>,
}

/// A store that lives in memory; one lock makes every write atomic.
#[derive(Default)]
pub struct MemorySettingsStore {
    state: Mutex<State>,
}

impl MemorySettingsStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl SettingsBackend for MemorySettingsStore {
    fn get<'a>(
        &'a self,
        scope: &'a ScopeRef,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<Stored>, StoreError>> {
        Box::pin(async move {
            Ok(self
                .lock()
                .values
                .get(&(scope.clone(), key.to_owned()))
                .cloned())
        })
    }

    fn list<'a>(
        &'a self,
        scope: &'a ScopeRef,
    ) -> BoxFuture<'a, Result<Vec<(String, Stored)>, StoreError>> {
        Box::pin(async move {
            Ok(self
                .lock()
                .values
                .iter()
                .filter(|((s, _), _)| s == scope)
                .map(|((_, k), v)| (k.clone(), v.clone()))
                .collect())
        })
    }

    fn set<'a>(&'a self, request: &'a SetRequest) -> BoxFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            let mut state = self.lock();
            let key = (request.scope.clone(), request.key.clone());
            let current = state.values.get(&key).cloned();
            let version = current.as_ref().map_or(0, |s| s.version);
            if request.expected.is_some_and(|e| e != version) {
                return Err(StoreError::new(
                    VERSION_CONFLICT,
                    "the value changed since it was read",
                ));
            }
            // Versions keep counting across a removal, so a stale writer never matches.
            let next = state.versions.get(&key).copied().unwrap_or(version) + 1;
            let row = HistoryRow {
                version: next,
                actor: request.actor.clone(),
                channel: request.channel.clone(),
                at: request.at,
                old_digest: current.as_ref().map(|s| digest::of(&s.value)),
                new_digest: request.value.as_ref().map(digest::of),
            };
            match &request.value {
                Some(value) => {
                    state.values.insert(
                        key.clone(),
                        Stored {
                            value: value.clone(),
                            version: next,
                        },
                    );
                }
                None => {
                    state.values.remove(&key);
                }
            }
            state.versions.insert(key.clone(), next);
            state.history.entry(key).or_default().push(row);
            Ok(next)
        })
    }

    fn history<'a>(
        &'a self,
        scope: &'a ScopeRef,
        key: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<HistoryRow>, StoreError>> {
        Box::pin(async move {
            Ok(self
                .lock()
                .history
                .get(&(scope.clone(), key.to_owned()))
                .map(|rows| rows.iter().rev().take(limit).cloned().collect())
                .unwrap_or_default())
        })
    }
}
