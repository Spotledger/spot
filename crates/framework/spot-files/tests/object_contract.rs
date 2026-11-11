//! The provider contract suite against the real services: feature `s3` runs against
//! MinIO, feature `azure` runs against Azurite, both as the environment's
//! `SPOT_TEST_*` variables name them.
//!
//! Run through `tests/with-object-stores.sh cargo test -p spot-files --all-features`,
//! which starts the emulators, exports the variables and removes them again. With a
//! `SPOT_TEST_*` variable missing the test fails naming that script — these tests
//! never skip silently — and the workspace-wide `cargo test` does not reach them at
//! all, because it builds without the optional features.

#[cfg(any(feature = "s3", feature = "azure"))]
/// Turns a `Result` into its value for test assertions, panicking with context instead
/// of `unwrap`/`expect`, which the workspace's clippy denies.
fn expect_ok<T, E>(outcome: Result<T, E>, what: &str) -> T
where
    E: core::fmt::Debug,
{
    match outcome {
        Ok(value) => value,
        Err(error) => panic!("{what}: {error:?}"),
    }
}

/// The MinIO-backed store, built from the runner's `SPOT_TEST_S3_*` variables. A
/// missing variable fails naming the script — these tests never skip silently.
#[cfg(feature = "s3")]
fn s3_store_from_env() -> spot_files::ObjectFileStore {
    use spot_files::{ObjectFileStore, S3Config};

    let endpoint = std::env::var("SPOT_TEST_S3_ENDPOINT").unwrap_or_else(|_| {
        panic!(
            "SPOT_TEST_S3_ENDPOINT is missing — run through crates/framework/spot-files/tests/with-object-stores.sh"
        )
    });
    let bucket = std::env::var("SPOT_TEST_S3_BUCKET").unwrap_or_else(|_| {
        panic!(
            "SPOT_TEST_S3_BUCKET is missing — run through crates/framework/spot-files/tests/with-object-stores.sh"
        )
    });
    let key = std::env::var("SPOT_TEST_S3_KEY").unwrap_or_else(|_| {
        panic!(
            "SPOT_TEST_S3_KEY is missing — run through crates/framework/spot-files/tests/with-object-stores.sh"
        )
    });
    let secret = std::env::var("SPOT_TEST_S3_SECRET").unwrap_or_else(|_| {
        panic!(
            "SPOT_TEST_S3_SECRET is missing — run through crates/framework/spot-files/tests/with-object-stores.sh"
        )
    });
    let config = S3Config {
        endpoint: Some(expect_ok(
            url::Url::parse(&endpoint),
            "parsing SPOT_TEST_S3_ENDPOINT",
        )),
        // Emulators accept any region; an explicitly set one wins.
        region: Some(
            std::env::var("SPOT_TEST_S3_REGION").unwrap_or_else(|_| "us-east-1".to_owned()),
        ),
        bucket,
        access_key_id: Some(key),
        secret_access_key: Some(secret),
        allow_http: true,
        path_style: true,
    };
    expect_ok(ObjectFileStore::new_s3(config), "building the s3 store")
}

/// The full contract suite against MinIO, the S3-compatible emulator.
#[cfg(feature = "s3")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_contract_suite() {
    let store = s3_store_from_env();
    spot_files::contract_tests::run_all(&store)
        .await
        .assert_all_passed();
}

/// The multipart path against MinIO: round trips at both part boundaries and the
/// guarded create refusal (WP-PLT-10.T2 review item 3).
#[cfg(feature = "s3")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s3_multipart_round_trip_and_create_refusal() {
    let store = s3_store_from_env();
    multipart_round_trip_and_create_refusal(&store).await;
}

/// The Azurite-backed store, built from the runner's `SPOT_TEST_AZURE_*` variables. A
/// missing variable fails naming the script — these tests never skip silently.
#[cfg(feature = "azure")]
fn azure_store_from_env() -> spot_files::ObjectFileStore {
    use spot_files::{AzureConfig, AzureCredential, ObjectFileStore};

    let endpoint = std::env::var("SPOT_TEST_AZURE_ENDPOINT").unwrap_or_else(|_| {
        panic!(
            "SPOT_TEST_AZURE_ENDPOINT is missing — run through crates/framework/spot-files/tests/with-object-stores.sh"
        )
    });
    let account = std::env::var("SPOT_TEST_AZURE_ACCOUNT").unwrap_or_else(|_| {
        panic!(
            "SPOT_TEST_AZURE_ACCOUNT is missing — run through crates/framework/spot-files/tests/with-object-stores.sh"
        )
    });
    let key = std::env::var("SPOT_TEST_AZURE_KEY").unwrap_or_else(|_| {
        panic!(
            "SPOT_TEST_AZURE_KEY is missing — run through crates/framework/spot-files/tests/with-object-stores.sh"
        )
    });
    let container = std::env::var("SPOT_TEST_AZURE_CONTAINER").unwrap_or_else(|_| {
        panic!(
            "SPOT_TEST_AZURE_CONTAINER is missing — run through crates/framework/spot-files/tests/with-object-stores.sh"
        )
    });
    let config = AzureConfig {
        endpoint: Some(expect_ok(
            url::Url::parse(&endpoint),
            "parsing SPOT_TEST_AZURE_ENDPOINT",
        )),
        account,
        container,
        credentials: AzureCredential::AccessKey(key),
        allow_http: true,
    };
    expect_ok(
        ObjectFileStore::new_azure(config),
        "building the azure store",
    )
}

/// The full contract suite against Azurite, the Azure blob emulator, with the
/// well-known development account key.
#[cfg(feature = "azure")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn azure_contract_suite() {
    let store = azure_store_from_env();
    spot_files::contract_tests::run_all(&store)
        .await
        .assert_all_passed();
}

/// The multipart path against Azurite: round trips at both part boundaries and the
/// guarded create refusal (WP-PLT-10.T2 review item 3).
#[cfg(feature = "azure")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn azure_multipart_round_trip_and_create_refusal() {
    let store = azure_store_from_env();
    multipart_round_trip_and_create_refusal(&store).await;
}

/// The multipart scenario every emulated backend runs: a body past the single-put
/// limit round-trips at both part boundaries — one full part plus a one-byte
/// remainder, and two full parts plus a small remainder — with the put-time sha-256
/// digest and content type surviving in `get`, `head` and the listing; and a
/// conditional create over an existing large key is refused with the old object
/// untouched. The bodies are deterministic, so a rerun against a reused container
/// reproduces the same objects and cannot fail on stale data.
#[cfg(any(feature = "s3", feature = "azure"))]
async fn multipart_round_trip_and_create_refusal(store: &spot_files::ObjectFileStore) {
    use sha2::{Digest, Sha256};
    use spot_core::SiteId;
    use spot_files::error::FileError;
    use spot_files::key::{FileKey, FileKeyPrefix, Visibility};
    use spot_files::store::{FileStore, PutOptions, body_from_vec};
    use spot_files::{PART_SIZE, SINGLE_PUT_LIMIT};
    use tokio::io::AsyncReadExt;

    /// A deterministic, non-uniform body: the bytes cycle, so a part landing in the
    /// wrong order or an off-by-one in the reassembled object cannot survive a
    /// byte-for-byte comparison.
    fn pattern(len: usize, start: u8) -> Vec<u8> {
        (0..len).map(|i| start.wrapping_add(i as u8)).collect()
    }

    /// The digest the puts stamp and the metadata must report back.
    fn digest_of(body: &[u8]) -> [u8; 32] {
        Sha256::digest(body).into()
    }

    /// Puts `body` as a large overwrite, then reads it back byte for byte with the
    /// stamped digest, size and content type.
    async fn put_and_read_back(
        store: &spot_files::ObjectFileStore,
        site: &SiteId,
        key: &FileKey,
        body: &[u8],
    ) {
        let stored = expect_ok(
            store
                .put(
                    site,
                    key.clone(),
                    body_from_vec(body.to_vec()),
                    PutOptions::new("application/x-spot-multipart-test", u64::MAX, true),
                )
                .await,
            "putting the large body",
        );
        assert_eq!(
            stored.sha256,
            digest_of(body),
            "the put must report the streamed digest",
        );
        let (meta, mut read) = expect_ok(store.get(site, key.clone()).await, "reading it back");
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
    }

    let site = expect_ok(SiteId::new("multipart"), "building the site id");
    let boundary_key = expect_ok(
        FileKey::new(Visibility::Private, "multipart/boundary.bin"),
        "building the boundary key",
    );
    let boundary_body = pattern(SINGLE_PUT_LIMIT as usize + 1, 3);
    put_and_read_back(store, &site, &boundary_key, &boundary_body).await;

    let three_parts_key = expect_ok(
        FileKey::new(Visibility::Private, "multipart/three-parts.bin"),
        "building the three-parts key",
    );
    let three_parts_body = pattern(2 * PART_SIZE + 1024 * 1024, 7);
    put_and_read_back(store, &site, &three_parts_key, &three_parts_body).await;

    // `head` answers from the attributes alone and agrees with the puts…
    for (key, body) in [
        (&boundary_key, &boundary_body),
        (&three_parts_key, &three_parts_body),
    ] {
        let head = expect_ok(store.head(&site, key.clone()).await, "heading the object");
        let head = match head {
            Some(meta) => meta,
            None => panic!("a stored object must head as Some"),
        };
        assert_eq!(head.size, body.len() as u64);
        assert_eq!(head.sha256, digest_of(body));
    }

    // …and the listing carries the attributes straight off the objects.
    // Prefixes match full key text, which starts with the visibility segment.
    let prefix = expect_ok(
        FileKeyPrefix::new(&format!("{}/multipart", Visibility::Private.as_str())),
        "building the prefix",
    );
    let listed = expect_ok(
        store.list(&site, &prefix, None, 100).await,
        "listing the multipart objects",
    );
    for (key, body) in [
        (&boundary_key, &boundary_body),
        (&three_parts_key, &three_parts_body),
    ] {
        let entry = listed
            .iter()
            .find(|entry| entry.key.as_str() == key.as_str())
            .unwrap_or_else(|| panic!("the listing must carry {key:?}"));
        assert_eq!(entry.size, body.len() as u64);
        assert_eq!(entry.sha256, digest_of(body));
    }

    // A conditional create over an existing large key is refused — the HEAD guard in
    // front of the multipart initiation — and the old object stays digest-identical.
    let refused_body = pattern(SINGLE_PUT_LIMIT as usize + 1, 250);
    let refused = store
        .put(
            &site,
            boundary_key.clone(),
            body_from_vec(refused_body),
            PutOptions::new("application/x-spot-multipart-test", u64::MAX, false),
        )
        .await;
    match refused {
        Err(FileError::AlreadyExists { .. }) => {}
        other => {
            panic!("the guarded create over an existing large key must be refused, got {other:?}")
        }
    }
    let head = expect_ok(
        store.head(&site, boundary_key.clone()).await,
        "heading the surviving object",
    );
    let head = match head {
        Some(meta) => meta,
        None => panic!("the refused create must leave the old object in place"),
    };
    assert_eq!(head.size, boundary_body.len() as u64);
    assert_eq!(head.sha256, digest_of(&boundary_body));
}
