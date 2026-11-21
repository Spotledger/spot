//! Real LocalMachine certificate store conformance (WP-ADM-05.T3, `certificates` in
//! `matrix.toml`).
//!
//! Read-only: it lists and parses the real LocalMachine Root store through the
//! `#[cfg(windows)]` `certutil` client and confirms a missing certificate is a typed
//! error. The suite never imports or removes a certificate, so it cannot change the
//! machine's trust.

use spot_agent_windows_sys::windows::WindowsCertStore;
use spot_agent_windows_sys::{CertStore, CertificateStore, Thumbprint};

#[test]
fn root_store_is_readable() {
    let store = WindowsCertStore::new();
    let certificates = store
        .list(CertStore::Root)
        .expect("the LocalMachine Root store must be readable");

    // A stock Windows trusts a set of roots; an empty list would mean the parse failed.
    assert!(
        !certificates.is_empty(),
        "a Windows machine has trusted root certificates"
    );
    for certificate in &certificates {
        assert_eq!(
            certificate.thumbprint.as_str().len(),
            40,
            "a SHA-1 thumbprint is 40 hex characters"
        );
        assert!(certificate.thumbprint.as_str().chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(certificate.store, CertStore::Root);
    }
}

#[test]
fn a_missing_certificate_is_a_typed_error() {
    let store = WindowsCertStore::new();
    let absent = Thumbprint::parse("0000000000000000000000000000000000000000")
        .expect("a 40-hex thumbprint parses");
    // `export_public` for a certificate that is not present must fail with a typed error,
    // never panic and never return an empty success.
    assert!(
        store.export_public(CertStore::Root, &absent).is_err(),
        "an absent certificate must not export"
    );
}
