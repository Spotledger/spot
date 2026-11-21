//! SHA-256, OS random bytes and the byte codec for wire values.

use rand::TryRngCore;
use sha2::{Digest, Sha256};

/// The SHA-256 digest of `data`, as exactly 32 bytes.
#[must_use]
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let mut digest = [0u8; 32];
    // The slice length is fixed by the array type; this copy cannot fail.
    digest.copy_from_slice(&hasher.finalize());
    digest
}

/// Fills `bytes` from the OS CSPRNG.
///
/// The OS entropy source is always available on the platforms this workspace targets; a
/// failure there is an unrecoverable environment error, not bad input, and the process
/// must never continue producing secret material from a source it knows is broken.
pub fn fill_os_random(bytes: &mut [u8]) {
    if rand::rngs::OsRng.try_fill_bytes(bytes).is_err() {
        panic!("the OS CSPRNG failed; refusing to create secret material");
    }
}
