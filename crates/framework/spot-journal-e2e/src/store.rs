//! A shared in-memory journal store that records every access, can fail a chosen write, and
//! logs durable tail chunks into an event log (to order them against acknowledgements).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use spot_journal_store::{
    JournalStore, MemoryStore, ObjectEntry, ObjectMeta, StoreError, StoreFuture, StoredObject,
};

/// An ordered log of events from several parts of a scenario.
pub type EventLog = Arc<Mutex<Vec<String>>>;

#[derive(Clone, Default)]
pub struct TestStore {
    inner: Arc<MemoryStore>,
    access: Arc<Mutex<Vec<(&'static str, String)>>>,
    log: EventLog,
    faults: Arc<Mutex<Vec<String>>>,
}

impl TestStore {
    /// A store that writes `tail <key>` into `log` for every tail chunk it accepts.
    pub fn with_log(log: EventLog) -> Self {
        Self {
            log,
            ..Self::default()
        }
    }

    /// Every `(operation, key or prefix)` asked of the store so far.
    pub fn accessed(&self) -> Vec<(&'static str, String)> {
        self.access.lock().unwrap().clone()
    }

    pub fn clear_access(&self) {
        self.access.lock().unwrap().clear();
    }

    /// The next write (put or delete) whose key contains `fragment` fails once, unapplied.
    pub fn fail_next_write(&self, fragment: &str) {
        self.faults.lock().unwrap().push(fragment.to_owned());
    }

    /// Every object under `prefix`, read without being logged.
    pub async fn dump(&self, prefix: &str) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        for entry in self.inner.list(prefix, None, 10_000).await.unwrap() {
            let object = self.inner.get(&entry.key).await.unwrap().unwrap();
            out.insert(entry.key, object.bytes);
        }
        out
    }

    fn note(&self, op: &'static str, key: &str) {
        self.access.lock().unwrap().push((op, key.to_owned()));
    }

    fn faulty(&self, operation: &'static str, key: &str) -> Option<StoreError> {
        let mut faults = self.faults.lock().unwrap();
        let at = faults.iter().position(|f| key.contains(f.as_str()))?;
        faults.remove(at);
        Some(StoreError::Io {
            operation,
            key: key.to_owned(),
            message: "injected failure".to_owned(),
        })
    }
}

impl JournalStore for TestStore {
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
        Box::pin(async move {
            self.note("put_new", key);
            if let Some(error) = self.faulty("write", key) {
                return Err(error);
            }
            let meta = self.inner.put_new(key, bytes).await?;
            if key.contains("/tail/") {
                self.log.lock().unwrap().push(format!("tail {key}"));
            }
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
            self.note("put_replace", key);
            if let Some(error) = self.faulty("replace", key) {
                return Err(error);
            }
            self.inner.put_replace(key, bytes, expected_etag).await
        })
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
        Box::pin(async move {
            self.note("get", key);
            self.inner.get(key).await
        })
    }

    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
        Box::pin(async move {
            self.note("head", key);
            self.inner.head(key).await
        })
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> StoreFuture<'a, Vec<ObjectEntry>> {
        Box::pin(async move {
            self.note("list", prefix);
            self.inner.list(prefix, after, limit).await
        })
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        Box::pin(async move {
            self.note("delete", key);
            if let Some(error) = self.faulty("delete", key) {
                return Err(error);
            }
            self.inner.delete(key).await
        })
    }
}
