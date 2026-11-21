//! The two small cryptographic helpers this crate needs, shared through this module.
//!
//! One hash (`SHA-256` over salted source addresses, via the `sha2` crate — the same
//! workspace digest `spot-auth` uses for [`TokenDigest`]) and OS randomness for the
//! [`SaltKey`](crate::model::SaltKey). `spot-auth` keeps its own copies `pub(crate)`, so
//! this crate holds the same two, with the same failure rules.

use rand::TryRngCore;
use sha2::{Digest, Sha256};

/// The SHA-256 digest of `data`, as exactly 32 bytes: the form
/// [`sha256_concat`] composes and the tests' fixed digests go through
/// ([`crate::tests_support::digest_of`]). Compiled out of production builds unless the
/// `testing` feature is on (only tests and backend suites use the fixed-digest helper).
#[cfg_attr(not(any(test, feature = "testing")), allow(dead_code))]
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let mut digest = [0u8; 32];
    // The length is fixed by the type of `digest`; this copy cannot fail.
    digest.copy_from_slice(&hasher.finalize());
    digest
}

/// SHA-256 over `prefix || 0x00 || rest`: the salted one-way form for source addresses.
/// The zero byte length-separates the parts so no split can collide.
#[must_use]
pub fn sha256_concat(prefix: [u8; 32], rest: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(prefix);
    hasher.update([0u8]);
    hasher.update(rest);
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&hasher.finalize());
    digest
}

/// Fills `bytes` from the OS CSPRNG (`rand`'s OsRng, already in the build through
/// `spot-auth`).
pub fn fill_os_random(bytes: &mut [u8]) {
    if rand::rngs::OsRng.try_fill_bytes(bytes).is_err() {
        panic!("the OS CSPRNG failed; refusing to create secret material");
    }
}
