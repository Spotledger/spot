//! LAN discovery for Spotledger (WP-PRD-06.T1): a shop or office runs the service host
//! on one PC and every other PC finds it without anyone typing an address.
//!
//! The pieces:
//! - [`record`]: the deliberately small `Advertisement` encoded as mDNS TXT keys, and
//!   what the config may reveal (`reveal_site_names`) — nothing about tenants,
//!   companies or users by default.
//! - [`advertiser`]: the service side. Registers the record under
//!   `_spotledger._tcp.local.`, re-announces on address changes, and withdraws it
//!   (goodbye) on `stop()` or drop. Multicast failure is a `Degraded` handle, never a
//!   crashed service (ADR-068 rule A1).
//! - [`browser`]: the client side. `scan` for a one-shot list, `watch` for a stream of
//!   appeared/updated/disappeared events; every [`Found`] carries a stable
//!   [`ServerId`] (the service instance, never the IP).
//! - [`select`]: the pure choice of which server to connect to: last used wins, one
//!   found means select it, several means `NeedsChoice`, none means `NoneFound` — a
//!   guess is never made.
//! - [`manual`]: the `host:port` fallback for networks without multicast.
//! - [`trust`]: the trust model behind "TLS optional on LAN": TLS modes, key
//!   fingerprints, pairing codes with HMAC-SHA-256 confirmation MACs, the rate-limited
//!   attempt counter, and the `TrustStore` (in-memory and atomic file) that refuses a
//!   changed fingerprint with `discovery.fingerprint_changed`.
//!
//! This crate implements no TLS, no installer, no firewall rules, no service wiring and
//! no UI. Clocks and randomness are injected (`spot_core::Clock`, [`random::
//! RandomSource`]) so every unit test is deterministic and no test sleeps.
//!
//! Discovery finds a server; it never authenticates anyone (ADR-070) and it never
//! advertises or connects to the database ([WP-PRD-02] keeps it localhost-bound).

pub mod advertiser;
pub mod browser;
pub mod error;
pub mod manual;
pub mod random;
pub mod record;
pub mod select;
pub mod trust;

pub use advertiser::{Advertiser, AdvertiserHandle, AdvertiserState, AdvertiserStatus};
pub use browser::{Browser, Found, FoundEvent, FoundSource};
pub use error::DiscoveryError;
pub use manual::parse_manual;
pub use random::{FixedRandom, RandomSource};
pub use record::{
    AdvertisedSiteNames, Advertisement, AdvertiserConfig, Fingerprint, ProtocolVersion, ServerId,
    TlsMode,
};
pub use select::{FoundServer, ManualServer, Selection, SelectionPolicy, pick};
pub use trust::{
    PairingAttemptCounter, PairingClient, PairingSession, TlsModeState, TrustStore, TrustedServer,
    Unprotected, key_fingerprint, pairing_code, pairing_confirm_mac, verify_paired,
};

/// The mDNS service type every Spotledger server is advertised under.
pub const SERVICE_TYPE: &str = "_spotledger._tcp.local.";

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-discovery";
