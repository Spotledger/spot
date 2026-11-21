//! Spotledger host agent identity and enrolment (blueprint WS22, WP-ADM-02.T1, ADR-44).
//!
//! A new server joins an Admin Center by an **outbound** call carrying a one-time token.
//! The two sides prove themselves with Ed25519 keys; the agent ends up with a pinned
//! server identity and the server with the agent's public key. This crate is that
//! protocol logic and key handling. It performs **no networking of any kind**: the
//! messages below are plain [`serde`] types that a later transport task carries over a
//! mutual-TLS connection it opens itself. Nothing here opens sockets, ports or servers,
//! and the crate must never gain them (guarded by a source-scan test in `tests/`).
//!
//! # Pieces
//!
//! - [`keys::KeyStore`] + [`keys::Protection`]: the identity key of a side. Signing is a
//!   trait method so a hardware (TPM) implementation never exports its private key.
//!   [`keys::FileKeyStore`] is the software store (Ed25519, OS CSPRNG, atomically written
//!   `0600` on Unix); [`keys::HardwareKeyStore`] is only the documented extension point —
//!   this crate ships no real TPM binding (the lead briefs that separately, hardware is
//!   needed to test it); [`keys::MemoryKeyStore`] is the test double.
//! - [`token::TokenIssuer`] + [`token::TokenStore`]: one-time enrolment tokens. The
//!   printable token has a checksum group so a typo is caught before any lookup; only a
//!   SHA-256 digest, the expiry, the group/tag and the issuer are stored. Tokens are
//!   single use, expire (default 24 h, maximum 7 days), and every refusal looks the same
//!   to the caller — the precise reason goes to the internal audit log only.
//! - [`message`]: the three exchange messages plus the acceptance,
//!   [`message::EnrolRequest`] → [`message::EnrolChallenge`] → [`message::EnrolProof`] →
//!   [`message::EnrolAccepted`], and the pinned expectation
//!   ([`keys::ExpectedServer`]) the install command hands the agent.
//! - [`agent::AgentEnroller`] and [`service::EnrolmentService`]: the two state machines.
//!   The agent never sends its proof before it has verified the challenge against the
//!   **pinned** server key ([`error::EnrolError::ServerKeyMismatch`] otherwise); the
//!   server consumes the token when the request arrives, and a proof that does not
//!   verify leaves the token consumed (a guess can never be retried) and records the
//!   attempt in the audit log.
//! - [`transcript`]: the one domain-separated byte string
//!   (`spotledger-enrol-v1\0` + both nonces + both keys + the token-hash prefix) that
//!   both signatures cover.
//! - [`identity::AgentIdentity`]: what the agent persists as TOML after acceptance,
//!   written atomically next to its key; `load` rejects files with extra or missing
//!   fields.
//!
//! # What never leaks
//!
//! Every type holding secret material (the printable token, token digests, key-store
//! seeds) is `zeroize`d on drop and prints as a redacted `Debug`; error texts never
//! echo tokens, keys or presented input.
//!
//! # What this crate is not
//!
//! No transport, no mutual TLS, no privilege separation, no signed self-updates, no
//! operation catalogue, and no real TPM binding: those are later WP-ADM-02/-03 tasks.
//! There is no clock either — every instant is passed in as whole seconds since the
//! Unix epoch (UTC), so callers inject time and the boundary tests stay exact.

#![forbid(unsafe_code)]

pub mod agent;
pub mod codec;
pub mod error;
pub mod identity;
pub mod keys;
pub mod message;
pub mod service;
pub mod token;
pub mod transcript;
pub mod util;
