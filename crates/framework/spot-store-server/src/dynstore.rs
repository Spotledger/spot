//! `Arc<dyn Trait>` adapters, so the registry core (generic over its stores) and the service
//! (which swaps PostgreSQL for memory doubles in tests) share one concrete type.

use std::sync::Arc;

use spot_registry::{
    AppName, BlobStore, BoxFuture, BytesStream, Caller, Digest, IndexStore, Ownership,
    PublishOutcome, Publisher, PublisherStatus, Release, Removal, Result, SearchPage, SearchQuery,
    TransferRecord, Version, Visibility,
};

/// An index behind a shared pointer.
pub(crate) struct SharedIndex(pub Arc<dyn IndexStore>);

impl IndexStore for SharedIndex {
    fn preflight(&self, release: &Release) -> Result<()> {
        self.0.preflight(release)
    }

    fn commit(&self, release: Release, publisher: Publisher) -> Result<PublishOutcome> {
        self.0.commit(release, publisher)
    }

    fn ownership(&self, app: &AppName) -> Result<Option<Ownership>> {
        self.0.ownership(app)
    }

    fn publisher(&self, id: &str) -> Result<Option<Publisher>> {
        self.0.publisher(id)
    }

    fn set_publisher_status(&self, id: &str, status: PublisherStatus) -> Result<()> {
        self.0.set_publisher_status(id, status)
    }

    fn releases(
        &self,
        app: &AppName,
        visibility: &Visibility,
        caller: Option<&Caller>,
    ) -> Result<Vec<Release>> {
        self.0.releases(app, visibility, caller)
    }

    fn latest(&self, app: &AppName, include_pre: bool) -> Result<Option<Release>> {
        self.0.latest(app, include_pre)
    }

    fn by_digest(&self, digest: Digest) -> Result<Option<Release>> {
        self.0.by_digest(digest)
    }

    fn release(&self, app: &AppName, version: &Version) -> Result<Option<Release>> {
        self.0.release(app, version)
    }

    fn search(&self, query: &SearchQuery) -> Result<SearchPage> {
        self.0.search(query)
    }

    fn remove(
        &self,
        app: &AppName,
        version: &Version,
        by: &str,
        removal: Removal,
        withdraw: bool,
    ) -> Result<Option<Release>> {
        self.0.remove(app, version, by, removal, withdraw)
    }

    fn transfer(&self, transfer: TransferRecord, recipient: Publisher) -> Result<()> {
        self.0.transfer(transfer, recipient)
    }

    fn transfers(&self, app: &AppName) -> Result<Vec<TransferRecord>> {
        self.0.transfers(app)
    }

    fn digests(&self) -> Result<Vec<Digest>> {
        self.0.digests()
    }

    fn describe(
        &self,
        app: &AppName,
        by: &str,
        description: String,
        category: String,
    ) -> Result<()> {
        self.0.describe(app, by, description, category)
    }
}

/// A blob store behind a shared pointer.
pub(crate) struct SharedBlobs(pub Arc<dyn BlobStore>);

impl BlobStore for SharedBlobs {
    fn put_if_absent(&self, digest: Digest, stream: BytesStream) -> BoxFuture<'_, ()> {
        self.0.put_if_absent(digest, stream)
    }

    fn get(&self, digest: Digest) -> BoxFuture<'_, BytesStream> {
        self.0.get(digest)
    }

    fn exists(&self, digest: Digest) -> BoxFuture<'_, bool> {
        self.0.exists(digest)
    }

    fn size(&self, digest: Digest) -> BoxFuture<'_, u64> {
        self.0.size(digest)
    }

    fn inventory(&self) -> BoxFuture<'_, Vec<Digest>> {
        self.0.inventory()
    }
}
