//! [`LocalFileStore`] against the contract suite plus the local-specific guarantees the
//! brief names: bytes survive a restart on the same root, an interrupted `put` leaves
//! nothing behind, normal use never leaves stray files, and a `get` racing a concurrent
//! overwrite always describes the very bytes it streams (each test runs on its own fresh
//! scratch root).

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use sha2::{Digest, Sha256};
use spot_core::SiteId;
use spot_files::contract_tests;
use spot_files::key::{FileKey, FileKeyPrefix, Visibility};
use spot_files::local::LocalFileStore;
use spot_files::store::{Body, FileStore, PutOptions, body_from_str, body_from_vec};
use tokio::io::{AsyncRead, AsyncReadExt, ReadBuf};

/// A fresh scratch root with its [`LocalFileStore`]; the returned [`tempfile::TempDir`]
/// stays alive in the test and removes the tree at the end.
#[track_caller]
fn scratch() -> (tempfile::TempDir, LocalFileStore) {
    match tempfile::tempdir() {
        Ok(dir) => {
            let root = dir.path().to_path_buf();
            (dir, LocalFileStore::new(root))
        }
        Err(e) => panic!("creating a scratch directory: {e}"),
    }
}

/// A [`SiteId`], failing the test on a construction mistake.
#[track_caller]
fn built_site(text: &str) -> SiteId {
    match SiteId::new(text) {
        Ok(site) => site,
        Err(e) => panic!("site id {text:?} is invalid: {e}"),
    }
}

/// A [`FileKey`], failing the test on a construction mistake.
#[track_caller]
fn built_key(rest: &str) -> FileKey {
    match FileKey::new(Visibility::Private, rest) {
        Ok(key) => key,
        Err(e) => panic!("key {rest:?} is invalid: {e}"),
    }
}

/// The contract suite on a multi-thread runtime: the local provider has blocking work,
/// and at least one case interleaves several of those.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_contract_suite() {
    let (_scratch, store) = scratch();
    contract_tests::run_all(&store).await.assert_all_passed();
}

/// An upload client that dies mid-stream: serves its `remaining` bytes of `byte`, then
/// IO-errors forever.
struct ExplodingRead {
    /// Bytes still to serve before dying.
    remaining: usize,
    /// The byte every served chunk contains.
    byte: u8,
}

impl AsyncRead for ExplodingRead {
    fn poll_read(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if this.remaining == 0 {
            return Poll::Ready(Err(std::io::Error::other("the upload stream died")));
        }
        let chunk = this.remaining.min(buf.remaining());
        let served = vec![this.byte; chunk];
        buf.put_slice(&served);
        this.remaining -= chunk;
        Poll::Ready(Ok(()))
    }
}

/// A `put` whose stream dies midway fails with `files.io`, the key stays absent, and
/// neither content nor sidecar nor temp file survives on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_interrupted_put_leaves_nothing_behind() {
    let (scratch, store) = scratch();
    let site = built_site("interrupt.test");
    let key = built_key("contract/partial/gone.bin");

    let body: Body = Box::pin(ExplodingRead {
        remaining: 3,
        byte: b'x',
    });
    let interrupt = match store
        .put(
            &site,
            key.clone(),
            body,
            PutOptions::new("text/plain", 4096, true),
        )
        .await
    {
        Err(e) => e,
        Ok(_) => panic!("a dying stream must fail the put, not store its bytes"),
    };
    assert_eq!(
        interrupt.code(),
        "files.io",
        "a dying stream is an IO failure"
    );

    // The key stays absent — three bytes are not a version.
    match store.get(&site, key).await {
        Err(e) => assert_eq!(e.code(), "files.not_found", "no partial version: {e}"),
        Ok(_) => panic!("an interrupted put must not have stored the object"),
    }

    // And the root holds no files at all: the temp sink was discarded.
    let files = walk_files(scratch.path()).await;
    assert!(files.is_empty(), "an interrupted put left {files:?} behind");
}

/// After normal use — puts, a `head`/`list`, then deletes across nested keys — the root
/// holds no files at all: deletes are complete and the prune emptied the tree.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_use_leaves_no_strays() {
    let (scratch, store) = scratch();
    let site = built_site("strays.test");
    let keys = [
        "contract/strays/one.bin",
        "contract/strays/two.txt",
        "contract/strays/nested/deep/three.bin",
    ];
    for rest in keys {
        match store
            .put(
                &site,
                built_key(rest),
                body_from_str("stray check"),
                PutOptions::new("text/plain", 4096, true),
            )
            .await
        {
            Ok(_) => {}
            Err(e) => panic!("put {rest}: {e}"),
        }
    }
    // The keys are there — exactly three under the prefix.
    let prefix = match FileKeyPrefix::new("private/contract/strays") {
        Ok(prefix) => prefix,
        Err(e) => panic!("prefix: {e}"),
    };
    let listed = match store.list(&site, &prefix, None, 10).await {
        Ok(listed) => listed,
        Err(e) => panic!("list: {e}"),
    };
    assert_eq!(listed.len(), 3, "all three keys must be under the prefix");
    for rest in keys {
        match store.delete(&site, built_key(rest)).await {
            Ok(deleted) => assert!(deleted, "the key {rest} was just put"),
            Err(e) => panic!("delete {rest}: {e}"),
        }
    }
    let listed = match store.list(&site, &prefix, None, 10).await {
        Ok(listed) => listed,
        Err(e) => panic!("list after deleting: {e}"),
    };
    assert!(
        listed.is_empty(),
        "listed after deleting everything: {listed:?}"
    );

    let files = walk_files(scratch.path()).await;
    assert!(
        files.is_empty(),
        "normal use left {files:?} behind — deletes must be complete"
    );
}

/// A new store opened on the same root reads what the previous one stored — restarts
/// must not lose bytes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stored_files_survive_a_restart() {
    let (scratch, first) = scratch();
    let site = built_site("restart.test");
    let key = built_key("2026/09/invoice.pdf");
    let payload = b"bytes that outlive the store process".to_vec();
    let stored = match first
        .put(
            &site,
            key.clone(),
            body_from_vec(payload.clone()),
            PutOptions::new("application/pdf", 4096, true),
        )
        .await
    {
        Ok(stored) => stored,
        Err(e) => panic!("put on a fresh store: {e}"),
    };
    drop(first);

    let second = LocalFileStore::new(scratch.path().to_path_buf());
    let (meta, mut body) = match second.get(&site, key).await {
        Ok(pair) => pair,
        Err(e) => panic!("get after restart: {e}"),
    };
    assert_eq!(meta.size, stored.size, "size after restart");
    assert_eq!(meta.sha256, stored.sha256, "digest after restart");
    assert_eq!(meta.content_type, "application/pdf");
    let mut back = Vec::new();
    match body.read_to_end(&mut back).await {
        Ok(_) => {}
        Err(e) => panic!("reading the stored bytes: {e}"),
    }
    assert_eq!(back, payload);
}

/// Rounds the race probe runs the writer/reader pair: enough iterations that a two-open
/// `get` (metadata from one open, body from a later one) reliably trips.
const RACE_ROUNDS: usize = 200;

/// One key, one writer overwriting it back and forth between a small and a large payload,
/// one reader opening it again and again: every `get` must describe exactly the bytes its
/// body streams — size and digest from the same open handle as the body (lead review of
/// #423). A reader that opens twice can take the size/digest from the bytes before an
/// overwrite commits and the body from the bytes after; this probe exists to make that
/// mismatch fail loudly instead of flickering in production.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_get_racing_an_overwrite_never_mixes_metadata_with_bytes() {
    let (_scratch, store) = scratch();
    let site = built_site("race.test");
    let key = built_key("contract/race/subject.bin");
    let payload_small = vec![b'a'; 512];
    let payload_large = vec![b'b'; 16 * 1024];
    let digest_small: [u8; 32] = Sha256::digest(&payload_small).into();
    let digest_large: [u8; 32] = Sha256::digest(&payload_large).into();

    // Stage one version first: from here on the object always exists (the writer only
    // ever overwrites, and the rename commit keeps one whole version at a time), so the
    // reader can expect every get to succeed.
    if let Err(e) = store
        .put(
            &site,
            key.clone(),
            body_from_vec(payload_small.clone()),
            PutOptions::new("application/x-probe", 64 * 1024, true),
        )
        .await
    {
        panic!("staging the first version: {e}");
    }

    let writer = {
        let store = store.clone();
        let site = site.clone();
        let key = key.clone();
        tokio::spawn(async move {
            for round in 0..RACE_ROUNDS {
                let (payload, kind) = if round % 2 == 0 {
                    (payload_small.as_slice(), "small")
                } else {
                    (payload_large.as_slice(), "large")
                };
                if let Err(e) = store
                    .put(
                        &site,
                        key.clone(),
                        body_from_vec(payload.to_vec()),
                        PutOptions::new("application/x-probe", 64 * 1024, true),
                    )
                    .await
                {
                    panic!("writer put round {round} ({kind}): {e}");
                }
            }
        })
    };

    let reader = {
        let site = site.clone();
        let key = key.clone();
        tokio::spawn(async move {
            for round in 0..RACE_ROUNDS {
                let (meta, mut body) = match store.get(&site, key.clone()).await {
                    Ok(pair) => pair,
                    Err(e) => panic!("reader get round {round} while the writer hammers: {e}"),
                };
                let mut bytes = Vec::new();
                if let Err(e) = body.read_to_end(&mut bytes).await {
                    panic!("reader read round {round}: {e}");
                }
                assert_eq!(
                    meta.size as usize,
                    bytes.len(),
                    "round {round}: size does not describe the streamed body"
                );
                let body_digest: [u8; 32] = Sha256::digest(&bytes).into();
                assert_eq!(
                    meta.sha256, body_digest,
                    "round {round}: digest does not describe the streamed body"
                );
                // The rename is atomic, so the store holds one whole version at a time;
                // anything else means a torn read slipped through.
                assert!(
                    meta.sha256 == digest_small || meta.sha256 == digest_large,
                    "round {round}: neither staged payload came back ({}/{})",
                    meta.size,
                    meta.content_type
                );
                assert_eq!(
                    meta.content_type.as_str(),
                    "application/x-probe",
                    "round {round}: a torn sidecar slipped through"
                );
            }
        })
    };

    // Either task panicking must fail the test; the reader panics first on any mismatch.
    let (writer_out, reader_out) = tokio::join!(writer, reader);
    for (kind, out) in [("writer", writer_out), ("reader", reader_out)] {
        if let Err(e) = out {
            panic!("the {kind} racer panicked: {e}");
        }
    }
}

/// Walks `root` recursively and returns every file path. Best effort over unreadable
/// entries: the tree is ours to read.
async fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut todo = vec![root.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let mut entries = match tokio::fs::read_dir(&dir).await {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if entry.file_type().await.is_ok_and(|kind| kind.is_dir()) {
                todo.push(path);
            } else {
                files.push(path);
            }
        }
    }
    files
}
