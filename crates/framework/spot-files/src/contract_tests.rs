//! The provider contract suite: behaviour every `spot-files` backend must show
//! (WP-PLT-10). A provider runs it from its own `tests/`:
//!
//! ```ignore
//! #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
//! async fn contract_suite() {
//!     let store = MyStore::new(/* scratch root */);
//!     spot_files::contract_tests::run_all(&store).await.assert_all_passed();
//! }
//! ```
//!
//! Cases hold on any [`FileStore`]: they use only the trait, touch only their own key
//! subtree (`…/contract/…`) and assume a freshly empty store for their keys — both
//! built-in tests start from a scratch store. The suite never *depends* on a particular
//! executor around it: everything is plain futures over the trait, and the concurrency
//! case drives itself with pure-std code ([`drive_concurrently`]), parking on a
//! `Condvar`-based [`Wake`]. That is what keeps it runnable by WP-PLT-10.T2's S3/Azure
//! providers without forking behaviours, and lets `tests/memory.rs` pass on a
//! currentThread runtime.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use spot_core::SiteId;
use tokio::io::AsyncReadExt;

use crate::error::FileError;
use crate::key::{FileKey, FileKeyPrefix, Visibility};
use crate::store::{
    Body, FileResult, FileStore, PutOptions, StoredFile, body_from_str, body_from_vec,
};

/// Why a case failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure(pub String);

impl From<FileError> for Failure {
    fn from(e: FileError) -> Self {
        Failure(format!("unexpected file error [{}]: {e}", e.code()))
    }
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Result of one case.
pub type CaseResult = Result<(), Failure>;

/// The outcome of one case.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// Case name.
    pub case: &'static str,
    /// Its result.
    pub result: CaseResult,
}

/// Outcomes of a full run.
#[derive(Clone, Debug, Default)]
pub struct Report {
    /// One per case, in run order.
    pub outcomes: Vec<Outcome>,
}

impl Report {
    /// The failed cases.
    #[must_use]
    pub fn failures(&self) -> Vec<&Outcome> {
        self.outcomes.iter().filter(|o| o.result.is_err()).collect()
    }

    /// Panics with every failure listed, unless all cases passed.
    pub fn assert_all_passed(&self) {
        let failed = self.failures();
        if !failed.is_empty() {
            let lines: Vec<String> = failed
                .iter()
                .map(|o| {
                    let why = o.result.as_ref().err().map_or("", |f| &f.0);
                    format!("  {}: {why}", o.case)
                })
                .collect();
            panic!(
                "{} of {} spot-files contract cases failed:\n{}",
                failed.len(),
                self.outcomes.len(),
                lines.join("\n")
            );
        }
    }
}

/// The `max_bytes` every suite put runs with: small enough to test limits, big enough
/// to pass real bytes through.
const TEST_LIMIT: u64 = 128 * 1024;

/// The site every case runs on; its name validates by construction.
const SUITE_SITE: &str = "spot-files-contract.test";

/// The second site, used by the isolation case only.
const SUITE_SITE_ALT: &str = "spot-files-contract-other.test";

/// The wake mechanism [`drive_concurrently`] parks on: a `Wake` impl that signals a
/// condition variable instead of a runtime's scheduler, so futures written for an
/// executor still run under a plain thread loop.
#[derive(Default)]
struct ParkSignal(Mutex<bool>, Condvar);

impl Wake for ParkSignal {
    fn wake(self: Arc<Self>) {
        let mut fired = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        *fired = true;
        self.1.notify_all();
    }
}

/// Parks until a wake fired, or (fallback) one one-second slice elapsed, so a wake that
/// races the round boundary cannot hang a case forever. Returns `false` when the
/// `deadline` is out.
fn park_one(signal: &ParkSignal, deadline: Instant) -> bool {
    let mut fired: MutexGuard<'_, bool> = signal.0.lock().unwrap_or_else(PoisonError::into_inner);
    while !*fired {
        if Instant::now() >= deadline {
            return false;
        }
        let (awake, _timed_out) = signal
            .1
            .wait_timeout(fired, Duration::from_secs(1))
            .unwrap_or_else(PoisonError::into_inner);
        fired = awake;
    }
    true
}

/// A boxed `Send` future; the plain type the polling driver juggles.
type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Drives `futures` to completion from plain code — no runtime: polls each once a
/// round, parks while all are pending, resumes on the next wake, and never re-polls a
/// future without a wake in between (a poll is only "owed" after a wake). Returns each
/// future's output in the input order, or a [`Failure`] if the `budget_ms` budget ran
/// out with futures unfinished.
///
/// This is also the template for T2's network-provider concurrency tests: the same
/// driver works wherever the provider's futures are `Send`.
pub fn drive_concurrently<'a, I, F, T>(futures: I, budget_ms: u64) -> Result<Vec<T>, Failure>
where
    I: IntoIterator<Item = F>,
    F: Future<Output = T> + Send + 'a,
    T: Send + 'a,
{
    let deadline = Instant::now() + Duration::from_millis(budget_ms);
    let signal = Arc::new(ParkSignal::default());
    // One waker per full drive: waking any future re-opens the round.
    let waker = Waker::from(Arc::clone(&signal));
    let mut cx = Context::from_waker(&waker);
    let mut outputs: Vec<Option<T>> = Vec::new();
    let mut working: Vec<(usize, Boxed<'a, T>)> = Vec::new();
    for (index, future) in futures.into_iter().enumerate() {
        outputs.push(None);
        working.push((
            index,
            Box::pin(future) as Pin<Box<dyn Future<Output = T> + Send + 'a>>,
        ));
    }

    while !working.is_empty() {
        // A fresh round: forget earlier wake flags, then give every future one try.
        *signal.0.lock().unwrap_or_else(PoisonError::into_inner) = false;
        let mut pending: Vec<(usize, Boxed<'a, T>)> = Vec::new();
        for (index, mut future) in working {
            match future.as_mut().poll(&mut cx) {
                Poll::Ready(value) => outputs[index] = Some(value),
                Poll::Pending => pending.push((index, future)),
            }
        }
        working = pending;
        if working.is_empty() {
            break;
        }
        if !park_one(&signal, deadline) {
            break; // the budget is out: report it below
        }
    }

    if working.is_empty() {
        let mut finished: Vec<T> = Vec::with_capacity(outputs.len());
        for slot in outputs {
            match slot {
                Some(value) => finished.push(value),
                None => return Err(Failure("a driven future produced no output".to_owned())),
            }
        }
        Ok(finished)
    } else {
        Err(Failure(format!(
            "driving {} futures did not finish within {budget_ms} ms",
            working.len()
        )))
    }
}

/// Calls [`FileStore::put`], turning any store error into the case's [`Failure`] via a
/// plain `panic!` — cases avoid `unwrap`/`expect` noise.
async fn put_stored(
    store: &dyn FileStore,
    site: &SiteId,
    key: FileKey,
    body: Body,
    content_type: &str,
) -> StoredFile {
    match store
        .put(
            site,
            key,
            body,
            PutOptions::new(content_type, TEST_LIMIT, true),
        )
        .await
    {
        Ok(stored) => stored,
        Err(e) => panic!("put failed: {e}"),
    }
}

/// Reads a [`Body`] to the end, failing if it streams more than [`TEST_LIMIT`] bytes —
/// the one error a put is allowed to produce past `max_bytes`.
async fn read_all(mut body: Body) -> Result<Vec<u8>, Failure> {
    let mut bytes = Vec::new();
    let mut buf = vec![0u8; 8 * 1024];
    loop {
        let n = match body.read(&mut buf).await {
            Ok(0) => return Ok(bytes),
            Ok(n) => n,
            Err(e) => return Err(Failure(format!("reading a stored body failed: {e}"))),
        };
        bytes.extend_from_slice(&buf[..n]);
        if bytes.len() as u64 > TEST_LIMIT {
            return Err(Failure(
                "a body streamed more than one max_bytes limit — put stores nothing past the limit"
                    .to_owned(),
            ));
        }
    }
}

/// The digest every provider must report for `bytes`.
fn sha256_of(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// Runs every case against one `store` (freshly empty for the suite's keys) and returns
/// the outcomes; assert with [`Report::assert_all_passed`].
pub async fn run_all(store: &dyn FileStore) -> Report {
    let site = match SiteId::new(SUITE_SITE) {
        Ok(site) => site,
        Err(e) => panic!("the suite's own site id {SUITE_SITE:?} is invalid: {e}"),
    };
    let mut outcomes: Vec<Outcome> = Vec::new();
    macro_rules! case {
        ($($name:ident),+ $(,)?) => {
            $(outcomes.push(Outcome { case: stringify!($name), result: $name(store, &site).await });)+
        };
    }
    case!(
        put_get_roundtrip,
        put_sizes_and_overwrites,
        put_empty_body,
        list_paging_and_order,
        delete_semantics,
        site_isolation,
        keys_that_are_directories_coexist,
        concurrent_puts_one_version,
    );
    Report { outcomes }
}

/// `put` reports what it stored; `get` returns the same metadata and exact bytes;
/// `head` agrees with `get`; `delete` removes it, then the key reads as
/// `files.not_found` and heads as `None` — all on both visibilities.
async fn put_get_roundtrip(store: &dyn FileStore, site: &SiteId) -> CaseResult {
    let payload: Vec<u8> = (0..=255u8).cycle().take(100_000).collect();
    let content_type = "application/octet-stream";
    let mut stored_by: Vec<StoredFile> = Vec::new();
    for visibility in [Visibility::Private, Visibility::Public] {
        let key = FileKey::new(visibility, "contract/roundtrip/payload.bin")?;
        let body = body_from_vec(payload.clone());
        let stored = put_stored(store, site, key.clone(), body, content_type).await;
        assert_eq!(stored.size, payload.len() as u64, "put's size");
        assert_eq!(
            stored.content_type.as_str(),
            content_type,
            "put's content type"
        );
        assert_eq!(stored.key, key, "put reported a different key");
        stored_by.push(stored);
    }

    for (index, visibility) in [Visibility::Private, Visibility::Public]
        .into_iter()
        .enumerate()
    {
        let stored = &stored_by[index];
        let key = FileKey::new(visibility, "contract/roundtrip/payload.bin")?;
        assert_eq!(stored.sha256, sha256_of(&payload), "put's digest");

        let (meta, body) = store.get(site, key.clone()).await?;
        assert_eq!(meta.size, payload.len() as u64, "get's size");
        assert_eq!(meta.sha256, sha256_of(&payload), "get's digest");
        assert_eq!(meta.content_type, content_type, "get's content type");
        assert_eq!(meta.key, key, "get's key");
        let bytes = read_all(body).await?;
        assert_eq!(
            bytes.as_slice(),
            payload.as_slice(),
            "get must stream the stored bytes back"
        );

        let head = store.head(site, key.clone()).await?;
        assert_eq!(head, Some(meta), "head and get must not disagree");
    }

    for visibility in [Visibility::Private, Visibility::Public] {
        let key = FileKey::new(visibility, "contract/roundtrip/payload.bin")?;
        assert!(store.delete(site, key.clone()).await?);
        assert!(
            !store.delete(site, key.clone()).await?,
            "a second delete is `false`"
        );
        let gone = match store.get(site, key.clone()).await {
            Err(e) => e,
            Ok(_) => panic!("a deleted key must fail to get"),
        };
        assert_eq!(gone.code(), "files.not_found", "the deleted key's error");
        assert!(
            store.head(site, key).await?.is_none(),
            "head of a deleted key must be None"
        );
    }
    Ok(())
}

/// `max_bytes` enforcement at the exact boundary, `TooLarge` leaves nothing behind,
/// `overwrite: false` fails with `files.already_exists` and keeps the old bytes, and
/// `overwrite: true` replaces them.
async fn put_sizes_and_overwrites(store: &dyn FileStore, site: &SiteId) -> CaseResult {
    let boundary = FileKey::new(Visibility::Private, "contract/sizes/boundary.bin")?;
    let exact = vec![42u8; TEST_LIMIT as usize];
    put_stored(
        store,
        site,
        boundary.clone(),
        body_from_vec(exact.clone()),
        "application/octet-stream",
    )
    .await;

    let over = FileKey::new(Visibility::Private, "contract/sizes/over.bin")?;
    let one_too_many = vec![7u8; TEST_LIMIT as usize + 1];
    let too_big = match store
        .put(
            site,
            over.clone(),
            body_from_vec(one_too_many),
            PutOptions::new("application/octet-stream", TEST_LIMIT, true),
        )
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("max_bytes + 1 must fail with files.too_large"),
    };
    assert_eq!(too_big.code(), "files.too_large");
    assert!(
        store.head(site, over).await?.is_none(),
        "a failed TooLarge put leaves nothing behind"
    );

    // overwrite = false keeps the old version in place after a clash.
    let shared = FileKey::new(Visibility::Private, "contract/sizes/overwritten.txt")?;
    put_stored(
        store,
        site,
        shared.clone(),
        body_from_str("original"),
        "text/plain",
    )
    .await;
    let clash = match store
        .put(
            site,
            shared.clone(),
            body_from_str("clash"),
            PutOptions::new("text/plain", TEST_LIMIT, false),
        )
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("overwrite = false on an existing key must fail"),
    };
    assert_eq!(clash.code(), "files.already_exists");
    let (meta, body) = store.get(site, shared.clone()).await?;
    assert_eq!(
        meta.sha256,
        sha256_of(b"original"),
        "the clash kept the old version"
    );
    assert_eq!(read_all(body).await?.as_slice(), b"original");

    // overwrite = true replaces.
    let replacement = "replacement, much longer than the original version";
    put_stored(
        store,
        site,
        shared.clone(),
        body_from_str(replacement),
        "text/plain",
    )
    .await;
    let (meta, body) = store.get(site, shared.clone()).await?;
    assert_eq!(meta.sha256, sha256_of(replacement.as_bytes()));
    assert_eq!(read_all(body).await?.as_slice(), replacement.as_bytes());

    assert!(store.delete(site, boundary).await?);
    assert!(store.delete(site, shared).await?);
    Ok(())
}

/// A zero-length body is stored and streams back exactly nothing.
async fn put_empty_body(store: &dyn FileStore, site: &SiteId) -> CaseResult {
    let key = FileKey::new(Visibility::Private, "contract/empty/zero.bin")?;
    let stored = put_stored(
        store,
        site,
        key.clone(),
        body_from_vec(Vec::new()),
        "application/x-empty",
    )
    .await;
    assert_eq!(stored.size, 0);
    assert_eq!(stored.sha256, sha256_of(&[]));
    let (meta, body) = store.get(site, key).await?;
    assert_eq!(meta.size, 0);
    assert_eq!(meta.content_type, "application/x-empty");
    assert_eq!(read_all(body).await?.as_slice(), &[] as &[u8]);
    Ok(())
}

/// `list` filters to the prefix (never a partial name), comes out in key byte order,
/// pages strictly after `after` under the limit, and lists the whole site under the
/// `all` prefix; other visibilities and sibling names stay out.
async fn list_paging_and_order(store: &dyn FileStore, site: &SiteId) -> CaseResult {
    // `-` sorts before `.` sorts before `/` at the first differing byte, so the expected
    // order is exactly contract/list/a-1.bin, contract/list/a.bin, contract/list/a/z.bin.
    let created: [(Visibility, &str); 5] = [
        (Visibility::Private, "contract/list/a-1.bin"),
        (Visibility::Private, "contract/list/a.bin"),
        (Visibility::Private, "contract/list/a/z.bin"),
        (Visibility::Public, "contract/list/public.bin"),
        (Visibility::Private, "contract/loose.bin"),
    ];
    for (kind, rest) in created {
        let key = FileKey::new(kind, rest)?;
        put_stored(store, site, key, body_from_str("listed"), "text/plain").await;
    }

    let prefix = FileKeyPrefix::new("private/contract/list")?;
    let listed = store.list(site, &prefix, None, 10).await?;
    let names: Vec<String> = listed.iter().map(|m| m.key.rest().to_owned()).collect();
    assert_eq!(
        names.len(),
        3,
        "exactly the three private/list keys, not {names:?}"
    );
    for (name, want) in names.iter().zip([
        "contract/list/a-1.bin",
        "contract/list/a.bin",
        "contract/list/a/z.bin",
    ]) {
        assert_eq!(name.as_str(), want, "list must come out in key byte order");
    }

    // Strictly-after paging, one at a time, covers the same order without repeats.
    let mut paged: Vec<String> = Vec::new();
    let mut after: Option<FileKey> = None;
    loop {
        let batch = store.list(site, &prefix, after.as_ref(), 1).await?;
        let Some(meta) = batch.into_iter().next() else {
            break;
        };
        paged.push(meta.key.rest().to_owned());
        after = Some(meta.key);
    }
    assert_eq!(
        paged, names,
        "paging after the previous key must cover the whole prefix"
    );

    // An empty prefix lists the whole site — every key of the suite so far plus this
    // case's public one.
    let everything = store
        .list(site, &FileKeyPrefix::all(), None, usize::MAX)
        .await?;
    assert!(
        everything.len() >= 5,
        "an empty prefix lists the whole site"
    );

    // Tidy up so later cases see only their own keys.
    for (kind, rest) in created {
        assert!(store.delete(site, FileKey::new(kind, rest)?).await?);
    }
    Ok(())
}

/// Deleting a missing key is `false`; deleting a nested key makes it gone for good.
async fn delete_semantics(store: &dyn FileStore, site: &SiteId) -> CaseResult {
    let missing = FileKey::new(Visibility::Private, "contract/never/put.bin")?;
    assert!(
        !store.delete(site, missing).await?,
        "deleting a missing key reported true"
    );

    let nested = FileKey::new(Visibility::Private, "contract/cleanup/nested/deep/gone.bin")?;
    put_stored(
        store,
        site,
        nested.clone(),
        body_from_str("soon gone"),
        "text/plain",
    )
    .await;
    assert!(store.delete(site, nested.clone()).await?);
    let gone = match store.get(site, nested).await {
        Err(e) => e,
        Ok(_) => panic!("a deleted key must fail to get"),
    };
    assert_eq!(gone.code(), "files.not_found");
    Ok(())
}

/// The same key path on two sites is two different objects: site B starts blind to
/// site A's file, holds its own twin under the same path, and deleting site B's twin
/// leaves site A untouched.
async fn site_isolation(store: &dyn FileStore, site: &SiteId) -> CaseResult {
    let other = match SiteId::new(SUITE_SITE_ALT) {
        Ok(site) => site,
        Err(e) => panic!("the suite's alt site id {SUITE_SITE_ALT:?} is invalid: {e}"),
    };
    let key = FileKey::new(Visibility::Private, "contract/isolated/shared.txt")?;
    put_stored(
        store,
        site,
        key.clone(),
        body_from_str("site A"),
        "text/plain",
    )
    .await;

    match store.get(&other, key.clone()).await {
        Err(FileError::NotFound { .. }) => {}
        Err(e) => panic!(
            "site B must report files.not_found, got [{}]: {e}",
            e.code()
        ),
        Ok(_) => panic!("site B must not see site A's file"),
    }
    let listed = store.list(&other, &FileKeyPrefix::all(), None, 10).await?;
    assert!(
        listed.is_empty(),
        "site B listed {} key(s) of site A",
        listed.len()
    );

    // The same key path on site B is a separate object.
    put_stored(
        store,
        &other,
        key.clone(),
        body_from_str("site B"),
        "text/plain",
    )
    .await;
    let (_, body_a) = store.get(site, key.clone()).await?;
    assert_eq!(read_all(body_a).await?.as_slice(), b"site A");
    let (_, body_b) = store.get(&other, key.clone()).await?;
    assert_eq!(read_all(body_b).await?.as_slice(), b"site B");

    // Deleting on site B leaves site A untouched.
    assert!(store.delete(&other, key.clone()).await?);
    assert!(
        store.head(site, key.clone()).await?.is_some(),
        "site A lost its file"
    );

    // Site B is empty again; tidy site A too.
    assert!(store.delete(site, key).await?);
    Ok(())
}

/// A key can be a directory of another key, as in object stores (the brief; lead review of
/// #423): `a`, `a/b` and `a/b/c` coexist in either birth order, each reads back exactly its
/// own bytes, `list` returns all three in byte order, and deleting `a/b` leaves `a` and
/// `a/b/c` readable.
async fn keys_that_are_directories_coexist(store: &dyn FileStore, site: &SiteId) -> CaseResult {
    // Leaf first, then parent, then grandparent: the order that breaks a plain-directory
    // layout (the nested keys needed directories; then a file with the directory's name
    // has to appear next to them).
    let created: [(Visibility, &str); 3] = [
        (Visibility::Private, "contract/coexist/a/b/c"),
        (Visibility::Private, "contract/coexist/a/b"),
        (Visibility::Private, "contract/coexist/a"),
    ];
    let payloads = ["the leaf a/b/c", "the middle a/b", "the parent a"];
    let mut stored = Vec::new();
    for ((kind, rest), bytes) in created.iter().zip(payloads.iter()) {
        let key = FileKey::new(*kind, rest)?;
        stored.push((
            key.clone(),
            put_stored(store, site, key, body_from_str(bytes), "text/plain").await,
        ));
    }

    // Every key reads back exactly its own bytes, never a sibling's.
    for (index, (key, _)) in stored.iter().enumerate() {
        let (_, body) = store.get(site, key.clone()).await?;
        assert_eq!(
            read_all(body).await?.as_slice(),
            payloads[index].as_bytes(),
            "key {} must hold its own bytes",
            key.rest()
        );
    }

    // Byte order regardless of birth order: the sorted keys, not the put sequence.
    let prefix = FileKeyPrefix::new("private/contract/coexist")?;
    let listed = store.list(site, &prefix, None, 10).await?;
    let names: Vec<&str> = listed.iter().map(|m| m.key.rest()).collect();
    assert_eq!(
        names,
        [
            "contract/coexist/a",
            "contract/coexist/a/b",
            "contract/coexist/a/b/c"
        ],
        "all three coexisting keys, in key byte order"
    );

    // Deleting the middle key leaves both of its neighbours readable, and its own key is
    // gone for good.
    let removed = FileKey::new(Visibility::Private, "contract/coexist/a/b")?;
    assert!(store.delete(site, removed.clone()).await?);
    assert!(
        store.head(site, removed).await?.is_none(),
        "the deleted middle key must be gone"
    );
    for (index, (key, _)) in stored.iter().enumerate() {
        if index == 1 {
            continue;
        }
        let (_, body) = store.get(site, key.clone()).await?;
        assert_eq!(
            read_all(body).await?.as_slice(),
            payloads[index].as_bytes(),
            "key {} must have survived its neighbour's delete",
            key.rest()
        );
    }
    let after = store.list(site, &prefix, None, 10).await?;
    let rest_after: Vec<&str> = after.iter().map(|m| m.key.rest()).collect();
    assert_eq!(
        rest_after,
        ["contract/coexist/a", "contract/coexist/a/b/c"],
        "the remaining keys, in key byte order"
    );

    // Tidy so later cases see only their own keys.
    for (index, (key, _)) in stored.iter().enumerate() {
        if index == 1 {
            continue;
        }
        assert!(store.delete(site, key.clone()).await?);
    }
    Ok(())
}

/// Six overlapping puts to one key end in one complete version — one writer's exact
/// bytes (digest included), never a mix: the streaming contract plus swap-in commits
/// make partial writes unobservable. The puts are driven with
/// [`drive_concurrently`], so any runtime that can poll them will do.
async fn concurrent_puts_one_version(store: &dyn FileStore, site: &SiteId) -> CaseResult {
    let key = FileKey::new(Visibility::Private, "contract/races/one.txt")?;
    let payloads: Vec<Vec<u8>> = (0..6)
        .map(|writer| vec![b'a' + writer as u8; 4096])
        .collect();
    let futures: Vec<_> = payloads
        .iter()
        .map(|bytes| {
            store.put(
                site,
                key.clone(),
                body_from_vec(bytes.clone()),
                PutOptions::new("text/plain", TEST_LIMIT, true),
            )
        })
        .collect();
    let results: Vec<FileResult<StoredFile>> = drive_concurrently(futures, 30_000)?;
    for (writer, result) in results.into_iter().enumerate() {
        match result {
            Ok(stored) => {
                assert_eq!(stored.size, 4096, "put #{writer}'s size");
                assert_eq!(
                    stored.sha256,
                    sha256_of(&payloads[writer]),
                    "put #{writer}'s digest"
                );
            }
            Err(e) => return Err(Failure(format!("concurrent put #{writer} failed: {e}"))),
        }
    }

    // The store holds exactly one complete version: one writer's bytes, and metadata
    // that agrees with them.
    let (meta, body) = store.get(site, key.clone()).await?;
    let bytes = read_all(body).await?;
    assert_eq!(
        bytes.len(),
        4096,
        "the final version is one writer's full payload"
    );
    let writer = payloads
        .iter()
        .position(|p| p.as_slice() == bytes.as_slice());
    assert!(
        writer.is_some(),
        "the stored bytes are neither writer's payload — a torn write"
    );
    if let Some(writer) = writer {
        assert_eq!(
            meta.sha256,
            sha256_of(&payloads[writer]),
            "the digest must match the stored version"
        );
    }
    assert_eq!(meta.size, 4096);
    assert_eq!(meta.content_type, "text/plain");

    // Exactly one entry under the raced prefix; after delete the key is gone.
    let prefix = FileKeyPrefix::new("private/contract/races")?;
    let listed = store.list(site, &prefix, None, 10).await?;
    assert_eq!(
        listed.len(),
        1,
        "exactly one version exists under the raced prefix"
    );
    assert_eq!(listed[0].key, key, "the listed key");
    assert!(store.delete(site, key).await?);
    Ok(())
}
