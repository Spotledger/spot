//! LocalMachine certificate stores: list, import, remove, expiry and chain checks, the
//! HTTPS binding of the Spotledger service, and public-only export.
//!
//! Only the three LocalMachine stores the brief names are supported — `Root`, `CA` and
//! `My` — and only the public part of a certificate ever leaves the machine: the trait
//! offers [`CertificateStore::export_public`] and a deliberately refusing
//! [`CertificateStore::export_private_key`]. A private key supplied to
//! [`CertificateStore::import`] stays inside the store and never appears in a return
//! value, a message or a `Debug` rendering.
//!
//! Time is an argument: [`CertificateStore::check_expiry`] and
//! [`CertificateStore::bind_https`] take the instant to judge against, never read the wall
//! clock. Thumbprints and store names are validated on construction, so a caller cannot
//! pass an unchecked string into the store.
//!
//! The real implementation is [`crate::windows::WindowsCertStore`] (`#[cfg(windows)]`);
//! tests use [`FakeCertStore`].

use std::collections::BTreeMap;
use std::sync::Mutex;

use sha2::{Digest, Sha256};

use crate::error::{WindowsSysError, WindowsSysResult};

/// One of the supported LocalMachine certificate stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CertStore {
    /// `LocalMachine\Root` — trusted root certification authorities.
    Root,
    /// `LocalMachine\CA` — intermediate certification authorities.
    Ca,
    /// `LocalMachine\My` — personal certificates (the service certificate lives here).
    My,
}

impl CertStore {
    /// The three stores, in the order the console shows them.
    pub const ALL: [CertStore; 3] = [CertStore::Root, CertStore::Ca, CertStore::My];

    /// Parses a store name (`root`, `ca`, `my`, case-insensitive).
    ///
    /// # Errors
    /// [`WindowsSysError::UnknownStore`] for any other name.
    pub fn parse(name: &str) -> WindowsSysResult<Self> {
        match name.trim().to_ascii_lowercase().as_str() {
            "root" => Ok(CertStore::Root),
            "ca" => Ok(CertStore::Ca),
            "my" => Ok(CertStore::My),
            other => Err(WindowsSysError::UnknownStore {
                store: other.to_owned(),
            }),
        }
    }

    /// The store's short name.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            CertStore::Root => "root",
            CertStore::Ca => "ca",
            CertStore::My => "my",
        }
    }

    /// The LocalMachine store path, as the platform tools spell it.
    #[must_use]
    pub fn machine_path(&self) -> &'static str {
        match self {
            CertStore::Root => "LocalMachine\\Root",
            CertStore::Ca => "LocalMachine\\CA",
            CertStore::My => "LocalMachine\\My",
        }
    }
}

/// A certificate thumbprint: 40 hex digits (SHA-1) or 64 (SHA-256), case-insensitive,
/// spaces and colons ignored. Stored lower-case.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Thumbprint(String);

impl Thumbprint {
    /// Normalises and validates a thumbprint.
    ///
    /// # Errors
    /// [`WindowsSysError::InvalidThumbprint`] unless the text is 40 or 64 hex digits once
    /// spaces and colons are removed.
    pub fn parse(text: &str) -> WindowsSysResult<Self> {
        let cleaned: String = text
            .chars()
            .filter(|c| !c.is_whitespace() && *c != ':')
            .collect();
        let valid_len = cleaned.len() == 40 || cleaned.len() == 64;
        if !valid_len || !cleaned.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(WindowsSysError::InvalidThumbprint {
                thumbprint: text.to_owned(),
            });
        }
        Ok(Thumbprint(cleaned.to_ascii_lowercase()))
    }

    /// The normalised thumbprint, lower-case hex.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What a certificate looks like to the modules. No field is a secret: a private key is
/// never described here, only whether one exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CertificateInfo {
    /// The certificate's thumbprint.
    pub thumbprint: Thumbprint,
    /// The store it lives in.
    pub store: CertStore,
    /// The subject, as the store reports it.
    pub subject: String,
    /// The issuer, as the store reports it.
    pub issuer: String,
    /// Start of validity, whole seconds since the Unix epoch (UTC).
    pub not_before_unix: i64,
    /// End of validity, whole seconds since the Unix epoch (UTC).
    pub not_after_unix: i64,
    /// Whether a private key is associated with the certificate.
    pub has_private_key: bool,
}

/// The validity of a certificate at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expiry {
    /// Valid at the instant.
    Valid,
    /// Not yet valid at the instant.
    NotYetValid,
    /// Expired at the instant.
    Expired,
}

/// The outcome of a chain check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainStatus {
    /// The chain validates to a trusted root.
    Valid,
    /// The chain does not validate; `reason` says why, in plain words.
    Untrusted {
        /// Why the chain was refused.
        reason: String,
    },
}

/// The Spotledger service HTTPS endpoint a certificate is bound to.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct HttpsBinding {
    /// The host name (no scheme, no port, no path).
    pub hostname: String,
    /// The TCP port.
    pub port: u16,
}

/// What a successful [`CertificateStore::bind_https`] additionally reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BindOutcome {
    /// Whole days remaining before the bound certificate's `NotAfter`, set only when that
    /// is 30 days or fewer away. `None` means the certificate is not close to expiring.
    pub expires_within_days: Option<i64>,
}

/// Whole days from `now_unix` to `not_after_unix`, when that is 30 days or fewer (never a
/// negative value: an already-expired certificate is refused before this runs). Integer
/// arithmetic only, per the no-float-for-time rule.
fn days_until_expiry_if_near(not_after_unix: i64, now_unix: i64) -> Option<i64> {
    let remaining_seconds = not_after_unix.saturating_sub(now_unix);
    if remaining_seconds < 0 {
        return None;
    }
    let remaining_days = remaining_seconds / 86_400;
    if remaining_days <= 30 {
        Some(remaining_days)
    } else {
        None
    }
}

/// Builds the outcome for a bind that just validated `not_after_unix` at `now_unix`,
/// warning (via `tracing`, never with the certificate's key material) when the certificate
/// is within 30 days of expiry.
fn outcome_for(thumbprint: &Thumbprint, not_after_unix: i64, now_unix: i64) -> BindOutcome {
    let expires_within_days = days_until_expiry_if_near(not_after_unix, now_unix);
    if let Some(days_remaining) = expires_within_days {
        tracing::warn!(
            thumbprint = thumbprint.as_str(),
            days_remaining,
            "certificate bound to an HTTPS endpoint is within 30 days of expiry"
        );
    }
    BindOutcome {
        expires_within_days,
    }
}

impl HttpsBinding {
    /// Validates a host name and port.
    ///
    /// # Errors
    /// [`WindowsSysError::BindFailed`] when the host name is empty, padded, carries a
    /// scheme, a slash, a colon or a control character, or when the port is zero.
    pub fn new(hostname: &str, port: u16) -> WindowsSysResult<Self> {
        let bad_host = hostname.is_empty()
            || hostname.trim() != hostname
            || hostname.contains('/')
            || hostname.contains(':')
            || hostname.contains('\\')
            || hostname.chars().any(|c| c.is_control());
        if bad_host {
            return Err(WindowsSysError::BindFailed {
                reason: "the host name must be a bare name with no scheme, port or path".to_owned(),
            });
        }
        if port == 0 {
            return Err(WindowsSysError::BindFailed {
                reason: "the HTTPS port must not be zero".to_owned(),
            });
        }
        Ok(HttpsBinding {
            hostname: hostname.to_owned(),
            port,
        })
    }
}

/// The LocalMachine certificate store, as the modules use it.
pub trait CertificateStore: Send + Sync {
    /// Lists the certificates in `store`, sorted by thumbprint.
    ///
    /// # Errors
    /// [`WindowsSysError::Io`] when the store cannot be read.
    fn list(&self, store: CertStore) -> WindowsSysResult<Vec<CertificateInfo>>;

    /// Imports `certificate` into `store`, associating `private_key` when given (the key
    /// stays in the store and is never returned). Importing the same certificate again is
    /// idempotent and returns the same thumbprint.
    ///
    /// # Errors
    /// [`WindowsSysError::Io`] when the store refuses the import.
    fn import(
        &self,
        store: CertStore,
        certificate: &[u8],
        private_key: Option<&[u8]>,
    ) -> WindowsSysResult<Thumbprint>;

    /// Removes the certificate with `thumbprint` from `store`. Idempotent: removing an
    /// absent certificate succeeds.
    ///
    /// # Errors
    /// [`WindowsSysError::Io`] when the store refuses the removal.
    fn remove(&self, store: CertStore, thumbprint: &Thumbprint) -> WindowsSysResult<()>;

    /// Returns the certificate's public part (the encoded certificate), never the private
    /// key.
    ///
    /// # Errors
    /// [`WindowsSysError::CertificateNotFound`] when the certificate is absent.
    fn export_public(&self, store: CertStore, thumbprint: &Thumbprint)
    -> WindowsSysResult<Vec<u8>>;

    /// Refuses, always: the private key never leaves the machine. Provided as a default
    /// method so every store answers the same way.
    ///
    /// # Errors
    /// [`WindowsSysError::PrivateKeyExportRefused`] unconditionally.
    fn export_private_key(
        &self,
        _store: CertStore,
        thumbprint: &Thumbprint,
    ) -> WindowsSysResult<Vec<u8>> {
        Err(WindowsSysError::PrivateKeyExportRefused {
            thumbprint: thumbprint.as_str().to_owned(),
        })
    }

    /// The validity of `thumbprint` at `now_unix` (whole seconds since the Unix epoch).
    ///
    /// # Errors
    /// [`WindowsSysError::CertificateNotFound`] when the certificate is absent.
    fn check_expiry(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
        now_unix: i64,
    ) -> WindowsSysResult<Expiry>;

    /// Whether `thumbprint`'s chain validates to a trusted root.
    ///
    /// # Errors
    /// [`WindowsSysError::CertificateNotFound`] when the certificate is absent.
    fn check_chain(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
    ) -> WindowsSysResult<ChainStatus>;

    /// Binds `thumbprint` to `binding`, after checking it is valid and trusted at
    /// `now_unix`. Binding the same certificate to the same endpoint again is idempotent;
    /// binding a different certificate replaces the endpoint's certificate. The returned
    /// [`BindOutcome`] carries a warning marker when the certificate is within 30 days of
    /// expiry (also emitted as a `tracing::warn!`, never with key material).
    ///
    /// # Errors
    /// [`WindowsSysError::CertificateNotFound`] when absent,
    /// [`WindowsSysError::CertificateNotValid`] when not valid at `now_unix` (expired or
    /// not yet valid),
    /// [`WindowsSysError::ChainInvalid`] when the chain does not validate.
    fn bind_https(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
        binding: &HttpsBinding,
        now_unix: i64,
    ) -> WindowsSysResult<BindOutcome>;

    /// The certificate currently bound to `binding`, when one is.
    ///
    /// # Errors
    /// [`WindowsSysError::Io`] when the bindings cannot be read.
    fn bound_https(&self, binding: &HttpsBinding) -> WindowsSysResult<Option<Thumbprint>>;
}

/// One stored certificate in the fake.
#[derive(Clone, Debug)]
struct Entry {
    info: CertificateInfo,
    certificate: Vec<u8>,
    private_key: Option<Vec<u8>>,
    chain: ChainStatus,
}

/// The in-memory certificate store used by every test on every platform. The thumbprint is
/// the SHA-256 of the certificate bytes, so imports are deterministic and idempotent.
#[derive(Debug, Default)]
pub struct FakeCertStore {
    state: Mutex<FakeCertState>,
}

#[derive(Debug, Default)]
struct FakeCertState {
    entries: BTreeMap<(CertStore, Thumbprint), Entry>,
    bindings: BTreeMap<HttpsBinding, Thumbprint>,
}

impl FakeCertStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Installs a certificate with an explicit validity window and chain status — the test
    /// seam the trait's `import` (which has no window) cannot express.
    pub fn install(
        &self,
        store: CertStore,
        certificate: &[u8],
        private_key: Option<&[u8]>,
        not_before_unix: i64,
        not_after_unix: i64,
        chain: ChainStatus,
    ) -> Thumbprint {
        let thumbprint = thumbprint_of(certificate);
        let info = CertificateInfo {
            thumbprint: thumbprint.clone(),
            store,
            subject: "CN=spotledger-test".to_owned(),
            issuer: "CN=spotledger-test-ca".to_owned(),
            not_before_unix,
            not_after_unix,
            has_private_key: private_key.is_some(),
        };
        let entry = Entry {
            info,
            certificate: certificate.to_vec(),
            private_key: private_key.map(<[u8]>::to_vec),
            chain,
        };
        self.lock()
            .entries
            .insert((store, thumbprint.clone()), entry);
        thumbprint
    }

    /// How many HTTPS bindings the store holds.
    #[must_use]
    pub fn binding_count(&self) -> usize {
        self.lock().bindings.len()
    }

    /// The length of the stored private key for a certificate, when one was imported. The
    /// bytes themselves are never exposed: this only proves the key stayed in the store.
    #[must_use]
    pub fn stored_private_key_len(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
    ) -> Option<usize> {
        self.lock()
            .entries
            .get(&(store, thumbprint.clone()))
            .and_then(|entry| entry.private_key.as_ref())
            .map(Vec::len)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeCertState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// The SHA-256 thumbprint of a certificate, lower-case hex (64 digits).
fn thumbprint_of(certificate: &[u8]) -> Thumbprint {
    let digest = Sha256::digest(certificate);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{byte:02x}");
    }
    Thumbprint(hex)
}

impl CertificateStore for FakeCertStore {
    fn list(&self, store: CertStore) -> WindowsSysResult<Vec<CertificateInfo>> {
        let state = self.lock();
        let mut infos: Vec<CertificateInfo> = state
            .entries
            .values()
            .filter(|entry| entry.info.store == store)
            .map(|entry| entry.info.clone())
            .collect();
        infos.sort_by(|a, b| a.thumbprint.cmp(&b.thumbprint));
        Ok(infos)
    }

    fn import(
        &self,
        store: CertStore,
        certificate: &[u8],
        private_key: Option<&[u8]>,
    ) -> WindowsSysResult<Thumbprint> {
        Ok(self.install(
            store,
            certificate,
            private_key,
            0,
            i64::MAX,
            ChainStatus::Valid,
        ))
    }

    fn remove(&self, store: CertStore, thumbprint: &Thumbprint) -> WindowsSysResult<()> {
        self.lock().entries.remove(&(store, thumbprint.clone()));
        Ok(())
    }

    fn export_public(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
    ) -> WindowsSysResult<Vec<u8>> {
        let state = self.lock();
        match state.entries.get(&(store, thumbprint.clone())) {
            Some(entry) => Ok(entry.certificate.clone()),
            None => Err(WindowsSysError::CertificateNotFound {
                store: store.as_str().to_owned(),
                thumbprint: thumbprint.as_str().to_owned(),
            }),
        }
    }

    fn check_expiry(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
        now_unix: i64,
    ) -> WindowsSysResult<Expiry> {
        let state = self.lock();
        match state.entries.get(&(store, thumbprint.clone())) {
            Some(entry) => Ok(expiry_of(&entry.info, now_unix)),
            None => Err(WindowsSysError::CertificateNotFound {
                store: store.as_str().to_owned(),
                thumbprint: thumbprint.as_str().to_owned(),
            }),
        }
    }

    fn check_chain(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
    ) -> WindowsSysResult<ChainStatus> {
        let state = self.lock();
        match state.entries.get(&(store, thumbprint.clone())) {
            Some(entry) => Ok(entry.chain.clone()),
            None => Err(WindowsSysError::CertificateNotFound {
                store: store.as_str().to_owned(),
                thumbprint: thumbprint.as_str().to_owned(),
            }),
        }
    }

    fn bind_https(
        &self,
        store: CertStore,
        thumbprint: &Thumbprint,
        binding: &HttpsBinding,
        now_unix: i64,
    ) -> WindowsSysResult<BindOutcome> {
        let mut state = self.lock();
        let Some(entry) = state.entries.get(&(store, thumbprint.clone())) else {
            return Err(WindowsSysError::CertificateNotFound {
                store: store.as_str().to_owned(),
                thumbprint: thumbprint.as_str().to_owned(),
            });
        };
        match expiry_of(&entry.info, now_unix) {
            Expiry::Valid => {}
            Expiry::NotYetValid => {
                return Err(WindowsSysError::CertificateNotValid {
                    thumbprint: thumbprint.as_str().to_owned(),
                    reason: "not yet valid".to_owned(),
                });
            }
            Expiry::Expired => {
                return Err(WindowsSysError::CertificateNotValid {
                    thumbprint: thumbprint.as_str().to_owned(),
                    reason: "expired".to_owned(),
                });
            }
        }
        if let ChainStatus::Untrusted { reason } = &entry.chain {
            return Err(WindowsSysError::ChainInvalid {
                thumbprint: thumbprint.as_str().to_owned(),
                reason: reason.clone(),
            });
        }
        let not_after_unix = entry.info.not_after_unix;
        state.bindings.insert(binding.clone(), thumbprint.clone());
        Ok(outcome_for(thumbprint, not_after_unix, now_unix))
    }

    fn bound_https(&self, binding: &HttpsBinding) -> WindowsSysResult<Option<Thumbprint>> {
        Ok(self.lock().bindings.get(binding).cloned())
    }
}

/// The validity of an entry at `now_unix`.
fn expiry_of(info: &CertificateInfo, now_unix: i64) -> Expiry {
    if now_unix < info.not_before_unix {
        Expiry::NotYetValid
    } else if now_unix > info.not_after_unix {
        Expiry::Expired
    } else {
        Expiry::Valid
    }
}
