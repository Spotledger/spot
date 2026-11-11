//! The ports the sign-in service depends on: users, keys, hashing.
//!
//! Each is a small object-safe trait so the service is testable without a database and a
//! storage-backed implementation can be swapped in behind it. The storage-backed implementations
//! that read the platform app's records directly are the next slice; see the crate docs for the
//! credential-storage constraint that shapes this boundary.

use std::future::Future;
use std::pin::Pin;

use spot_auth::{HashParams, Password, PasswordHash, StoredDigest, TotpSecret};
use spot_storage_contract::RecordId;

use crate::error::{LoginError, LoginErrorKind};
use crate::model::{KeyRecord, UserRecord};

/// A boxed `Send` future, the same idiom `spot-http` and `spot-app-api` use (ADR-057) so the
/// ports can be objects behind `Arc<dyn …>`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Finds users and maintains their credential material.
pub trait UserDirectory: Send + Sync + 'static {
    /// Finds a user by sign-in name or e-mail, case-insensitively.
    fn find_by_identifier<'a>(
        &'a self,
        identifier: &'a str,
    ) -> BoxFuture<'a, Result<Option<UserRecord>, LoginError>>;

    /// Loads a user by its stable id.
    fn find_by_id<'a>(
        &'a self,
        id: RecordId,
    ) -> BoxFuture<'a, Result<Option<UserRecord>, LoginError>>;

    /// Replaces the user's password hash.
    fn set_password_hash<'a>(
        &'a self,
        id: RecordId,
        hash: PasswordHash,
    ) -> BoxFuture<'a, Result<(), LoginError>>;

    /// Records the last accepted TOTP step, so the same code cannot be replayed.
    fn record_totp_step<'a>(
        &'a self,
        id: RecordId,
        step: u64,
    ) -> BoxFuture<'a, Result<(), LoginError>>;

    /// Burns the recovery-code digest at `index` (the user shows a code once).
    fn consume_recovery_digest<'a>(
        &'a self,
        id: RecordId,
        index: usize,
    ) -> BoxFuture<'a, Result<(), LoginError>>;

    /// Replaces the whole second factor: an optional new TOTP secret and a fresh set of
    /// recovery-code digests.
    fn set_second_factor<'a>(
        &'a self,
        id: RecordId,
        totp: Option<TotpSecret>,
        recovery: Vec<StoredDigest>,
    ) -> BoxFuture<'a, Result<(), LoginError>>;
}

/// Looks up API keys by their public id.
pub trait KeyStore: Send + Sync + 'static {
    /// Finds a key by its public key id.
    fn find_by_key_id<'a>(
        &'a self,
        key_id: &'a str,
    ) -> BoxFuture<'a, Result<Option<KeyRecord>, LoginError>>;

    /// Spends `user`'s one-time initial secret if `presented` is it: single use, with an expiry.
    ///
    /// The default knows no initial secrets and refuses every one with
    /// `login.initial_secret_invalid`, so a store that does not implement it fails closed.
    ///
    /// # Errors
    ///
    /// `login.initial_secret_invalid` (unknown, expired, another user's),
    /// `login.initial_secret_used`, or `login.store_failed`.
    fn consume_initial_secret<'a>(
        &'a self,
        _user: RecordId,
        _presented: &'a spot_auth::OpaqueToken,
        _now: i64,
    ) -> BoxFuture<'a, Result<(), LoginError>> {
        Box::pin(async {
            Err(LoginError::new(
                LoginErrorKind::InitialSecretInvalid,
                "this key store holds no initial secrets",
            ))
        })
    }
}

/// Hashes and verifies passwords. A seam so tests can count the exact work done for an unknown
/// user versus a wrong password; production uses [`Argon2Hasher`].
pub trait PasswordHasher: Send + Sync + 'static {
    /// Hashes a password with a fresh salt.
    fn hash(&self, password: &Password) -> PasswordHash;

    /// Verifies a password against a stored hash in constant time relative to the hash.
    fn verify(&self, password: &Password, stored: &PasswordHash) -> bool;
}

/// The production hasher over `spot-auth`'s Argon2id implementation.
pub struct Argon2Hasher {
    params: HashParams,
}

impl Argon2Hasher {
    /// Builds a hasher with the given cost parameters.
    #[must_use]
    pub fn new(params: HashParams) -> Self {
        Self { params }
    }
}

impl PasswordHasher for Argon2Hasher {
    fn hash(&self, password: &Password) -> PasswordHash {
        spot_auth::hash_password(password, &self.params)
    }

    fn verify(&self, password: &Password, stored: &PasswordHash) -> bool {
        spot_auth::verify_password(password, stored, &self.params).is_ok()
    }
}
