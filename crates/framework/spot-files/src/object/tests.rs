//! The contract suite over the shared code paths, on a scratch client that needs no
//! cloud service, plus one oversize-put test proving a refused body leaves nothing
//! behind. The same suite against the real services runs from `tests/object_contract.rs`
//! through `tests/with-object-stores.sh` (MinIO and Azurite): the tests there read the
//! `SPOT_TEST_*` variables the script exports, and a missing variable fails the test
//! naming the script — they never skip silently.
//!
//! The scratch client is object_store's in-memory one: its filesystem client rejects
//! every put that carries attributes, and the contract puts always do.
//!
//! Mid-upload failures are injected with [`FailingStore`] — a test-only wrapper client
//! (the trait's stream types and async form need the `futures-util`/`async-trait`
//! dev-dependencies; see `Cargo.toml`). Its tests prove that a part failure or a failed
//! completion after the upload started calls `abort_multipart`, leaves no object behind,
//! and that a listing HEAD failure surfaces as an error instead of silently dropping a
//! live key from the page (lead review of #446, items 2 and 3).

use super::{Backend, ObjectFileStore, PART_SIZE, SINGLE_PUT_LIMIT};
use crate::contract_tests;
use crate::error::FileError;
use crate::key::{FileKey, FileKeyPrefix, Visibility};
use crate::store::{FileStore, PutOptions, body_from_vec};
use sha2::{Digest, Sha256};
use spot_core::SiteId;
use tokio::io::AsyncReadExt;

/// Turns a `Result` into its value for test assertions, panicking with context instead
/// of `unwrap`/`expect`, which this workspace forbids.
fn expect_ok<T, E>(outcome: Result<T, E>, what: &str) -> T
where
    E: core::fmt::Debug,
{
    match outcome {
        Ok(value) => value,
        Err(error) => panic!("{what}: {error:?}"),
    }
}

/// A fresh, empty backend under the shared code paths, labelled s3 for messages.
#[track_caller]
fn scratch_store() -> ObjectFileStore {
    ObjectFileStore::from_parts(Backend::S3, object_store::memory::InMemory::new())
}

/// Every provider contract case holding on the object provider's shared code.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn object_contract_suite() {
    let store = scratch_store();
    contract_tests::run_all(&store).await.assert_all_passed();
}

/// An oversize body stops while it is still being counted into the provider's local
/// spool: no upload ever starts, and afterwards the site lists empty and `head`
/// answers `None` — no partial object remains anywhere in the bucket prefix.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversize_abort_leaves_no_partial() {
    let store = scratch_store();
    let site = expect_ok(SiteId::new("contract"), "building the site id");
    let key = expect_ok(
        FileKey::new(Visibility::Private, "contract/object/oversize.bin"),
        "building the oversize key",
    );
    let refused = store
        .put(
            &site,
            key.clone(),
            body_from_vec(vec![7u8; (SINGLE_PUT_LIMIT + 1) as usize]),
            PutOptions::new("application/octet-stream", SINGLE_PUT_LIMIT, true),
        )
        .await;
    match refused {
        Err(FileError::TooLarge { .. }) => {}
        other => panic!("the oversize put must fail with files.too_large, got {other:?}"),
    }
    let listed = expect_ok(
        store.list(&site, &FileKeyPrefix::all(), None, 1_000).await,
        "listing the site after the refused put",
    );
    assert!(
        listed.is_empty(),
        "a refused oversize put must leave nothing, listed {listed:?}"
    );
    let head = expect_ok(store.head(&site, key).await, "heading the refused key");
    assert!(
        head.is_none(),
        "a refused oversize put must not be readable"
    );
}

/// A deterministic, non-uniform body: the bytes cycle, so a part landing in the
/// wrong order or an off-by-one in the reassembled object cannot survive a
/// byte-for-byte comparison.
fn pattern(len: usize, start: u8) -> Vec<u8> {
    (0..len).map(|i| start.wrapping_add(i as u8)).collect()
}

/// The digest [`StoredFile::sha256`](crate::store::StoredFile::sha256) and the
/// metadata carry for a body.
fn digest_of(body: &[u8]) -> [u8; 32] {
    Sha256::digest(body).into()
}

/// A body one byte past the single-put limit is the smallest body the multipart
/// path serves: one exactly-`PART_SIZE` part plus a one-byte final part. The put
/// reports the streamed digest, `get` returns the identical bytes with the stamped
/// attributes, `head` agrees without reading the body, and `list` carries both off
/// the object's metadata — the multipart path must behave exactly like a small put.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multipart_round_trip_one_full_part_plus_remainder() {
    let store = scratch_store();
    let site = expect_ok(SiteId::new("contract"), "building the site id");
    let key = expect_ok(
        FileKey::new(
            Visibility::Private,
            "contract/object/multipart-boundary.bin",
        ),
        "building the key",
    );
    let body = pattern((SINGLE_PUT_LIMIT + 1) as usize, 3);
    let stored = expect_ok(
        store
            .put(
                &site,
                key.clone(),
                body_from_vec(body.clone()),
                PutOptions::new("application/x-spot-multipart-test", u64::MAX, true),
            )
            .await,
        "putting the boundary body",
    );
    assert_eq!(
        stored.sha256,
        digest_of(&body),
        "the put must report the streamed digest",
    );

    let (meta, mut read) = expect_ok(store.get(&site, key.clone()).await, "reading it back");
    let mut round_tripped = Vec::new();
    expect_ok(
        read.read_to_end(&mut round_tripped).await,
        "reading the body",
    );
    assert_eq!(
        round_tripped, body,
        "the body must round-trip byte for byte"
    );
    assert_eq!(meta.size, stored.size);
    assert_eq!(meta.sha256, stored.sha256);
    assert_eq!(
        meta.content_type, "application/x-spot-multipart-test",
        "the stamped content type must survive the round trip",
    );

    let head = expect_ok(store.head(&site, key.clone()).await, "heading the object");
    let head = match head {
        Some(meta) => meta,
        None => panic!("a stored object must head as Some"),
    };
    assert_eq!(head.size, stored.size);
    assert_eq!(head.sha256, stored.sha256);

    // Prefixes match full key text, which starts with the visibility segment.
    let prefix = expect_ok(
        FileKeyPrefix::new(&format!("{}/contract/object", Visibility::Private.as_str())),
        "building the prefix",
    );
    let listed = expect_ok(
        store.list(&site, &prefix, None, 100).await,
        "listing the object's directory",
    );
    let [entry] = listed.as_slice() else {
        panic!("expected exactly one entry under the prefix, got {listed:?}")
    };
    assert_eq!(entry.key.as_str(), key.as_str());
    assert_eq!(entry.size, stored.size);
    assert_eq!(entry.sha256, stored.sha256);
}

/// Two full parts plus a small remainder — three parts, every full part exactly
/// `PART_SIZE` and the final one smaller — round-trips exactly. This walks the
/// upload loop past its first iteration: a part boundary inside the body must not
/// shift what get returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multipart_round_trip_two_full_parts_and_remainder() {
    let store = scratch_store();
    let site = expect_ok(SiteId::new("contract"), "building the site id");
    let key = expect_ok(
        FileKey::new(
            Visibility::Private,
            "contract/object/multipart-three-parts.bin",
        ),
        "building the key",
    );
    let body = pattern(2 * PART_SIZE + 1024 * 1024, 7);
    let stored = expect_ok(
        store
            .put(
                &site,
                key.clone(),
                body_from_vec(body.clone()),
                PutOptions::new("application/x-spot-multipart-test", u64::MAX, true),
            )
            .await,
        "putting the three-part body",
    );
    assert_eq!(
        stored.sha256,
        digest_of(&body),
        "the put must report the streamed digest",
    );

    let (meta, mut read) = expect_ok(store.get(&site, key.clone()).await, "reading it back");
    let mut round_tripped = Vec::new();
    expect_ok(
        read.read_to_end(&mut round_tripped).await,
        "reading the body",
    );
    assert_eq!(
        round_tripped, body,
        "the body must round-trip byte for byte"
    );
    assert_eq!(meta.size, stored.size);
    assert_eq!(meta.sha256, stored.sha256);

    let head = expect_ok(store.head(&site, key.clone()).await, "heading the object");
    let head = match head {
        Some(meta) => meta,
        None => panic!("a stored object must head as Some"),
    };
    assert_eq!(head.size, stored.size);
    assert_eq!(head.sha256, stored.sha256);
}

/// A conditional create (`overwrite = false`) of a large body over an existing
/// large object is refused with `AlreadyExists` — the HEAD guard in front of the
/// multipart initiation — and the old object is untouched afterwards, digest and
/// bytes included. The multipart path cannot be atomic, but the guard must hold
/// whenever the object is already visible.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multipart_create_of_existing_large_key_is_refused() {
    let store = scratch_store();
    let site = expect_ok(SiteId::new("contract"), "building the site id");
    let key = expect_ok(
        FileKey::new(Visibility::Private, "contract/object/multipart-guarded.bin"),
        "building the key",
    );
    let first = pattern((SINGLE_PUT_LIMIT + 1) as usize, 1);
    let stored = expect_ok(
        store
            .put(
                &site,
                key.clone(),
                body_from_vec(first.clone()),
                PutOptions::new("application/x-spot-multipart-test", u64::MAX, true),
            )
            .await,
        "putting the first large body",
    );
    let second = pattern((SINGLE_PUT_LIMIT + 1) as usize, 250);
    let refused = store
        .put(
            &site,
            key.clone(),
            body_from_vec(second),
            PutOptions::new("application/x-spot-multipart-test", u64::MAX, false),
        )
        .await;
    match refused {
        Err(FileError::AlreadyExists { .. }) => {}
        other => panic!("the guarded create must be refused, got {other:?}"),
    }

    // The old object is still there, digest-identical by head…
    let head = expect_ok(store.head(&site, key.clone()).await, "heading the survivor");
    let head = match head {
        Some(meta) => meta,
        None => panic!("the refused create must leave the old object in place"),
    };
    assert_eq!(head.size, stored.size);
    assert_eq!(head.sha256, stored.sha256);

    // …and byte-identical through get.
    let (_, mut read) = expect_ok(store.get(&site, key.clone()).await, "reading the survivor");
    let mut round_tripped = Vec::new();
    expect_ok(
        read.read_to_end(&mut round_tripped).await,
        "reading the body",
    );
    assert_eq!(
        round_tripped, first,
        "the refused create must not touch the old bytes",
    );
}

/// The fault flags one [`FailingStore`] injects, shared with the store's uploads.
#[derive(Debug, Default)]
struct Faults {
    /// The number of part uploads that still succeed before every later one fails;
    /// `usize::MAX` never fails a part.
    fail_part_after: usize,
    /// Fail the upload's `complete`.
    fail_complete: std::sync::atomic::AtomicBool,
    /// Fail HEAD requests (`get_opts` with `head`) with a generic backend error.
    fail_head: std::sync::atomic::AtomicBool,
    /// Set once an upload's `abort` ran — the provider's side of the promise.
    aborted: std::sync::atomic::AtomicBool,
}

/// A wrapper client that delegates to `InMemory` and fails the operations `faults`
/// arms — the only way a unit test can make an upload fail mid-part (lead review
/// of #446, item 3).
#[derive(Debug)]
struct FailingStore {
    inner: object_store::memory::InMemory,
    faults: std::sync::Arc<Faults>,
}

impl std::fmt::Display for FailingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FailingStore")
    }
}

/// The injected backend error, named after what it simulates.
fn injected(what: &str) -> object_store::Error {
    object_store::Error::Generic {
        store: "FailingStore",
        source: what.to_owned().into(),
    }
}

/// A store over the fault-injecting client.
fn failing_store(faults: std::sync::Arc<Faults>) -> ObjectFileStore {
    ObjectFileStore::from_parts(
        Backend::S3,
        FailingStore {
            inner: object_store::memory::InMemory::new(),
            faults,
        },
    )
}

#[async_trait::async_trait]
impl object_store::ObjectStore for FailingStore {
    async fn put_opts(
        &self,
        location: &object_store::path::Path,
        payload: object_store::PutPayload,
        opts: object_store::PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &object_store::path::Path,
        opts: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        let upload = self.inner.put_multipart_opts(location, opts).await?;
        Ok(Box::new(FailingUpload {
            inner: upload,
            parts: 0,
            faults: std::sync::Arc::clone(&self.faults),
        }))
    }

    async fn get_opts(
        &self,
        location: &object_store::path::Path,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        if self
            .faults
            .fail_head
            .load(std::sync::atomic::Ordering::SeqCst)
            && options.head
        {
            return Err(injected("injected HEAD failure"));
        }
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: futures_util::stream::BoxStream<
            'static,
            object_store::Result<object_store::path::Path>,
        >,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::path::Path>>
    {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>>
    {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&object_store::path::Path>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &object_store::path::Path,
        to: &object_store::path::Path,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

/// The upload half of [`FailingStore`]: parts past `fail_part_after` fail, a
/// completion fails when armed, and `abort` records that the provider aborted.
#[derive(Debug)]
struct FailingUpload {
    inner: Box<dyn object_store::MultipartUpload>,
    parts: usize,
    faults: std::sync::Arc<Faults>,
}

#[async_trait::async_trait]
impl object_store::MultipartUpload for FailingUpload {
    fn put_part(&mut self, data: object_store::PutPayload) -> object_store::UploadPart {
        self.parts += 1;
        if self.parts > self.faults.fail_part_after {
            return Box::pin(futures_util::future::ready(Err(injected(
                "injected part failure",
            ))));
        }
        self.inner.put_part(data)
    }

    async fn complete(&mut self) -> object_store::Result<object_store::PutResult> {
        if self
            .faults
            .fail_complete
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(injected("injected completion failure"));
        }
        self.inner.complete().await
    }

    async fn abort(&mut self) -> object_store::Result<()> {
        self.faults
            .aborted
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.inner.abort().await
    }
}

/// A part upload failing after the multipart upload started: the provider aborts
/// the upload, reports `files.io`, and the object is neither listed nor headed —
/// no partial object and no orphaned part set (lead review of #446, item 3).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multipart_part_failure_aborts_the_upload() {
    let faults = std::sync::Arc::new(Faults {
        fail_part_after: 0,
        ..Default::default()
    });
    let store = failing_store(std::sync::Arc::clone(&faults));
    let site = expect_ok(SiteId::new("contract"), "building the site id");
    let key = expect_ok(
        FileKey::new(
            Visibility::Private,
            "contract/object/multipart-failing-part.bin",
        ),
        "building the key",
    );
    let failed = store
        .put(
            &site,
            key.clone(),
            body_from_vec(pattern(PART_SIZE + 1024 * 1024, 11)),
            PutOptions::new("application/x-spot-multipart-test", u64::MAX, true),
        )
        .await;
    match failed {
        Err(e) => assert_eq!(e.code(), "files.io", "the injected part failure"),
        Ok(stored) => panic!("the injected part failure must fail the put: {stored:?}"),
    }
    assert!(
        faults.aborted.load(std::sync::atomic::Ordering::SeqCst),
        "a part failure after the upload started must abort it",
    );
    let listed = expect_ok(
        store.list(&site, &FileKeyPrefix::all(), None, 100).await,
        "listing after the aborted upload",
    );
    assert!(
        listed.is_empty(),
        "an aborted upload must leave no object, listed {listed:?}"
    );
    let head = expect_ok(store.head(&site, key).await, "heading the aborted key");
    assert!(head.is_none(), "an aborted upload must not be readable");
}

/// The same promise one step later: a completion that fails after every part
/// uploaded still aborts the upload and leaves no object.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multipart_completion_failure_aborts_the_upload() {
    let faults = std::sync::Arc::new(Faults {
        fail_part_after: usize::MAX,
        fail_complete: std::sync::atomic::AtomicBool::new(true),
        ..Default::default()
    });
    let store = failing_store(std::sync::Arc::clone(&faults));
    let site = expect_ok(SiteId::new("contract"), "building the site id");
    let key = expect_ok(
        FileKey::new(
            Visibility::Private,
            "contract/object/multipart-failing-complete.bin",
        ),
        "building the key",
    );
    let failed = store
        .put(
            &site,
            key.clone(),
            body_from_vec(pattern(PART_SIZE + 1024 * 1024, 13)),
            PutOptions::new("application/x-spot-multipart-test", u64::MAX, true),
        )
        .await;
    match failed {
        Err(e) => assert_eq!(e.code(), "files.io", "the injected completion failure"),
        Ok(stored) => panic!("the injected completion must fail the put: {stored:?}"),
    }
    assert!(
        faults.aborted.load(std::sync::atomic::Ordering::SeqCst),
        "a failed completion must abort the started upload",
    );
    let listed = expect_ok(
        store.list(&site, &FileKeyPrefix::all(), None, 100).await,
        "listing after the aborted upload",
    );
    assert!(
        listed.is_empty(),
        "an aborted upload must leave no object, listed {listed:?}"
    );
}

/// A HEAD that fails during `head` or a listing page fails the operation with
/// `files.io` instead of silently dropping the entry: a live key dropped from a
/// page would be lost forever to `after` paging (lead review of #446, item 2).
/// The `NotFound`-as-absent half of the same rule is the contract suite's
/// `delete_semantics` case: deleting a never-put key answers `false` through this
/// very path, so `NotFound` must keep mapping to absent.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn head_failures_fail_head_and_list() {
    let faults = std::sync::Arc::new(Faults::default());
    let store = failing_store(std::sync::Arc::clone(&faults));
    let site = expect_ok(SiteId::new("contract"), "building the site id");
    let key = expect_ok(
        FileKey::new(Visibility::Private, "contract/object/head-failure.bin"),
        "building the key",
    );
    expect_ok(
        store
            .put(
                &site,
                key.clone(),
                body_from_vec(pattern(1024, 5)),
                PutOptions::new("text/plain", u64::MAX, true),
            )
            .await,
        "staging the object",
    );
    faults
        .fail_head
        .store(true, std::sync::atomic::Ordering::SeqCst);
    match store.list(&site, &FileKeyPrefix::all(), None, 100).await {
        Err(e) => assert_eq!(e.code(), "files.io", "the injected HEAD in list"),
        Ok(entries) => panic!("a failing HEAD must fail the listing, got {entries:?}"),
    }
    match store.head(&site, key.clone()).await {
        Err(e) => assert_eq!(e.code(), "files.io", "the injected HEAD in head"),
        Ok(maybe) => panic!("a failing HEAD must fail head, got {maybe:?}"),
    }
}
