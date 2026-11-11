//! The real `AuthService` for the native API (WP-PLT-02.T3, ADR-070).
//!
//! After this crate a person can sign in with a password and, when enrolled, a TOTP or a
//! recovery code; a service holding an API key can authenticate too. It implements
//! `spot_http::api::AuthService` (login, logout, session), the extra native-API v0.3.0 operations
//! (change password, list and end my sessions) and administrative actions (unlock, revoke
//! sessions, reset a second factor). [`LoginService::authenticate`] is the one call every other
//! service makes to turn a request credential into a user.
//!
//! # Shape
//!
//! - [`ports`]: the small object-safe traits the service depends on ([`UserDirectory`],
//!   [`KeyStore`], [`PasswordHasher`]);
//! - [`service`]: [`LoginService`] itself;
//! - [`authenticator`]: [`Authenticator`] and the [`Authenticated`] principal;
//! - [`events`]: the [`IdentityEvents`] sink and the `Login Attempt` record;
//! - [`error`]: the stable `login.*` vocabulary and its `auth.*` mapping;
//! - [`admin`]: the administrative sign-in actions;
//! - [`stores`]: the storage-backed stores over the platform app's records and the one
//!   `SecretAccess` capability (WP-PLT-02.T4).
//!
//! # Security properties
//!
//! - The throttle check runs **before** any hash work.
//! - An unknown user and a wrong password do the **same** hash work (against a fixed dummy hash),
//!   so timing and the returned error never reveal whether a user exists.
//! - A bad password, a disabled account, an unaccepted invitation and an anonymised account all
//!   return the one `auth.invalid_credentials`; the true reason only reaches the audit record.
//! - No event or record carries a credential, a token, a recovery code, a TOTP secret or a raw
//!   address: sources are salted digests and accounts are sign-in names.
//!
//! # The credential-storage boundary
//!
//! A credential reaches storage in exactly one way: the engine's [`SecretAccess`] capability
//! (WP-SEC-03.T3). The platform app declares `user.password_hash`, `user.totp_secret`,
//! `session.token_digest` and `api_key.digest` as `secret` fields, and the engine seals a value
//! into its `<field>__sealed` column (and blind-indexes a `lookup = true` field into
//! `__bidx`) in the same transaction as the record. [`stores::LoginStores`] takes that
//! capability **once** at construction (`SecretCaller::LoginService`) and no public item of this
//! crate can hand it out.
//!
//! Three properties of the platform app's DocTypes are not expressible yet, and the stores fail
//! closed rather than work around them:
//!
//! - a recovery-code digest has no secret-capable record (`user_recovery_digest` is a child
//!   DocType, and the secret path refuses child rows), so recovery codes cannot sign anyone in
//!   over storage and [`stores::StorageUserDirectory`] refuses to write one;
//! - `session` has no end-reason column, so a durable end records "ended on purpose" or
//!   "evicted by the cap" and nothing finer;
//! - `session` has no source-digest column, so a listed session shows the zero digest.
//!
//! # The one-time initial secret
//!
//! [`StorageKeyStore::set_initial_password`](stores::StorageKeyStore::set_initial_password)
//! issues `spot new-site`'s one-time password: an `api_key` record whose sealed digest is a
//! lookup secret with an expiry, spent in one transaction on first use
//! ([`consume_initial_secret`](stores::StorageKeyStore::consume_initial_secret)). The CLI prints
//! the token once; nothing stores it in the clear. At sign-in, a password that does not verify is
//! tried as the user's initial secret ([`KeyStore::consume_initial_secret`]); a missing or non-date
//! expiry on the row is refused (fail closed). See the store's documentation for the
//! hand-over, and `docs/` for how a real deployment should pass it on.
//!
//! [`SecretAccess`]: spot_document::secrets::SecretAccess

#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(missing_docs)]
#![allow(clippy::too_many_arguments)]

pub mod admin;
pub mod authenticator;
pub mod error;
pub mod events;
pub mod model;
pub mod ports;
pub mod service;
pub mod stores;

pub use authenticator::{Authenticated, Authenticator, Via};
pub use error::{LoginError, LoginErrorKind, invalid_credentials};
pub use events::{IdentityEvent, IdentityEvents, LoginAttempt};
pub use model::{KeyRecord, KeyState, SessionSummary, UserRecord, UserState};
pub use ports::{Argon2Hasher, BoxFuture, KeyStore, PasswordHasher, UserDirectory};
pub use service::{
    LoginConfig, LoginService, LoginSource, RESULT_OK, format_time, normalise_identifier,
};
pub use stores::{
    LoginStores, StorageKeyStore, StorageSessionStore, StorageThrottleStore, StorageUserDirectory,
};
