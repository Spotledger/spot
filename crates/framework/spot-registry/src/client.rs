use crate::{
    AppName, BlobStore, BoxFuture, Caller, IndexStore, Registry, Release, SearchPage, SearchQuery,
    Version, Visibility,
};
use spot_bundle_signing::Verified;
use std::sync::Arc;
/// Bytes exposed only after digest, manifest identity and current signature/trust checks.
#[derive(Clone, Debug)]
pub struct VerifiedBundle {
    /// Validated archive bytes.
    pub bytes: Vec<u8>,
    /// Authenticated release facts and current availability markers.
    pub release: Release,
    /// Signature verifier evidence.
    pub verified: Verified,
}
/// Object-safe installer-facing registry interface. Time is explicit; transports must
/// enforce the same verification contract before constructing VerifiedBundle.
pub trait RegistryClient: Send + Sync {
    /// Latest available release.
    fn latest<'a>(&'a self, app: &'a AppName, include_pre: bool) -> BoxFuture<'a, Option<Release>>;
    /// Available versions with explicit prerelease visibility.
    fn releases<'a>(
        &'a self,
        app: &'a AppName,
        visibility: &'a Visibility,
        caller: Option<&'a Caller>,
        now: i64,
    ) -> BoxFuture<'a, Vec<Release>>;
    /// Verified bytes for a new install; no yanked/withdrawn release allowed.
    fn fetch<'a>(
        &'a self,
        app: &'a AppName,
        version: &'a Version,
        now: i64,
    ) -> BoxFuture<'a, VerifiedBundle>;
    /// Ranked catalogue page.
    fn search<'a>(&'a self, query: &'a SearchQuery, now: i64) -> BoxFuture<'a, SearchPage>;
}
/// In-process client over the same rules a future network transport must enforce.
pub struct LocalRegistryClient<I: IndexStore, B: BlobStore> {
    registry: Arc<Registry<I, B>>,
}
impl<I: IndexStore, B: BlobStore> LocalRegistryClient<I, B> {
    /// Wraps a registry without changing trust or policy.
    pub fn new(registry: Arc<Registry<I, B>>) -> Self {
        Self { registry }
    }
}
impl<I: IndexStore, B: BlobStore> RegistryClient for LocalRegistryClient<I, B> {
    fn latest<'a>(&'a self, app: &'a AppName, include_pre: bool) -> BoxFuture<'a, Option<Release>> {
        Box::pin(async move { self.registry.latest(app, include_pre) })
    }
    fn releases<'a>(
        &'a self,
        app: &'a AppName,
        visibility: &'a Visibility,
        caller: Option<&'a Caller>,
        now: i64,
    ) -> BoxFuture<'a, Vec<Release>> {
        Box::pin(async move { self.registry.releases(app, visibility, caller, now) })
    }
    fn fetch<'a>(
        &'a self,
        app: &'a AppName,
        version: &'a Version,
        now: i64,
    ) -> BoxFuture<'a, VerifiedBundle> {
        Box::pin(self.registry.fetch(app, version, now))
    }
    fn search<'a>(&'a self, query: &'a SearchQuery, now: i64) -> BoxFuture<'a, SearchPage> {
        Box::pin(async move { self.registry.search(query, now) })
    }
}
