//! Providers for the ports of the usage emitter that need no new dependency (WP-CTL-10.T10): the
//! per-site salt of the active-user digests.
//!
//! The mutual-TLS shipper and the usage counters are not here (see
//! `docs/running-spot/link_ports.md`).

use super::metering::SiteSalt;

/// The shortest salt accepted, in bytes.
pub const MIN_SALT_BYTES: usize = 16;

/// A per-site salt held in memory. The host reads the bytes from the secret store at start and
/// hands them over; this type only refuses a salt too short to protect the digests and never
/// prints, logs or formats its bytes.
pub struct StoredSalt {
    bytes: Vec<u8>,
}

impl StoredSalt {
    /// A salt from `bytes`.
    ///
    /// # Errors
    /// A message (no value) when `bytes` is shorter than [`MIN_SALT_BYTES`] or all zero.
    pub fn new(bytes: Vec<u8>) -> Result<Self, String> {
        if bytes.len() < MIN_SALT_BYTES || bytes.iter().all(|b| *b == 0) {
            return Err(format!(
                "the site salt must be at least {MIN_SALT_BYTES} bytes and not all zero"
            ));
        }
        Ok(Self { bytes })
    }
}

impl std::fmt::Debug for StoredSalt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StoredSalt(..)")
    }
}

impl SiteSalt for StoredSalt {
    fn salt(&self) -> Vec<u8> {
        self.bytes.clone()
    }
}
