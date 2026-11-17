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
//! # Audience (ADR-083 Barrier 1)
//!
//! Every session records who it is for, chosen once at sign-in from the user's type by
//! [`audience_for`] (`internal`, `service` and `agent` give `staff`; `portal` gives `partner`;
//! no staff login path can produce `consumer`). The authenticated principal carries the user's
//! type, read live from the user directory at every lookup, and the session's audience; an API
//! key is `staff` (a key whose owner is a portal user is refused). A session whose audience is
//! not the one its user's type implies is refused as invalid, and a stored session with a
//! missing or unknown audience is invalid too: there is no default. [`LoginService`] answers
//! `AuthService::audience_of` for the HTTP layer, which refuses a credential on a route whose
//! mark does not list its audience before any handler runs.
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
//! Recovery codes are `recovery_code` records (one top-level row per code, the digest a sealed
//! lookup secret, spent once by a compare-and-set on `used_at`), and `session` carries a
//! `source_digest` and an `end_reason` (WP-PLT-02.T10). A session created before those columns
//! existed has neither: it reads as an unknown source with no recorded end reason.
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
pub mod admission;
pub mod authenticator;
pub mod error;
pub mod events;
pub mod model;
pub mod policy;
pub mod ports;
pub mod service;
pub mod stores;
pub mod totp;

#[cfg(any(test, feature = "testing"))]
pub use admission::AllowAll;
pub use admission::{
    Admission, AdmissionError, CoordinatorAdmission, SignInAdmission, SignInAttempt,
};
pub use authenticator::{Authenticated, Authenticator, Via};
pub use error::{LoginError, LoginErrorKind, invalid_credentials};
pub use events::{IdentityEvent, IdentityEvents, LoginAttempt};
pub use model::{
    KeyRecord, KeyState, SessionSummary, UserRecord, UserState, audience_for, parse_user_type,
};
pub use policy::{
    AllowLocal, PasswordDecision, PolicyUnavailable, SigninMode, SigninPolicy, SigninPolicyPort,
    decide_password, ensure_not_weaker,
};
pub use ports::{Argon2Hasher, BoxFuture, KeyStore, PasswordHasher, UserDirectory};
pub use service::{
    LoginConfig, LoginService, LoginSource, RESULT_BREAK_GLASS, RESULT_OK, format_time,
    normalise_identifier,
};
pub use stores::{
    LoginStores, StorageKeyStore, StorageSessionStore, StorageThrottleStore, StorageUserDirectory,
};
pub use totp::{Enrolment, TOTP_ISSUER};
