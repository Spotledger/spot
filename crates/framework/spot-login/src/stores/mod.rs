//! The storage-backed stores: sessions, throttle, users and API keys over the platform app's
//! records, with every credential travelling only through the `SecretAccess` capability.
//!
//! # The capability
//!
//! A [`LoginStores`] obtains **one** `SecretAccess` from the document engine at construction
//! (`SecretCaller::LoginService`) and keeps it in a private field. No function of this crate
//! returns it, no public type has a field of that type, and the four stores reach it only
//! through [`LoginStores::access`], which is `pub(crate)`. `tests/capability.rs` greps this
//! crate's own source for a public path to it.
//!
//! # Which records hold what
//!
//! | Value | Record | Field | Kind |
//! | --- | --- | --- | --- |
//! | session token digest | `session` | `token_digest` | lookup secret (blind index) |
//! | password hash | `user` | `password_hash` | secret |
//! | TOTP secret and its last accepted step | `user` | `totp_secret` | secret |
//! | API key digest | `api_key` | `digest` | lookup secret (blind index) |
//! | one-time initial secret | `api_key` | `digest` | lookup secret (blind index) |
//!
//! Everything else (a session's window, a user's state, an attempt's result) is an ordinary
//! column read and written as the system principal through the storage contract, exactly as
//! WP-PLT-02.T3's ports described. A credential never has a column: it is sealed into
//! `<field>__sealed` (and indexed into `<field>__bidx` for a lookup field) by the engine, in the
//! same transaction as the record.
//!
//! # Short transactions
//!
//! Every call opens its own transaction, does one thing and commits or rolls back; no
//! transaction spans two store calls. The isolation level is declared on every one (ADR-46):
//! `ReadCommitted` for a write, read-only for a read. A `touch` inside the granularity window
//! never opens a write.
//!
//! # Gaps in the platform app this crate cannot fix
//!
//! The platform app's DocTypes (`apps/platform/doctypes/`, not an owned path of this task) have
//! no column for a session's source digest, no session end-reason, and no home for the
//! recovery-code digests (they are child-table rows, and the secret path refuses child
//! DocTypes). Where that leaves a security property unexpressible, the store below **fails
//! closed** and the report lists the exact field each one needs.

mod keys;
mod session;
mod throttle;
pub(crate) mod user;

pub use keys::StorageKeyStore;
pub use session::StorageSessionStore;
pub use throttle::StorageThrottleStore;
pub use user::StorageUserDirectory;

use std::sync::Arc;

use spot_app_api::context::Invocation;
use spot_core::Clock;
use spot_document::engine::Engine;
use spot_document::secrets::{SecretAccess, SecretCaller};
use spot_storage_contract::{Catalog, Isolation, Name, StorageBackend, TxOptions, Value};

use crate::error::{LoginError, LoginErrorKind};

/// The transaction label every store's transaction carries (tracing and the metrics table).
pub const TX_LABEL: &str = "spot_login";

/// The stores of one site's sign-in service, sharing one engine handle and one capability.
///
/// Built once per site (and per test rig). Every store is a thin handle over this value, so
/// the four of them share the same engine, catalog and capability.
pub struct LoginStores<B: StorageBackend> {
    engine: Arc<Engine<B>>,
    catalog: Arc<Catalog>,
    /// The one capability. Private, `pub(crate)` accessor, never returned by a `pub` function.
    access: SecretAccess,
    site: String,
    clock: Arc<dyn Clock>,
}

impl<B: StorageBackend> LoginStores<B> {
    /// Takes the engine, the catalog compiled from the platform app's DocTypes, and the
    /// invocation the service acts under, and obtains the capability **once**.
    ///
    /// The invocation supplies the site (part of every secret's associated data and blind
    /// index) and the audit time of every secret operation.
    pub fn new(
        engine: Arc<Engine<B>>,
        catalog: Arc<Catalog>,
        inv: &Invocation,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let access = engine.secret_access(SecretCaller::LoginService, inv);
        Self {
            engine,
            catalog,
            access,
            site: inv.site.as_str().to_owned(),
            clock,
        }
    }

    /// The session store over the `session` records.
    #[must_use]
    pub fn session_store(self: &Arc<Self>) -> StorageSessionStore<B> {
        StorageSessionStore::new(self.clone())
    }

    /// The throttle store over the `user` counters and the `login_attempt` rows.
    ///
    /// `window` is [`ThrottleConfig::window`](spot_sessions::ThrottleConfig::window): the
    /// derived per-source counter counts only a source's failures inside it.
    #[must_use]
    pub fn throttle_store(
        self: &Arc<Self>,
        window: std::time::Duration,
    ) -> StorageThrottleStore<B> {
        StorageThrottleStore::new(self.clone(), window)
    }

    /// The user directory over the `user` records.
    #[must_use]
    pub fn user_directory(self: &Arc<Self>) -> StorageUserDirectory<B> {
        StorageUserDirectory::new(self.clone())
    }

    /// The API key store over the `api_key` records.
    #[must_use]
    pub fn key_store(self: &Arc<Self>) -> StorageKeyStore<B> {
        StorageKeyStore::new(self.clone())
    }

    /// The capability, for this crate's stores only.
    pub(crate) fn access(&self) -> &SecretAccess {
        &self.access
    }

    /// The engine, for the secret path.
    pub(crate) fn engine(&self) -> &Engine<B> {
        &self.engine
    }

    /// The site every secret operation of this service is bound to.
    #[allow(dead_code)]
    pub(crate) fn site(&self) -> &str {
        &self.site
    }

    /// The clock the service reads time through (never the wall clock directly).
    #[allow(dead_code)]
    pub(crate) fn clock(&self) -> &dyn Clock {
        self.clock.as_ref()
    }

    /// Opens one short transaction: read-only for a read, `ReadCommitted` for a write.
    pub(crate) async fn begin(&self, label: &str, write: bool) -> Result<B::Tx, LoginError> {
        let name = name(label)?;
        let mut options = TxOptions::new(name, Isolation::ReadCommitted);
        if !write {
            options = options.read_only();
        }
        self.engine
            .backend()
            .begin(self.catalog.clone(), options)
            .await
            .map_err(|error| failed_at("begin", &error))
    }
}

/// A storage failure as the one stable `login.store_failed` kind.
pub(crate) fn store_failed(detail: impl Into<String>) -> LoginError {
    LoginError::new(LoginErrorKind::StoreFailed, detail)
}

/// A failure the stores can classify without reading text: a storage error by its
/// [`spot_storage_contract::ErrorKind`], an engine error by its `ErrorClass`.
pub(crate) trait StoreFailure {
    /// Whether the cause was storage being unavailable (retryable, nothing committed).
    fn is_unavailable(&self) -> bool;
    /// The non-secret text for the log: never a value.
    fn describe(&self) -> String;
}

impl StoreFailure for spot_storage_contract::StorageError {
    fn is_unavailable(&self) -> bool {
        self.kind == spot_storage_contract::ErrorKind::Unavailable
    }

    fn describe(&self) -> String {
        self.to_string()
    }
}

impl StoreFailure for spot_app_api::AppError {
    fn is_unavailable(&self) -> bool {
        self.class == spot_app_api::ErrorClass::Unavailable
    }

    fn describe(&self) -> String {
        self.code.clone()
    }
}

/// A storage or engine error with `what` for the log: the detail never carries a value. An
/// unavailable cause becomes the typed [`LoginError::store_unavailable`], decided from the
/// error's kind or class.
pub(crate) fn failed_at(what: &str, error: &impl StoreFailure) -> LoginError {
    let detail = format!("{what}: {}", error.describe());
    if error.is_unavailable() {
        LoginError::store_unavailable(detail)
    } else {
        store_failed(detail)
    }
}

/// The text a `spot-sessions` store error carries for `error`, with the private unavailable
/// marker when the cause was storage being unavailable.
pub(crate) fn failed_text(what: &str, error: &impl StoreFailure) -> String {
    failed_at(what, error).store_text()
}

/// A `Name` from a static string; a failure here is a programming error, reported as a store
/// failure because the store cannot proceed without it.
pub(crate) fn name(text: &str) -> Result<Name, LoginError> {
    Name::new(text).map_err(|error| store_failed(format!("invalid name `{text}`: {error}")))
}

/// Base16 of `bytes`, the stored text form of a digest (a digest is not necessarily UTF-8).
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Parses base16; `None` for anything that is not one.
pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    for pair in bytes.chunks(2) {
        let digit = |byte: u8| (byte as char).to_digit(16).map(|value| value as u8);
        out.push(digit(pair[0])? * 16 + digit(pair[1])?);
    }
    Some(out)
}

/// A unix-seconds instant as the storage contract's `DateTime`.
pub(crate) fn instant(seconds: i64) -> Result<time::OffsetDateTime, LoginError> {
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .map_err(|error| store_failed(format!("instant {seconds}: {error}")))
}

/// The unix seconds of a stored `DateTime`, or `None` for a missing or non-date value.
pub(crate) fn seconds(value: &Value) -> Option<i64> {
    match value {
        Value::DateTime(instant) => Some(instant.unix_timestamp()),
        _ => None,
    }
}

/// The text of a stored value, or `None` for a missing or non-text value.
pub(crate) fn text(value: &Value) -> Option<String> {
    match value {
        Value::Text(text) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// The record id of a stored `Id` value.
pub(crate) fn record(value: &Value) -> Option<spot_storage_contract::RecordId> {
    match value {
        Value::Id(id) => Some(*id),
        _ => None,
    }
}

/// Parses a hyphenated record id (the text form a `UserKey` and a session id carry).
pub(crate) fn parse_record(text_value: &str) -> Option<spot_storage_contract::RecordId> {
    spot_http::api::codec::parse_record_id(text_value)
}

#[cfg(test)]
mod tests {
    use super::{hex, unhex};

    #[test]
    fn base16_round_trips() {
        let bytes = [0u8, 1, 15, 16, 255];
        assert_eq!(hex(&bytes), "00010f10ff");
        assert_eq!(unhex("00010f10ff").as_deref(), Some(&bytes[..]));
        assert!(unhex("0f0").is_none());
        assert!(unhex("zz").is_none());
    }
}
