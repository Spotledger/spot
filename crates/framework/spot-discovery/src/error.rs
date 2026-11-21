//! Typed errors of the discovery layer, with stable `discovery.*` codes.
//!
//! Callers match on [`code()`](DiscoveryError::code), never on the text. The catalogue
//! entries live in `crates/framework/spot-errors/catalogue/discovery.toml`; every code
//! below has one entry there.

/// Everything that can go wrong in discovery, selection and pairing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscoveryError {
    /// Multicast could not be joined: the advertiser degrades, the browser cannot scan.
    #[error("multicast is unavailable: {detail}")]
    MulticastUnavailable {
        /// Plain-language cause from the mDNS daemon or the OS (no addresses, bind
        /// failure, network down). Never contains secrets.
        detail: String,
    },

    /// An advertised record is malformed or carries forbidden content.
    #[error("the discovery record is invalid: {reason}")]
    RecordInvalid {
        /// What is wrong with the record, in plain words.
        reason: String,
    },

    /// A paired server now presents a different key fingerprint.
    #[error("the server's key fingerprint changed since pairing")]
    FingerprintChanged,

    /// The entered pairing code does not match the server's current code.
    #[error("the pairing code is not correct")]
    PairingCodeInvalid,

    /// The pairing code's window passed, or a replay of a consumed confirmation.
    #[error("the pairing code or its confirmation is no longer valid")]
    PairingExpired,

    /// Too many failed attempts: the server refuses codes until the lockout ends.
    #[error("pairing is locked out after {attempts} failed attempts")]
    PairingLocked {
        /// The number of failed attempts that triggered the lockout.
        attempts: u32,
    },

    /// A manually entered address is not a valid host:port pair.
    #[error("the manual server address is invalid: {reason}")]
    ManualAddressInvalid {
        /// What is wrong with the input, in plain words.
        reason: String,
    },

    /// The trust store could not be read or written.
    #[error("the trust store at {path} is unavailable: {detail}")]
    TrustStoreUnavailable {
        /// Store file path (path only; never file contents).
        path: String,
        /// Plain-language cause.
        detail: String,
    },
}

impl DiscoveryError {
    /// The stable machine-readable code.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MulticastUnavailable { .. } => "discovery.multicast_unavailable",
            Self::RecordInvalid { .. } => "discovery.record_invalid",
            Self::FingerprintChanged => "discovery.fingerprint_changed",
            Self::PairingCodeInvalid => "discovery.pairing_code_invalid",
            Self::PairingExpired => "discovery.pairing_expired",
            Self::PairingLocked { .. } => "discovery.pairing_locked",
            Self::ManualAddressInvalid { .. } => "discovery.manual_address_invalid",
            Self::TrustStoreUnavailable { .. } => "discovery.trust_store_unavailable",
        }
    }
}
