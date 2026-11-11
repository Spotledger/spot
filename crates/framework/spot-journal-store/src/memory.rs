//! [`MemoryStore`]: the reference backend, and what a caller uses in tests.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use crate::error::StoreError;
use crate::layout::Key;
use crate::store::{
    JournalStore, ObjectEntry, ObjectMeta, StoreFuture, StoredObject, etag, guard_delete,
    guard_put_new, guard_put_replace, length, page,
};

/// One stored object: the bytes, their etag and their length.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Object {
    bytes: Vec<u8>,
    etag: String,
}

impl Object {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            etag: etag(&bytes),
            bytes,
        }
    }

    fn meta(&self, key: &str) -> ObjectMeta {
        ObjectMeta {
            key: key.to_owned(),
            etag: self.etag.clone(),
            size: length(&self.bytes),
        }
    }
}

/// An in-memory store: no filesystem, no clock, no randomness.
///
/// It is the reference implementation of the contract and the backend a unit test of the relay
/// uses. Its `put_new` is atomic because the map sits behind one mutex: the check and the insert
/// happen inside one lock, so a race has exactly one winner.
#[derive(Debug, Default)]
pub struct MemoryStore {
    objects: Mutex<HashMap<String, Object>>,
}

impl MemoryStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many objects the store holds.
    ///
    /// # Errors
    /// [`StoreError::Backend`] when the lock is poisoned.
    pub fn len(&self) -> Result<usize, StoreError> {
        Ok(self.lock()?.len())
    }

    /// Whether the store holds no object.
    ///
    /// # Errors
    /// [`StoreError::Backend`] when the lock is poisoned.
    pub fn is_empty(&self) -> Result<bool, StoreError> {
        Ok(self.lock()?.is_empty())
    }

    fn lock(&self) -> Result<MutexGuard<'_, HashMap<String, Object>>, StoreError> {
        self.objects.lock().map_err(|_| StoreError::Backend {
            message: "the in-memory store lock is poisoned".to_owned(),
        })
    }
}

impl JournalStore for MemoryStore {
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
        Box::pin(async move {
            let key = key.to_owned();
            guard_put_new(&key)?;
            let object = Object::new(bytes);
            let meta = object.meta(&key);
            let mut objects = self.lock()?;
            if objects.contains_key(&key) {
                return Err(StoreError::AlreadyExists { key });
            }
            objects.insert(key, object);
            Ok(meta)
        })
    }

    fn put_replace<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        expected_etag: &'a str,
    ) -> StoreFuture<'a, ObjectMeta> {
        Box::pin(async move {
            let key = key.to_owned();
            let expected = expected_etag.to_owned();
            guard_put_replace(&key)?;
            let object = Object::new(bytes);
            let meta = object.meta(&key);
            let mut objects = self.lock()?;
            let current = objects
                .get(&key)
                .ok_or_else(|| StoreError::NotFound { key: key.clone() })?;
            if current.etag != expected {
                return Err(StoreError::EtagMismatch { key, expected });
            }
            objects.insert(key, object);
            Ok(meta)
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
        Box::pin(async move {
            let key = key.to_owned();
            Key::parse(&key)?;
            let objects = self.lock()?;
            Ok(objects.get(&key).map(|object| StoredObject {
                meta: object.meta(&key),
                bytes: object.bytes.clone(),
            }))
        })
    }

    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
        Box::pin(async move {
            let key = key.to_owned();
            Key::parse(&key)?;
            let objects = self.lock()?;
            Ok(objects.get(&key).map(|object| object.meta(&key)))
        })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> StoreFuture<'a, Vec<ObjectEntry>> {
        Box::pin(async move {
            let objects = self.lock()?;
            let entries = objects
                .iter()
                .map(|(key, object)| ObjectEntry {
                    key: key.clone(),
                    size: length(&object.bytes),
                })
                .collect();
            Ok(page(entries, prefix, after, limit))
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            let key = key.to_owned();
            guard_delete(&key)?;
            let mut objects = self.lock()?;
            objects
                .remove(&key)
                .map(|_| ())
                .ok_or(StoreError::NotFound { key })
        })
    }
}
