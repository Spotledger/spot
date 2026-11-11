//! Signed administrative commands (`blueprint WS22`, `WP-ADM-01.T2`, `ADR-44`).
//!
//! An administrator's console or CLI turns an operation into a [`Command`]; the
//! administrator's key signs it ([`sign`], or [`SignedCommand::add_signature`] for a
//! co-signature); the agent's [`CommandVerifier`] decides locally whether to run it. The
//! verifier's inputs are the signed command, the agent's trust list and policy, the
//! agent's nonce window, the audience it expects and the time — **the time is always an
//! argument**: this crate never reads a clock, so a test or an offline agent drives the
//! same code a live one does.
//!
//! # The bytes
//!
//! The signed message is [`DOMAIN_PREFIX`]` || `
//! [`Command::canonical_bytes`](command::Command::canonical_bytes) — a domain-separated
//! canonical JSON document, one exact byte sequence per command
//! ([`canonical`] documents the rules). Every signature is a raw 64-byte Ed25519
//! signature over those bytes, and [`KeyId`] names keys by the first 16 bytes of
//! SHA-256 of the public key, hex.
//!
//! # Safety model
//!
//! - **Audience**: a command for another agent never verifies here
//!   ([`CommandError::WrongAudience`]).
//! - **Expiry**: the command carries its window; the verifier applies the policy's skew
//!   on the issue side and refuses at and past `expires_at`
//!   ([`CommandError::NotYetValid`], [`CommandError::Expired`]).
//! - **Lifetime bound**: a window longer than the policy allows is refused
//!   ([`CommandError::LifetimeExceeded`]), so an issuer cannot mint never-expiring
//!   commands.
//! - **Signatures**: at least the policy's count of valid signatures from distinct
//!   trusted keys; the same key signing twice counts once
//!   ([`CommandError::InsufficientSignatures`]).
//! - **Replay**: the nonce is checked-and-recorded atomically, last, only after every
//!   other check passed — a rejected command can be corrected and sent again, an
//!   accepted one can never verify again ([`CommandError::Replay`]).
//! - **Revocation**: a key revoked before `issued_at` fails for those commands
//!   ([`CommandError::KeyRevoked`]).
//!
//! # What this crate is not
//!
//! No networking, no clock reads, no dependency on the operation catalogue
//! (`op`/`params` are opaque). Which keys are trusted, hardware-backed keys and the
//! two-person rule belong to WP-ADM-03; the signature list is plural and
//! [`SignedCommand::add_signature`] exists so that task needs no redesign here.
//!
//! # Errors
//!
//! Every failure is a [`CommandError`] with a stable `cmd.*` code
//! ([`CommandError::code`]); messages never echo `params` or other untrusted input.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod canonical;
pub mod command;
pub mod envelope;
pub mod error;
pub mod nonce;
#[cfg(feature = "testing")]
pub mod testing;

pub use command::{COMMAND_VERSION, Command, DOMAIN_PREFIX, KeyId};
pub use envelope::{
    CommandSignature, CommandVerifier, DEFAULT_MAX_LIFETIME, DEFAULT_MAX_SKEW,
    DEFAULT_MIN_SIGNATURES, SignedCommand, Signer, SoftwareSigner, TrustedKeyEntry, TrustedKeys,
    Verified, VerifyPolicy, sign,
};
pub use error::{CommandError, StoreError};
pub use nonce::{DEFAULT_NONCE_CAP, MemoryNonceStore, NonceFuture, NonceStore};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-signed-command";
