//! [`SaltSource`] and [`DerivedSalt`]: derivation, tenancy, versions and fail-closed.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use spot_secrets::{KeyProvider, KeyRef, MemoryKeyring, ProviderFuture, SecretBytes, WrappedKey};
use spot_throttle_coordinator::{
    DerivedSalt, SALT_INFO, SaltFuture, SaltRing, SaltSource, ThrottleError, ThrottleSalt,
    load_ring,
};

fn source() -> DerivedSalt<MemoryKeyring> {
    DerivedSalt::new(MemoryKeyring::new("throttle").unwrap())
}

/// A provider that keeps its key but cannot derive from it: `spot-secrets`' default
/// `derive_key` refuses, so the throttle must refuse too rather than invent a salt.
struct NoDerivation(MemoryKeyring);

impl KeyProvider for NoDerivation {
    fn current(&self) -> KeyRef {
        self.0.current()
    }

    fn wrap<'a>(&'a self, data_key: &'a [u8], aad: &'a [u8]) -> ProviderFuture<'a, WrappedKey> {
        self.0.wrap(data_key, aad)
    }

    fn unwrap<'a>(
        &'a self,
        key: &'a KeyRef,
        wrapped: &'a WrappedKey,
        aad: &'a [u8],
    ) -> ProviderFuture<'a, SecretBytes> {
        self.0.unwrap(key, wrapped, aad)
    }
}

/// A source that is simply down.
struct Down;

impl SaltSource for Down {
    fn load(&self, _tenant: &str, _version: u32) -> SaltFuture<'_> {
        Box::pin(async {
            Err(ThrottleError::SaltUnavailable {
                reason: "the salt source is unavailable",
            })
        })
    }
}

#[test]
fn the_same_tenant_and_version_always_derive_the_same_salt() {
    let source = source();
    let first = block_on(source.load("t1", 1)).unwrap();
    let second = block_on(source.load("t1", 1)).unwrap();
    assert!(
        first.same_key(&second),
        "a restart must rebuild exactly the ring it had"
    );
}

#[test]
fn a_different_tenant_or_version_derives_a_different_salt() {
    let source = source();
    let base = block_on(source.load("t1", 1)).unwrap();
    let other_tenant = block_on(source.load("t2", 1)).unwrap();
    let other_version = block_on(source.load("t1", 2)).unwrap();
    assert!(
        !base.same_key(&other_tenant),
        "two tenants never share a salt"
    );
    assert!(
        !base.same_key(&other_version),
        "a version is a rotation, not the same salt"
    );
    assert!(!other_tenant.same_key(&other_version));
}

#[test]
fn two_providers_with_different_master_keys_derive_different_salts() {
    let one = source();
    let two = source();
    let a = block_on(one.load("t1", 1)).unwrap();
    let b = block_on(two.load("t1", 1)).unwrap();
    assert!(!a.same_key(&b), "the master key is part of the input");
}

#[test]
fn a_provider_that_cannot_derive_refuses() {
    let source = DerivedSalt::new(NoDerivation(MemoryKeyring::new("throttle").unwrap()));
    let error = block_on(source.load("t1", 1)).unwrap_err();
    assert_eq!(error.code(), "throttle_coordinator.salt_unavailable");
    assert!(
        error.to_string().contains("refuse"),
        "the caller is told to refuse: {error}"
    );
}

#[test]
fn a_source_that_is_down_refuses_and_so_refuses_the_whole_ring() {
    let error = block_on(Down.load("t1", 1)).unwrap_err();
    assert_eq!(error.code(), "throttle_coordinator.salt_unavailable");
    let ring: Result<SaltRing, _> = block_on(load_ring(&Down, "t1", 1, Some(0)));
    assert_eq!(
        ring.unwrap_err().code(),
        "throttle_coordinator.salt_unavailable",
        "no ring means no counting, which means refusal"
    );
}

#[test]
fn a_version_of_zero_and_an_unplain_tenant_are_refused() {
    let source = source();
    for (tenant, version) in [("t1", 0), ("", 1), ("a:b", 1), ("with space", 1)] {
        let error = block_on(source.load(tenant, version)).unwrap_err();
        assert_eq!(
            error.code(),
            "throttle_coordinator.salt_unavailable",
            "tenant {tenant:?} version {version}"
        );
    }
    // A tenant of exactly the longest accepted length still works.
    let longest = "t".repeat(64);
    assert!(block_on(source.load(&longest, 1)).is_ok());
    assert!(block_on(source.load(&format!("{longest}t"), 1)).is_err());
}

#[test]
fn load_ring_loads_both_salts_or_neither() {
    let source = source();
    let single = block_on(load_ring(&source, "t1", 2, None)).unwrap();
    assert!(!single.has_previous());
    assert!(
        single
            .current()
            .same_key(&block_on(source.load("t1", 2)).unwrap())
    );

    let overlap = block_on(load_ring(&source, "t1", 2, Some(1))).unwrap();
    assert!(overlap.has_previous());
    assert!(
        overlap
            .current()
            .same_key(&block_on(source.load("t1", 2)).unwrap())
    );
    assert!(
        overlap
            .previous()
            .is_some_and(|p| p.same_key(&block_on(source.load("t1", 1)).unwrap()))
    );
    // The two salts of one ring are different, or the rotation would not rotate anything.
    assert!(!overlap.previous().unwrap().same_key(overlap.current()));
}

#[test]
fn the_label_is_the_documented_domain_separator() {
    assert_eq!(SALT_INFO, "spot.throttle.salt.v2");
}

#[test]
fn the_salt_is_the_providers_derive_key_under_the_documented_info() {
    let provider = Arc::new(MemoryKeyring::new("throttle").unwrap());
    let source = DerivedSalt::new(Arc::clone(&provider));
    let mut info = SALT_INFO.as_bytes().to_vec();
    info.push(0);
    info.extend_from_slice(&2_u64.to_be_bytes());
    info.extend_from_slice(b"t1");
    info.extend_from_slice(&3_u32.to_be_bytes());
    let expected = block_on(provider.derive_key(&provider.current(), &info, 32)).unwrap();
    let salt = block_on(source.load("t1", 3)).unwrap();
    assert!(
        salt.same_key(&ThrottleSalt::new(expected.expose()).unwrap()),
        "the salt is exactly one derive_key call with the documented info"
    );
    // It is not the blind-index key, which is what `v1` chained from.
    let index = block_on(provider.index_key(&provider.current())).unwrap();
    assert!(!salt.same_key(&ThrottleSalt::new(index.expose()).unwrap()));
}

#[test]
fn two_tenants_never_share_a_salt_across_many_ids() {
    let source = source();
    let salts: Vec<_> = (0..20)
        .map(|i| block_on(source.load(&format!("tenant-{i}"), 1)).unwrap())
        .collect();
    for (i, a) in salts.iter().enumerate() {
        for b in &salts[i + 1..] {
            assert!(!a.same_key(b));
        }
    }
}

#[test]
fn the_debug_of_every_new_type_is_redacted() {
    let source = source();
    assert_eq!(format!("{source:?}"), "DerivedSalt(<redacted>)");
    assert!(!format!("{source:?}").contains("throttle\""));

    let derived = block_on(source.load("t1", 1)).unwrap();
    assert_eq!(format!("{derived:?}"), "ThrottleSalt(<redacted>)");

    let ring = block_on(load_ring(&source, "t1", 1, Some(0)));
    assert!(ring.is_err());
    // An error carries a fixed sentence, never key material or a tenant.
    let error = ring.unwrap_err();
    assert!(!error.to_string().contains("t1"));
    assert!(!format!("{error:?}").contains("t1"));
}

/// A tiny executor for the synchronous cases: they never yield on anything but a ready future.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    use std::task::{Context, Poll, Waker};
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        std::thread::yield_now();
    }
}

#[test]
fn the_source_is_object_safe_and_usable_behind_a_reference() {
    let source = Arc::new(source());
    let as_dyn: &dyn SaltSource = source.as_ref();
    assert!(block_on(as_dyn.load("t1", 1)).is_ok());
}
