//! Certificate store behaviour on the in-memory fake: validation, idempotency, expiry and
//! chain checks, the HTTPS binding, and the guarantee that a private key never leaves.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_agent_windows_sys::{
    BindOutcome, CertStore, CertificateStore, ChainStatus, Expiry, FakeCertStore, HttpsBinding,
    Thumbprint,
};

const CERT: &[u8] = b"-----BEGIN CERTIFICATE-----\nspotledger-test\n-----END CERTIFICATE-----\n";
const KEY: &[u8] = b"a-private-key";

#[test]
fn thumbprints_validate_and_normalise() {
    let sha1 =
        Thumbprint::parse("AA BB CC DD EE FF 00 11 22 33 44 55 66 77 88 99 AA BB CC DD").unwrap();
    assert_eq!(sha1.as_str().len(), 40);
    assert!(
        sha1.as_str()
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    );
    let sha256 = Thumbprint::parse(&"ab".repeat(32)).unwrap();
    assert_eq!(sha256.as_str().len(), 64);
    assert!(Thumbprint::parse("not-hex").is_err());
    assert!(Thumbprint::parse(&"ab".repeat(21)).is_err());
}

#[test]
fn store_names_parse_case_insensitively() {
    assert_eq!(CertStore::parse("Root").unwrap(), CertStore::Root);
    assert_eq!(CertStore::parse(" ca ").unwrap(), CertStore::Ca);
    assert_eq!(CertStore::parse("MY").unwrap(), CertStore::My);
    let error = CertStore::parse("personal").unwrap_err();
    assert_eq!(error.code(), "agent_windows_sys.unknown_store");
    assert!(CertStore::Root.machine_path().contains("LocalMachine"));
}

#[test]
fn import_is_idempotent() {
    let store = FakeCertStore::new();
    let first = store.import(CertStore::My, CERT, None).unwrap();
    let second = store.import(CertStore::My, CERT, None).unwrap();
    assert_eq!(first, second);
    assert_eq!(store.list(CertStore::My).unwrap().len(), 1);
    assert_eq!(store.list(CertStore::Root).unwrap().len(), 0);
}

#[test]
fn remove_is_idempotent() {
    let store = FakeCertStore::new();
    let thumbprint = store.import(CertStore::My, CERT, None).unwrap();
    store.remove(CertStore::My, &thumbprint).unwrap();
    store.remove(CertStore::My, &thumbprint).unwrap();
    assert!(store.list(CertStore::My).unwrap().is_empty());
    // Removing something that was never there is also fine.
    store.remove(CertStore::Ca, &thumbprint).unwrap();
}

#[test]
fn a_private_key_never_leaves_the_store() {
    let store = FakeCertStore::new();
    let thumbprint = store.install(
        CertStore::My,
        CERT,
        Some(KEY),
        0,
        i64::MAX,
        ChainStatus::Valid,
    );
    assert_eq!(
        store.stored_private_key_len(CertStore::My, &thumbprint),
        Some(KEY.len())
    );
    let exported = store.export_public(CertStore::My, &thumbprint).unwrap();
    assert_eq!(exported, CERT);
    assert!(!exported.windows(KEY.len()).any(|window| window == KEY));
    let refused = store
        .export_private_key(CertStore::My, &thumbprint)
        .unwrap_err();
    assert_eq!(
        refused.code(),
        "agent_windows_sys.private_key_export_refused"
    );
}

#[test]
fn export_of_a_missing_certificate_is_not_found() {
    let store = FakeCertStore::new();
    let thumbprint = Thumbprint::parse(&"ab".repeat(32)).unwrap();
    let error = store.export_public(CertStore::My, &thumbprint).unwrap_err();
    assert_eq!(error.code(), "agent_windows_sys.certificate_not_found");
}

#[test]
fn expiry_is_judged_at_the_given_instant() {
    let store = FakeCertStore::new();
    let thumbprint = store.install(CertStore::My, CERT, None, 1_000, 2_000, ChainStatus::Valid);
    assert_eq!(
        store.check_expiry(CertStore::My, &thumbprint, 999).unwrap(),
        Expiry::NotYetValid
    );
    assert_eq!(
        store
            .check_expiry(CertStore::My, &thumbprint, 1_500)
            .unwrap(),
        Expiry::Valid
    );
    assert_eq!(
        store
            .check_expiry(CertStore::My, &thumbprint, 2_001)
            .unwrap(),
        Expiry::Expired
    );
}

#[test]
fn binding_validates_expiry_and_chain_and_is_idempotent() {
    let store = FakeCertStore::new();
    const DAY: i64 = 86_400;
    let valid = store.install(
        CertStore::My,
        CERT,
        None,
        1_000,
        1_000 + 60 * DAY,
        ChainStatus::Valid,
    );
    let short_lived = store.install(
        CertStore::My,
        b"short-lived",
        None,
        1_000,
        2_000,
        ChainStatus::Valid,
    );
    let untrusted = store.install(
        CertStore::My,
        b"other",
        None,
        1_000,
        2_000,
        ChainStatus::Untrusted {
            reason: "unknown issuer".to_owned(),
        },
    );
    let binding = HttpsBinding::new("spot.example", 443).unwrap();

    // Not valid yet at this instant.
    let error = store
        .bind_https(CertStore::My, &short_lived, &binding, 10)
        .unwrap_err();
    assert_eq!(error.code(), "agent_windows_sys.certificate_not_valid");

    // Expired at this instant.
    let error = store
        .bind_https(CertStore::My, &short_lived, &binding, 5_000)
        .unwrap_err();
    assert_eq!(error.code(), "agent_windows_sys.certificate_not_valid");

    // A chain that does not validate is refused.
    let error = store
        .bind_https(CertStore::My, &untrusted, &binding, 1_500)
        .unwrap_err();
    assert_eq!(error.code(), "agent_windows_sys.chain_invalid");

    // The valid one binds (far from expiry, so no warning), and binding it again is
    // idempotent.
    let outcome = store
        .bind_https(CertStore::My, &valid, &binding, 1_500)
        .unwrap();
    assert_eq!(
        outcome,
        BindOutcome {
            expires_within_days: None
        }
    );
    store
        .bind_https(CertStore::My, &valid, &binding, 1_500)
        .unwrap();
    assert_eq!(store.binding_count(), 1);
    assert_eq!(store.bound_https(&binding).unwrap(), Some(valid.clone()));
}

#[test]
fn binding_warns_within_thirty_days_of_expiry() {
    let store = FakeCertStore::new();
    const DAY: i64 = 86_400;
    let near_expiry = store.install(CertStore::My, CERT, None, 0, 100 * DAY, ChainStatus::Valid);
    let binding = HttpsBinding::new("spot.example", 443).unwrap();

    // 10 days remain: the outcome carries the warning marker.
    let outcome = store
        .bind_https(CertStore::My, &near_expiry, &binding, 90 * DAY)
        .unwrap();
    assert_eq!(
        outcome,
        BindOutcome {
            expires_within_days: Some(10)
        }
    );

    // Far from expiry: no warning.
    let outcome = store
        .bind_https(CertStore::My, &near_expiry, &binding, DAY)
        .unwrap();
    assert_eq!(
        outcome,
        BindOutcome {
            expires_within_days: None
        }
    );
}

#[test]
fn an_invalid_https_binding_is_refused() {
    assert!(HttpsBinding::new("", 443).is_err());
    assert!(HttpsBinding::new("https://spot.example", 443).is_err());
    assert!(HttpsBinding::new("spot.example:443", 443).is_err());
    assert!(HttpsBinding::new("spot.example", 0).is_err());
    assert!(HttpsBinding::new("spot.example", 443).is_ok());
}
