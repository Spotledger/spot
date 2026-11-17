//! A journal store that can reach one tenant's prefix only.

use spot_journal_store::{
    JournalStore, ObjectEntry, ObjectMeta, StoreError, StoreFuture, StoredObject,
};

/// Wraps a store so that every key, and every listing prefix, must start with `<tenant>/`.
/// Another tenant's object cannot be read, listed, written or replaced through it, whatever a
/// caller computes (`journal_store` itself is tenant-agnostic: the layout carries the tenant in
/// the key).
#[derive(Debug)]
pub struct TenantScoped<S> {
    inner: S,
    prefix: String,
}

impl<S: JournalStore> TenantScoped<S> {
    /// A view of `inner` limited to `tenant`.
    ///
    /// # Errors
    /// [`StoreError::KeyInvalid`] when the tenant id is not one safe path component.
    pub fn new(inner: S, tenant: &str) -> Result<Self, StoreError> {
        spot_journal_store::TenantId::new(tenant)?;
        Ok(Self {
            inner,
            prefix: format!("{tenant}/"),
        })
    }

    fn check(&self, key: &str) -> Result<(), StoreError> {
        if key.starts_with(&self.prefix) {
            Ok(())
        } else {
            Err(StoreError::KeyInvalid(
                "the key is outside this tenant's prefix",
            ))
        }
    }
}

impl<S: JournalStore> JournalStore for TenantScoped<S> {
    fn put_new<'a>(&'a self, key: &'a str, bytes: Vec<u8>) -> StoreFuture<'a, ObjectMeta> {
        if let Err(error) = self.check(key) {
            return Box::pin(async move { Err(error) });
        }
        self.inner.put_new(key, bytes)
    }

    fn put_replace<'a>(
        &'a self,
        key: &'a str,
        bytes: Vec<u8>,
        expected_etag: &'a str,
    ) -> StoreFuture<'a, ObjectMeta> {
        if let Err(error) = self.check(key) {
            return Box::pin(async move { Err(error) });
        }
        self.inner.put_replace(key, bytes, expected_etag)
    }

    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<StoredObject>> {
        if let Err(error) = self.check(key) {
            return Box::pin(async move { Err(error) });
        }
        self.inner.get(key)
    }

    fn head<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Option<ObjectMeta>> {
        if let Err(error) = self.check(key) {
            return Box::pin(async move { Err(error) });
        }
        self.inner.head(key)
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        after: Option<&'a str>,
        limit: usize,
    ) -> StoreFuture<'a, Vec<ObjectEntry>> {
        if let Err(error) = self.check(prefix) {
            return Box::pin(async move { Err(error) });
        }
        self.inner.list(prefix, after, limit)
    }

    fn delete<'a>(&'a self, key: &'a str) -> StoreFuture<'a, ()> {
        if let Err(error) = self.check(key) {
            return Box::pin(async move { Err(error) });
        }
        self.inner.delete(key)
    }
}
