//! The storage-backed stores under the real schema: the `spot-sessions` contract suite on the
//! fake backend and on PostgreSQL, and the properties only storage can break — a credential
//! beside its secret field, a sealed blob moved to another record, the granularity window, the
//! one-time initial secret, and the capability's absence from the public API.
//!
//! Every case runs on both backends. The PostgreSQL variants call `TestDb`, which prints
//! `SKIPPED: set SPOT_TEST_PG_URL` and returns `None` when no database is configured (and
//! `SPOT_REQUIRE_PG=1` turns that skip into a failure where one is expected).
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

mod common;

use std::sync::Mutex;

use common::storage as st;
use spot_login::{KeyRecord, KeyStore, LoginErrorKind, UserDirectory, UserRecord};
use spot_sessions::contract;
use spot_sessions::{SessionPolicy, SessionStore};
use spot_storage_contract::{StorageBackend, Value};
use st::Touched;

/// Enough prepared rigs for every property `run_all` runs (one fresh store per property).
const CONTRACT_RIGS: usize = 24;

/// Runs the contract suite over `rigs`, one fresh store per property. The rigs are leaked so
/// that their scratch databases outlive the stores the suite keeps.
async fn run_contract<B: StorageBackend + 'static>(rigs: Vec<st::Rig<B>>) {
    assert!(
        rigs.len() >= CONTRACT_RIGS,
        "the suite needs {CONTRACT_RIGS} fresh stores"
    );
    let stores: Vec<_> = rigs.iter().map(st::Linked::new).collect();
    let _keep: &'static Vec<st::Rig<B>> = Box::leak(Box::new(rigs));
    let pool: &'static Mutex<Vec<_>> = Box::leak(Box::new(Mutex::new(stores)));
    contract::run_all(move || {
        pool.lock()
            .expect("the pool")
            .pop()
            .expect("a prepared store per property")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_contract_suite_passes_on_the_fake_backend() {
    let mut rigs = Vec::new();
    for _ in 0..CONTRACT_RIGS {
        rigs.push(st::fake().await);
    }
    run_contract(rigs).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_session_contract_suite_passes_on_postgres() {
    let mut rigs = Vec::new();
    for _ in 0..CONTRACT_RIGS {
        let Some(rig) = st::postgres().await else {
            println!("SKIPPED: set SPOT_TEST_PG_URL");
            return;
        };
        rigs.push(rig);
    }
    run_contract(rigs).await;
}

// ---------------------------------------------------------------------------------------------
// A user is found by name or e-mail, whatever the case
// ---------------------------------------------------------------------------------------------

async fn case_case_insensitive<B: StorageBackend>(rig: st::Rig<B>) {
    let id = rig.user("Alice.Example", "active").await;
    rig.set_password(id, "correct horse battery staple 7731")
        .await;
    let directory = rig.stores.user_directory();
    for identifier in ["alice.example", "ALICE.EXAMPLE", "Alice.Example"] {
        let found = directory
            .find_by_identifier(identifier)
            .await
            .expect("the lookup runs")
            .expect("the user is found");
        assert_eq!(found.id, id, "`{identifier}` did not find the user");
    }
    // The e-mail column answers the same way, so a person may type either.
    let by_email = directory
        .find_by_identifier("ALICE.example@EXAMPLE.test")
        .await
        .expect("the lookup runs")
        .expect("the user is found by e-mail");
    assert_eq!(by_email.id, id);
    assert!(
        directory
            .find_by_identifier("nobody")
            .await
            .expect("the lookup runs")
            .is_none()
    );
}

#[tokio::test]
async fn a_user_is_found_case_insensitively_on_the_fake_backend() {
    case_case_insensitive(st::fake().await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_user_is_found_case_insensitively_on_postgres() {
    if let Some(rig) = st::postgres().await {
        case_case_insensitive(rig).await;
    }
}

// ---------------------------------------------------------------------------------------------
// A sealed credential belongs to one record: moved, it does not open
// ---------------------------------------------------------------------------------------------

async fn case_swapped_blob<B: StorageBackend>(rig: st::Rig<B>) {
    let alice = rig.user("alice", "active").await;
    let bob = rig.user("bob", "active").await;
    rig.set_password(alice, "alice's long password 0001").await;
    rig.set_password(bob, "bob's long password 0002").await;

    let alice_sealed = rig
        .bytes(st::USER, alice, "password_hash__sealed")
        .await
        .expect("alice has a sealed verifier");
    let bob_sealed = rig
        .bytes(st::USER, bob, "password_hash__sealed")
        .await
        .expect("bob has a sealed verifier");
    assert_ne!(alice_sealed, bob_sealed);

    // An attacker with write access to the table copies alice's blob onto bob's row. The
    // associated data binds a value to its site, DocType, record and field, so bob's row holds
    // bytes that cannot open: the read fails closed instead of accepting alice's password.
    rig.poke(
        st::USER,
        bob,
        "password_hash__sealed",
        Value::Bytes(alice_sealed),
    )
    .await;
    let directory = rig.stores.user_directory();
    match directory.find_by_id(bob).await {
        Ok(record) => panic!("the swapped blob opened: {record:?}"),
        Err(error) => assert!(
            error.detail.contains("open_failed"),
            "unexpected error: {error:?}"
        ),
    }
    // Alice's own record still opens.
    assert!(
        directory
            .find_by_id(alice)
            .await
            .expect("alice still reads")
            .is_some()
    );
}

#[tokio::test]
async fn a_swapped_sealed_blob_fails_to_open_on_the_fake_backend() {
    case_swapped_blob(st::fake().await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_swapped_sealed_blob_fails_to_open_on_postgres() {
    if let Some(rig) = st::postgres().await {
        case_swapped_blob(rig).await;
    }
}

// ---------------------------------------------------------------------------------------------
// No credential, and no sealed byte, appears anywhere but its own secret column
// ---------------------------------------------------------------------------------------------

async fn case_no_credential_elsewhere<B: StorageBackend>(rig: st::Rig<B>) {
    let password = "correct horse battery staple 7731";
    let id = rig.user("alice", "active").await;
    rig.set_password(id, password).await;
    let totp = rig.enrol_totp(id).await;

    let key_token = st::OpaqueToken::generate();
    rig.issue_key(id, "spk_alice", &key_token, "active", None)
        .await;
    let signed_in = rig
        .manager(SessionPolicy::DEFAULT)
        .start(st::user_key(id), "10.0.0.1", st::client(), 1_000_000)
        .await
        .expect("a session starts");
    let session_row_id = st::record_id(&signed_in.session.id);
    let key_row_id = rig.api_key_row_id("spk_alice").await;

    let user_row = rig.raw(st::USER, id).await;
    let key_row = rig.raw(st::API_KEY, key_row_id).await;
    let session_row = rig.raw(st::SESSION, session_row_id).await;

    let user_hash = rig
        .bytes(st::USER, id, "password_hash__sealed")
        .await
        .expect("the verifier is sealed");
    let totp_sealed = rig
        .bytes(st::USER, id, "totp_secret__sealed")
        .await
        .expect("the second factor is sealed");
    let key_digest = rig
        .bytes(st::API_KEY, key_row_id, "digest__sealed")
        .await
        .expect("the key digest is sealed");
    let token_digest = rig
        .bytes(st::SESSION, session_row_id, "token_digest__sealed")
        .await
        .expect("the session digest is sealed");

    // The sealed columns exist and hold different bytes: nothing is stored in the clear.
    for sealed in [&user_hash, &totp_sealed, &key_digest, &token_digest] {
        assert!(!sealed.is_empty());
    }

    // Every column a reader could see, secret columns excluded: a credential, its digest and
    // its sealed bytes must appear in none of them.
    let haystack = [
        st::render_public(&user_row.fields),
        st::render_public(&key_row.fields),
        st::render_public(&session_row.fields),
        rig.audit().into_bytes(),
        format!("{:?}", signed_in.session).into_bytes(),
    ]
    .concat();
    let forbidden = [
        password.as_bytes().to_vec(),
        totp.as_bytes().to_vec(),
        st::hex(key_token.digest().as_bytes()).into_bytes(),
        st::hex(signed_in.token.digest().as_bytes()).into_bytes(),
        st::hex(&user_hash).into_bytes(),
        st::hex(&totp_sealed).into_bytes(),
        st::hex(&key_digest).into_bytes(),
        st::hex(&token_digest).into_bytes(),
        user_hash.clone(),
        totp_sealed.clone(),
        key_digest.clone(),
        token_digest.clone(),
    ];
    for needle in &forbidden {
        assert!(
            !st::contains(&haystack, needle),
            "a stored row or the audit log holds a credential form: {}",
            String::from_utf8_lossy(needle)
        );
    }

    // And the domain types redact: the records a caller holds never print a credential.
    let record: UserRecord = rig
        .stores
        .user_directory()
        .find_by_id(id)
        .await
        .expect("the user reads")
        .expect("the user exists");
    let printed = format!("{record:?}");
    assert!(!printed.contains(password));
    assert!(!printed.contains(&totp));
    assert!(!printed.contains("$argon2"));
    let key: KeyRecord = rig
        .stores
        .key_store()
        .find_by_key_id("spk_alice")
        .await
        .expect("the key reads")
        .expect("the key exists");
    assert!(!format!("{key:?}").contains(&st::hex(key_token.digest().as_bytes())));
}

#[tokio::test]
async fn no_credential_appears_beside_its_secret_field_on_the_fake_backend() {
    case_no_credential_elsewhere(st::fake().await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn no_credential_appears_beside_its_secret_field_on_postgres() {
    if let Some(rig) = st::postgres().await {
        case_no_credential_elsewhere(rig).await;
    }
}

// ---------------------------------------------------------------------------------------------
// A touch inside the granularity window writes nothing
// ---------------------------------------------------------------------------------------------

async fn case_granularity<B: StorageBackend>(rig: st::Rig<B>) {
    let id = rig.user("alice", "active").await;
    let store = rig.stores.session_store();
    let started = rig
        .manager(SessionPolicy::DEFAULT)
        .start(st::user_key(id), "10.0.0.1", st::client(), 1_000_000)
        .await
        .expect("a session starts");
    let session_row_id = st::record_id(&started.session.id);
    let before = rig.row(st::SESSION, session_row_id).await.1;
    let idle = SessionPolicy::DEFAULT.idle.as_secs() as i64;
    let absolute = started.session.absolute_expires;

    // 30 s later: the extension is under the 60 s granularity, so nothing is written.
    let touched = store
        .touch(&started.session.id, 1_000_030, idle, absolute)
        .await
        .expect("touch runs");
    assert_eq!(touched, Touched::Skipped);
    assert_eq!(
        rig.row(st::SESSION, session_row_id).await.1,
        before,
        "a skipped touch wrote"
    );

    // Two minutes later: the window moves, and the write is visible.
    let touched = store
        .touch(&started.session.id, 1_000_120, idle, absolute)
        .await
        .expect("touch runs");
    assert_eq!(touched, Touched::Extended);
    let (after, version) = rig.row(st::SESSION, session_row_id).await;
    assert!(version > before, "the extended touch wrote");
    let seen = after
        .fields
        .iter()
        .find(|(name, _)| name.as_str() == "last_seen")
        .map(|(_, value)| value.clone())
        .expect("last_seen");
    assert_eq!(
        seen,
        Value::DateTime(time::OffsetDateTime::from_unix_timestamp(1_000_120).expect("an instant"))
    );
}

#[tokio::test]
async fn a_touch_inside_the_granularity_window_does_not_write_on_the_fake_backend() {
    case_granularity(st::fake().await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_touch_inside_the_granularity_window_does_not_write_on_postgres() {
    if let Some(rig) = st::postgres().await {
        case_granularity(rig).await;
    }
}

// ---------------------------------------------------------------------------------------------
// The one-time initial secret: single use, expiring, never in the clear
// ---------------------------------------------------------------------------------------------

async fn case_initial_password<B: StorageBackend>(rig: st::Rig<B>) {
    let id = rig.user("alice", "invited").await;
    let keys = rig.stores.key_store();
    let secret = st::OpaqueToken::generate();
    let key_id = keys
        .set_initial_password(
            id,
            &secret,
            std::time::Duration::from_secs(3_600),
            1_000_000,
        )
        .await
        .expect("the initial secret is issued");
    assert!(key_id.starts_with("initial-"));
    assert!(
        !rig.audit().contains(secret.as_str()),
        "the audit log names a value"
    );

    // The row holds no readable form of the secret.
    let row_id = rig.api_key_row_id(&key_id).await;
    let row = rig.raw(st::API_KEY, row_id).await;
    let haystack = st::render(&row.fields);
    assert!(!st::contains(&haystack, secret.as_str().as_bytes()));
    assert!(!st::contains(
        &haystack,
        st::hex(secret.digest().as_bytes()).as_bytes()
    ));

    // First use spends it; the second is refused as used.
    keys.consume_initial_secret(id, &secret, 1_000_010)
        .await
        .expect("the first use succeeds");
    let second = keys
        .consume_initial_secret(id, &secret, 1_000_020)
        .await
        .expect_err("the second use is refused");
    assert_eq!(second.kind, LoginErrorKind::InitialSecretUsed);

    // An unused one expires.
    let expiring = st::OpaqueToken::generate();
    keys.set_initial_password(id, &expiring, std::time::Duration::from_secs(60), 1_000_000)
        .await
        .expect("the second initial secret is issued");
    let late = keys
        .consume_initial_secret(id, &expiring, 1_000_061)
        .await
        .expect_err("an expired secret is refused");
    assert_eq!(late.kind, LoginErrorKind::InitialSecretInvalid);

    // Another user's secret is not this user's.
    let bob = rig.user("bob", "invited").await;
    let theft = st::OpaqueToken::generate();
    keys.set_initial_password(bob, &theft, std::time::Duration::from_secs(60), 1_000_000)
        .await
        .expect("bob's initial secret is issued");
    let wrong = keys
        .consume_initial_secret(id, &theft, 1_000_010)
        .await
        .expect_err("another user's secret is refused");
    assert_eq!(wrong.kind, LoginErrorKind::InitialSecretInvalid);
}

#[tokio::test]
async fn the_initial_password_is_single_use_and_expires_on_the_fake_backend() {
    case_initial_password(st::fake().await).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_initial_password_is_single_use_and_expires_on_postgres() {
    if let Some(rig) = st::postgres().await {
        case_initial_password(rig).await;
    }
}

// ---------------------------------------------------------------------------------------------
// Capability discipline: the public API never hands out the capability
// ---------------------------------------------------------------------------------------------

#[test]
fn the_public_api_holds_no_path_to_the_capability() {
    const SOURCES: [(&str, &str); 7] = [
        ("lib.rs", include_str!("../src/lib.rs")),
        ("stores/mod.rs", include_str!("../src/stores/mod.rs")),
        (
            "stores/session.rs",
            include_str!("../src/stores/session.rs"),
        ),
        ("stores/user.rs", include_str!("../src/stores/user.rs")),
        ("stores/keys.rs", include_str!("../src/stores/keys.rs")),
        (
            "stores/throttle.rs",
            include_str!("../src/stores/throttle.rs"),
        ),
        ("admin.rs", include_str!("../src/admin.rs")),
    ];
    let mut issued = 0;
    for (file, source) in SOURCES {
        for line in source.lines() {
            let code = line.trim();
            if code.starts_with("///") || code.starts_with("//!") || code.starts_with("//") {
                continue;
            }
            if code.contains("secret_access(") {
                issued += 1;
                assert!(
                    file == "stores/mod.rs",
                    "{file} obtains a SecretAccess outside the one construction site"
                );
            }
            let public = [
                "pub fn",
                "pub async fn",
                "pub struct",
                "pub const",
                "pub static",
                "pub mod",
                "pub use",
                "pub type",
                "pub enum",
                "pub trait",
            ]
            .iter()
            .any(|prefix| code.starts_with(prefix));
            if public {
                assert!(
                    !code.contains("SecretAccess"),
                    "{file} exposes the capability in a public item: {code}"
                );
            }
            assert!(
                !code.starts_with("pub fn access") && !code.starts_with("pub fn access_"),
                "{file} exposes a public accessor for the capability: {code}"
            );
        }
    }
    assert_eq!(
        issued, 1,
        "the capability must be obtained exactly once, at construction"
    );
}
