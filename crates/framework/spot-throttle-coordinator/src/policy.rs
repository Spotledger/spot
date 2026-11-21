//! The four limits and the digest salt.

use std::fmt;
use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use spot_coordinator::RateLimit;

use crate::error::ThrottleError;

/// The shortest salt accepted, in bytes.
pub const MIN_SALT_BYTES: usize = 16;

/// The key of the keyed digest that names every bucket.
///
/// It is never written by `Debug`, stored or returned. Rotating it on its own starts every
/// bucket afresh, which only forgets (an attacker who waits for a rotation gets a free reset),
/// so a rotation goes through [`crate::SaltRing`], which keeps the replaced salt in force until
/// its window has refilled.
#[derive(Clone)]
pub struct ThrottleSalt {
    mac: Hmac<Sha256>,
}

impl ThrottleSalt {
    /// A salt of at least [`MIN_SALT_BYTES`] bytes of secret material.
    pub fn new(secret: &[u8]) -> Result<Self, ThrottleError> {
        if secret.len() < MIN_SALT_BYTES {
            return Err(ThrottleError::BadPolicy {
                reason: "the salt must be at least 16 bytes",
            });
        }
        let mac = Hmac::<Sha256>::new_from_slice(secret).map_err(|_| ThrottleError::BadPolicy {
            reason: "the salt was not accepted as a key",
        })?;
        Ok(Self { mac })
    }

    /// Hex of HMAC-SHA-256 over the length-prefixed `parts`.
    pub(crate) fn digest(&self, parts: &[&str]) -> String {
        let mut mac = self.mac.clone();
        for part in parts {
            // Length prefix: ("ab","c") and ("a","bc") cannot collide.
            mac.update(&(part.len() as u64).to_be_bytes());
            mac.update(part.as_bytes());
        }
        let bytes = mac.finalize().into_bytes();
        let mut out = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            for nibble in [byte >> 4, byte & 0x0f] {
                out.push(char::from(b"0123456789abcdef"[usize::from(nibble)]));
            }
        }
        out
    }

    /// Whether `other` holds the same secret.
    ///
    /// The key bytes are never exposed, so the two are compared through the digest of one fixed
    /// input. It lets a caller notice that a "rotation" names the salt already in force, which
    /// would drop the previous salt without changing anything (see
    /// [`crate::SaltRing::rotate_checked`]).
    pub fn same_key(&self, other: &Self) -> bool {
        self.digest(&[SAME_KEY_PROBE]) == other.digest(&[SAME_KEY_PROBE])
    }
}

/// The fixed input the salt comparison digests. A constant, so the comparison reveals nothing
/// about either salt. It carries no dot: the repository's error-code scan reads a dotted
/// lower-case literal as an error code, and this is not one.
const SAME_KEY_PROBE: &str = "spot throttle salt same key probe v1";

impl fmt::Debug for ThrottleSalt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ThrottleSalt(<redacted>)")
    }
}

/// One rate limit per dimension.
///
/// Validated at construction: every limit non-zero, and the tenant ceiling at least as fast as
/// the per-source rate (a ceiling below one source's rate would let one source deny the tenant).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThrottlePolicy {
    per_account: RateLimit,
    per_source: RateLimit,
    per_destination: RateLimit,
    per_tenant: RateLimit,
}

impl ThrottlePolicy {
    /// Validates and builds a policy.
    pub fn new(
        per_account: RateLimit,
        per_source: RateLimit,
        per_destination: RateLimit,
        per_tenant: RateLimit,
    ) -> Result<Self, ThrottleError> {
        for limit in [per_account, per_source, per_destination, per_tenant] {
            if limit.capacity == 0 || limit.per.is_zero() {
                return Err(ThrottleError::BadPolicy {
                    reason: "every limit needs a capacity and a window above zero",
                });
            }
            limit.validate().map_err(|_| ThrottleError::BadPolicy {
                reason: "a limit is outside what the coordinator accepts",
            })?;
        }
        // tenant rate >= source rate, compared exactly: capacity_t / per_t >= capacity_s / per_s.
        let tenant = u128::from(per_tenant.capacity) * per_source.per.as_nanos();
        let source = u128::from(per_source.capacity) * per_tenant.per.as_nanos();
        if tenant < source {
            return Err(ThrottleError::BadPolicy {
                reason: "the per-tenant limit must be at least as fast as the per-source limit",
            });
        }
        Ok(Self {
            per_account,
            per_source,
            per_destination,
            per_tenant,
        })
    }

    /// The limit on one account name (per attempt kind).
    pub fn per_account(&self) -> RateLimit {
        self.per_account
    }

    /// The limit on one source.
    pub fn per_source(&self) -> RateLimit {
        self.per_source
    }

    /// The limit on one destination (e-mail or phone) across all kinds.
    pub fn per_destination(&self) -> RateLimit {
        self.per_destination
    }

    /// The tenant-wide ceiling.
    pub fn per_tenant(&self) -> RateLimit {
        self.per_tenant
    }

    /// The longest window of the four limits.
    ///
    /// It is how long a rotation must stand before every bucket named by the salt it replaced
    /// has refilled on its own, so it is what [`crate::SaltRing::safe_to_retire`] compares the
    /// time since the rotation against.
    pub fn longest_window(&self) -> Duration {
        [
            self.per_account,
            self.per_source,
            self.per_destination,
            self.per_tenant,
        ]
        .into_iter()
        .map(|limit| limit.per)
        .max()
        .unwrap_or(Duration::ZERO)
    }
}
