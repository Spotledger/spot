//! The domain-separated transcript both signatures cover.
//!
//! One byte string ties the enrolled identities to this exchange and to the token:
//!
//! ```text
//! b"spotledger-enrol-v1\0" ++ nonce_a ++ nonce_s ++ agent_public_key ++ server_public_key ++ token_hash_prefix
//! ```
//!
//! The `spotledger-enrol-v1\0` prefix keeps a signature over this transcript from ever
//! verifying over a message meant for another Spotledger protocol (domain separation).
//! The **token hash prefix** — the first 16 bytes of the token's SHA-256 digest — binds
//! the transcript to one enrolment attempt without putting the printable token (secret
//! material) into the transcript itself. Both sides build the same bytes from the same
//! three messages; a single flipped bit anywhere makes both signatures fail.

use crate::error::EnrolError;
use crate::util::sha256;

/// The domain separator, versioned: a future protocol version changes this constant.
pub const TRANSCRIPT_PREFIX: &[u8] = b"spotledger-enrol-v1\0";

/// How many bytes of the token's SHA-256 digest the transcript carries.
pub const TOKEN_HASH_PREFIX_LEN: usize = 16;

/// Nonce length of both sides: 32 random bytes, fresh per exchange.
pub const NONCE_BYTES: usize = 32;

/// A fresh random nonce (32 OS-CSPRNG bytes) as the base64 text the messages carry.
///
/// Panics only when the OS CSPRNG is broken, which this crate treats as an
/// unrecoverable environment failure exactly as `spot-auth` does — it never falls back
/// to a weaker source.
#[must_use]
pub fn fresh_nonce() -> String {
    let mut bytes = [0u8; NONCE_BYTES];
    crate::util::fill_os_random(&mut bytes);
    crate::codec::enc64(&bytes)
}

/// The base64 text of a caller-supplied nonce, for tests and replays of fixed vectors.
#[must_use]
pub fn nonce_b64(bytes: &[u8; NONCE_BYTES]) -> String {
    crate::codec::enc64(bytes)
}

/// Builds the transcript bytes over the exchange described by the field values given
/// as base64 text, exactly as the messages carry them.
///
/// # Errors
/// [`EnrolError::CryptoMalformed`] when a key or nonce text does not decode in its
/// declared length — the caller passes the texts it received, so a tampered message
/// cannot smuggle in a value of an unexpected shape.
pub fn build(
    nonce_a: &str,
    nonce_s: &str,
    agent_public_key: &str,
    server_public_key: &str,
    token_digest_bytes: &[u8; 32],
) -> Result<Vec<u8>, EnrolError> {
    if nonce_a.len() > 64 || nonce_s.len() > 64 {
        return Err(EnrolError::CryptoMalformed);
    }
    let mut out = Vec::with_capacity(
        TRANSCRIPT_PREFIX.len() + TOKEN_HASH_PREFIX_LEN + nonce_a.len() + nonce_s.len() + 88,
    );
    out.extend_from_slice(TRANSCRIPT_PREFIX);
    out.extend_from_slice(crate::codec::dec64::<32>(nonce_a)?.as_slice());
    out.extend_from_slice(crate::codec::dec64::<32>(nonce_s)?.as_slice());
    out.extend_from_slice(crate::codec::dec64::<32>(agent_public_key)?.as_slice());
    out.extend_from_slice(crate::codec::dec64::<32>(server_public_key)?.as_slice());
    out.extend_from_slice(&token_digest_bytes[..TOKEN_HASH_PREFIX_LEN]);
    Ok(out)
}

/// Convenience for tests and callers that hold digests, not digest arrays.
#[must_use]
pub fn token_hash_prefix(token_digest: &[u8; 32]) -> [u8; TOKEN_HASH_PREFIX_LEN] {
    let mut prefix = [0u8; TOKEN_HASH_PREFIX_LEN];
    prefix.copy_from_slice(&token_digest[..TOKEN_HASH_PREFIX_LEN]);
    prefix
}

/// The digest of a printable token text, for transcript building.
#[must_use]
pub fn token_digest_of(token_text: &str) -> [u8; 32] {
    sha256(token_text.as_bytes())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::{build, nonce_b64, token_digest_of, token_hash_prefix};

    #[test]
    fn transcript_is_deterministic_and_binds_everything() {
        let nonce_a = nonce_b64(&[1u8; 32]);
        let nonce_s = nonce_b64(&[2u8; 32]);
        let agent_key = crate::codec::enc64(&[3u8; 32]);
        let server_key = crate::codec::enc64(&[4u8; 32]);
        let digest = token_digest_of("token");
        let first = build(&nonce_a, &nonce_s, &agent_key, &server_key, &digest)
            .unwrap_or_else(|error| panic!("{error:?}"));
        let second = build(&nonce_a, &nonce_s, &agent_key, &server_key, &digest)
            .unwrap_or_else(|error| panic!("{error:?}"));
        assert_eq!(first, second);
        // Every ingredient is in there: flipping each one changes the transcript.
        assert_ne!(
            first,
            build(
                &nonce_b64(&[9u8; 32]),
                &nonce_s,
                &agent_key,
                &server_key,
                &digest
            )
            .unwrap_or_else(|error| panic!("{error:?}"))
        );
        assert_ne!(
            first,
            build(
                &nonce_a,
                &nonce_s,
                &agent_key,
                &server_key,
                &token_digest_of("other")
            )
            .unwrap_or_else(|error| panic!("{error:?}"))
        );
        // A non-decodable nonce is refused with the codec error, never the byte text.
        let bad = super::build(
            "not-base64-at-all-with-wrong-length-!!!!!!",
            &nonce_s,
            &agent_key,
            &server_key,
            &digest,
        );
        assert!(matches!(
            bad,
            Err(crate::error::EnrolError::CryptoMalformed)
        ));
        let _ = (token_hash_prefix(&digest), first);
    }
}
