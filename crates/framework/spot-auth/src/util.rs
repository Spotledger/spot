//! Small shared helpers used across the crate.

/// The SHA-256 digest of `data`, as exactly 32 bytes (RustCrypto `sha2`).
#[must_use]
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};

    let mut hasher = Sha256::new();
    hasher.update(data);
    let mut digest = [0u8; 32];
    // The length is fixed by the type of `digest`; this copy cannot fail.
    digest.copy_from_slice(&hasher.finalize());
    digest
}

/// Fills `bytes` from the OS CSPRNG.
///
/// The OS entropy source is always available on the platforms this workspace targets;
/// a failure there is an unrecoverable environment error, not bad input, and the process
/// must never continue producing secret material from a source it knows is broken.
pub(crate) fn fill_os_random(bytes: &mut [u8]) {
    use rand::TryRngCore;

    if rand::rngs::OsRng.try_fill_bytes(bytes).is_err() {
        panic!("the OS CSPRNG failed; refusing to create secret material");
    }
}

/// Turns a `Result` into its value for test assertions, panicking with context instead of
/// `unwrap`/`expect`, which this workspace forbids.
#[cfg(test)]
pub(crate) fn expect_ok<T, E>(outcome: Result<T, E>, what: &str) -> T
where
    E: core::fmt::Debug,
{
    match outcome {
        Ok(value) => value,
        Err(error) => panic!("{what}: {error:?}"),
    }
}
