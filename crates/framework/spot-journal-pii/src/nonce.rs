//! The nonce source: the crate reads no randomness, so a fresh nonce per encryption is an
//! argument.
//!
//! XChaCha20-Poly1305 takes a 24-byte nonce, and its security depends on a nonce never
//! repeating under one key. The library therefore cannot derive one from the key and the
//! plaintext (that is deterministic encryption, which leaks that two records hold the same
//! value), and it cannot draw one itself. The caller injects a source: production passes an
//! operating-system CSPRNG, tests pass a counter.

/// The XChaCha20-Poly1305 nonce length, in bytes.
pub const NONCE_LEN: usize = 24;

/// A source of fresh AEAD nonces.
///
/// An implementation must return a value it has not returned before under the same key: a
/// random source is fine, a counter is fine for tests and for a persistent sequence, and
/// anything derived from the plaintext is not.
pub trait NonceSource {
    /// The next nonce, or `None` when the source cannot produce one (which fails the protect
    /// call with `journal_pii.nonce_unavailable` rather than reusing a nonce).
    fn next_nonce(&mut self) -> Option<[u8; NONCE_LEN]>;
}
