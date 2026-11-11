//! The backend-independent contract suite for [`JournalStore`].
//!
//! Every backend runs [`run`]: the in-memory store, the folder store, and the providers added
//! later. A failure is a plain message naming the rule that does not hold, so a new provider's
//! first test run says what to fix rather than where to look.
//!
//! `make` builds a store per call, so the suite isolates the concurrency case from the earlier
//! ones; for a directory backend every call may return a fresh handle on the same directory,
//! because the race the suite checks is the one on the storage, not on the handle.

use std::future::Future;
use std::task::{Context, Poll, Waker};

use crate::error::StoreError;
use crate::layout::{Key, Layout};
use crate::store::{JournalStore, etag};

/// The tenant the suite writes under. A store may be shared with other tests, so the suite never
/// assumes it starts empty — it only assumes this tenant's keys are its own.
pub const TENANT: &str = "contract";

/// How many writers race for one key in the concurrency case.
const RACERS: u8 = 4;

/// How many times a store future may report `Pending` before the suite calls it stuck.
const POLLS: usize = 10_000;

/// Runs the suite.
///
/// # Errors
/// The rule that does not hold, as one sentence.
pub async fn run<S, F>(make: F) -> Result<(), String>
where
    S: JournalStore,
    F: Fn() -> S,
{
    let store = make();
    let root = format!("{TENANT}/");

    // An unwritten key is absent, not an error.
    let manifest = key(Layout::manifest(TENANT, 1))?;
    check(
        store.get(&manifest).await.map_err(failed)?.is_none(),
        "an unwritten key reads as None",
    )?;
    check(
        store.head(&manifest).await.map_err(failed)?.is_none(),
        "an unwritten key has no metadata",
    )?;

    // A write is visible with the etag and size of its bytes.
    let first = b"the first manifest of generation one".to_vec();
    let written = store
        .put_new(&manifest, first.clone())
        .await
        .map_err(failed)?;
    check(
        written.etag == etag(&first),
        "put_new reports the content etag (lower-case hex SHA-256)",
    )?;
    check(
        written.size == u64::try_from(first.len()).unwrap_or(u64::MAX),
        "put_new reports the byte length",
    )?;
    let read = store
        .get(&manifest)
        .await
        .map_err(failed)?
        .ok_or("a written key reads back")?;
    check(read.bytes == first, "get returns exactly the bytes written")?;
    check(
        read.meta.etag == written.etag && read.meta.size == written.size,
        "get reports the etag and size put_new reported",
    )?;
    check(
        store.head(&manifest).await.map_err(failed)? == Some(written.clone()),
        "head reports what put_new reported",
    )?;

    // A second put_new of the same key is refused and changes nothing.
    match store
        .put_new(&manifest, b"a second manifest".to_vec())
        .await
    {
        Err(StoreError::AlreadyExists { key: named }) => {
            check(named == manifest, "AlreadyExists names the key it refused")?
        }
        other => {
            return Err(format!(
                "put_new of a taken key must fail with AlreadyExists, not {other:?}"
            ));
        }
    }
    check(
        store.get(&manifest).await.map_err(failed)?.map(|o| o.bytes) == Some(first.clone()),
        "a refused put_new writes nothing",
    )?;

    // The manifest is replaced only through the etag guard.
    let second = b"the second manifest of generation one".to_vec();
    match store
        .put_replace(&manifest, second.clone(), "not-the-etag")
        .await
    {
        Err(StoreError::EtagMismatch { .. }) => {}
        other => {
            return Err(format!(
                "put_replace with a stale etag must fail with EtagMismatch, not {other:?}"
            ));
        }
    }
    check(
        store.get(&manifest).await.map_err(failed)?.map(|o| o.bytes) == Some(first.clone()),
        "a refused put_replace writes nothing",
    )?;
    let replaced = store
        .put_replace(&manifest, second.clone(), &written.etag)
        .await
        .map_err(failed)?;
    check(
        replaced.etag == etag(&second) && replaced.etag != written.etag,
        "a guarded replace reports the etag of the new bytes",
    )?;
    check(
        store.get(&manifest).await.map_err(failed)?.map(|o| o.bytes) == Some(second.clone()),
        "a guarded replace stores the new bytes",
    )?;
    let absent = key(Layout::manifest(TENANT, 2))?;
    match store
        .put_replace(&absent, b"nothing here".to_vec(), "e")
        .await
    {
        Err(StoreError::NotFound { .. }) => {}
        other => {
            return Err(format!(
                "put_replace of a key that does not exist must fail with NotFound, not {other:?}"
            ));
        }
    }

    // Everything that is not a manifest is written once, and a replace of it is refused.
    let segment = key(Layout::segment(TENANT, 1, 1, 2))?;
    let segment_bytes = b"one sealed segment".to_vec();
    store
        .put_new(&segment, segment_bytes.clone())
        .await
        .map_err(failed)?;
    match store
        .put_replace(&segment, b"rewritten".to_vec(), &etag(&segment_bytes))
        .await
    {
        Err(StoreError::ReplaceRefused { .. }) => {}
        other => {
            return Err(format!(
                "put_replace of a segment must fail with ReplaceRefused, not {other:?}"
            ));
        }
    }
    check(
        store.get(&segment).await.map_err(failed)?.map(|o| o.bytes) == Some(segment_bytes.clone()),
        "a refused replace leaves the segment as it was",
    )?;

    // Listing is ordered, paged by key, and does not cross the prefix.
    let other_segment = key(Layout::segment(TENANT, 1, 3, 3))?;
    store
        .put_new(&other_segment, b"another sealed segment".to_vec())
        .await
        .map_err(failed)?;
    let tail_prefix = format!("{TENANT}/tail/{}", Layout::padded(1));
    let mut chunks = Vec::new();
    for batch in 1..=3 {
        chunks.push(key(Layout::tail_chunk(TENANT, 1, batch))?);
    }
    for chunk in &chunks {
        store
            .put_new(chunk, format!("chunk {chunk}").into_bytes())
            .await
            .map_err(failed)?;
    }
    let page_one = store.list(&tail_prefix, None, 2).await.map_err(failed)?;
    check(
        page_one.len() == 2 && page_one[0].key == chunks[0] && page_one[1].key == chunks[1],
        "a listing is ascending by key and honours the limit",
    )?;
    let page_two = store
        .list(&tail_prefix, Some(&page_one[1].key), 2)
        .await
        .map_err(failed)?;
    check(
        page_two.len() == 1 && page_two[0].key == chunks[2],
        "a listing resumes strictly after the last key of the page",
    )?;
    check(
        store
            .list(&tail_prefix, Some(&chunks[2]), 2)
            .await
            .map_err(failed)?
            .is_empty(),
        "a listing past the last key is empty",
    )?;
    check(
        store
            .list(&tail_prefix, None, 0)
            .await
            .map_err(failed)?
            .is_empty(),
        "a limit of zero returns no object",
    )?;
    let everything = store.list(&root, None, 100).await.map_err(failed)?;
    check(
        everything.len() >= 6,
        "a tenant listing holds the manifest, both segments and the three tail chunks",
    )?;
    check(
        everything.iter().all(|entry| entry.key.starts_with(&root)),
        "a tenant listing never leaves the tenant prefix",
    )?;
    let mut sorted = everything.clone();
    sorted.sort_by(|left, right| left.key.cmp(&right.key));
    check(sorted == everything, "a tenant listing is ascending by key")?;
    check(
        store
            .list(&format!("{TENANT}/manifest/"), None, 10)
            .await
            .map_err(failed)?
            .iter()
            .all(|entry| entry.key.starts_with(&format!("{TENANT}/manifest/"))),
        "a listing honours its prefix",
    )?;

    // Deletion exists for tail chunks only.
    match store.delete(&segment).await {
        Err(StoreError::DeleteRefused { .. }) => {}
        other => {
            return Err(format!(
                "delete of a segment must fail with DeleteRefused, not {other:?}"
            ));
        }
    }
    check(
        store.head(&segment).await.map_err(failed)?.is_some(),
        "a refused delete leaves the segment in place",
    )?;
    let snapshot = key(Layout::snapshot(TENANT, 1, 1))?;
    let snapshot_object = key(Layout::snapshot_object(TENANT, 1, 1, "base-image"))?;
    store
        .put_new(&snapshot_object, b"a base image".to_vec())
        .await
        .map_err(failed)?;
    match store.delete(&snapshot_object).await {
        Err(StoreError::DeleteRefused { .. }) => {}
        other => {
            return Err(format!(
                "delete under a snapshot prefix must fail with DeleteRefused, not {other:?}"
            ));
        }
    }
    match store.delete(&snapshot).await {
        Err(StoreError::DeleteRefused { .. }) => {}
        other => {
            return Err(format!(
                "delete of a snapshot prefix must fail with DeleteRefused, not {other:?}"
            ));
        }
    }
    match store.delete(&key(Layout::tail_chunk(TENANT, 1, 9))?).await {
        Err(StoreError::NotFound { .. }) => {}
        other => {
            return Err(format!(
                "delete of a key that does not exist must fail with NotFound, not {other:?}"
            ));
        }
    }
    store.delete(&chunks[0]).await.map_err(failed)?;
    check(
        store.get(&chunks[0]).await.map_err(failed)?.is_none(),
        "a deleted tail chunk reads as None",
    )?;
    check(
        !store
            .list(&tail_prefix, None, 10)
            .await
            .map_err(failed)?
            .iter()
            .any(|entry| entry.key == chunks[0]),
        "a deleted tail chunk is gone from the listing",
    )?;

    // Keys outside the layout are refused before a backend is touched.
    let generation = Layout::padded(1);
    let foreign = [
        String::new(),
        "nonsense".to_owned(),
        format!("/{TENANT}/manifest/{generation}.json"),
        format!("../evil/manifest/{generation}.json"),
        format!("{TENANT}/manifest/1.json"),
        format!("{TENANT}/segments/{generation}/1-2.seg"),
        format!("{TENANT}/elsewhere/{generation}.json"),
    ];
    for raw in &foreign {
        let refusals = [
            store.put_new(raw, b"x".to_vec()).await.map(|_| ()),
            store.put_replace(raw, b"x".to_vec(), "e").await.map(|_| ()),
            store.get(raw).await.map(|_| ()),
        ];
        for refusal in refusals {
            match refusal {
                Err(StoreError::KeyInvalid(_)) => {}
                other => {
                    return Err(format!(
                        "the key {raw:?} must be refused with KeyInvalid, not {other:?}"
                    ));
                }
            }
        }
    }

    // Of several writers racing for one key, exactly one wins.
    let racing = make();
    let contested = key(Layout::segment(TENANT, 3, 1, 1))?;
    let outcomes = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for racer in 0..RACERS {
            let store = &racing;
            let contested = contested.clone();
            handles.push(scope.spawn(move || {
                let bytes = vec![racer; 16];
                let outcome = block_on_ready(store.put_new(&contested, bytes.clone()));
                (bytes, outcome)
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join())
            .collect::<Vec<_>>()
    });
    let mut winners = 0;
    let mut refused = 0;
    let mut winning: Option<(Vec<u8>, String)> = None;
    for joined in outcomes {
        let (bytes, outcome) = joined.map_err(|_| "a racing writer panicked".to_owned())?;
        match outcome {
            Ok(meta) => {
                winners += 1;
                winning = Some((bytes, meta.etag));
            }
            Err(StoreError::AlreadyExists { .. }) => refused += 1,
            Err(other) => return Err(format!("a racing put_new failed with {other:?}")),
        }
    }
    check(
        winners == 1,
        "of writers racing for one key exactly one wins",
    )?;
    check(
        refused == usize::from(RACERS) - 1,
        "every other racing writer is refused with AlreadyExists",
    )?;
    let (bytes, winner_etag) = winning.ok_or("the winner of the race reports its etag")?;
    check(
        etag(&bytes) == winner_etag,
        "the winner's etag is the content etag of the bytes it wrote",
    )?;
    check(
        stored_bytes(&racing, &contested).await? == bytes,
        "the contested key holds exactly the winner's bytes",
    )?;
    Ok(())
}

/// The stored bytes at `contested`.
async fn stored_bytes<S: JournalStore>(store: &S, contested: &str) -> Result<Vec<u8>, String> {
    store
        .get(contested)
        .await
        .map_err(failed)?
        .map(|object| object.bytes)
        .ok_or_else(|| "the contested key holds the winner's bytes".to_owned())
}

/// A built key, or the refusal as the suite's message.
fn key(built: Result<Key, StoreError>) -> Result<String, String> {
    built.map(|key| key.to_key()).map_err(failed)
}

fn check(ok: bool, rule: &str) -> Result<(), String> {
    if ok { Ok(()) } else { Err(rule.to_owned()) }
}

fn failed<E: std::fmt::Display>(error: E) -> String {
    format!("store error: {error}")
}

/// Drives one store future to completion on a no-op waker.
///
/// The store futures of this crate run to completion on their first poll (they do blocking
/// filesystem work inside), so the suite can step them from a plain thread without a runtime. A
/// provider whose futures need a real wakeup must not report `Pending` here; if it does, this
/// panics with that sentence instead of hanging, and the joined thread reports it.
fn block_on_ready<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::noop();
    let mut context = Context::from_waker(waker);
    for _ in 0..POLLS {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
    panic!("a store future stayed pending on a ready poll");
}
