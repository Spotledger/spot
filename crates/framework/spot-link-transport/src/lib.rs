//! Carriers for tenant links (WP-CTL-10.T2; ADR-058 rule 11; ADR-080).
//!
//! Two servers that may be disconnected for long periods exchange the signed messages of
//! `spot-link-exchange` over one of two carriers, chosen **per direction** of each link:
//!
//! * **`direct`**: the sender dials the peer's inbound endpoint, `POST /link/v1/{link}/messages`,
//!   over mutual TLS. The answer carries the receiver's signed receipts.
//! * **`pull`**: the side that cannot accept inbound connections long-polls the peer's mailbox,
//!   `GET /link/v1/{link}/mailbox`, hands what it took to its own `Inbox::receive`, and returns
//!   the receipts with `POST /link/v1/{link}/acks`.
//!
//! A side behind NAT therefore sends `direct` (it dials out) and receives `pull` (it dials out
//! again); its peer serves both endpoints and never dials in. See [`LinkCarriers`].
//!
//! A third, optional pair of endpoints is the host intake ([`intake`]): `POST /link/v1/handshake`
//! and `POST /link/v1/{link}/revoke`, mounted only when the host supplies a [`LinkIntake`], behind
//! their own per-peer limiter.
//!
//! # Identity comes from the transport
//!
//! The server maps the mutual-TLS client certificate to a [`Party`](spot_tenant_link::Party)
//! through a [`PeerCertificates`] directory and passes that party to `Inbox::receive` and
//! `Outbox::on_receipt`. The client's [`PeerTransport`] carries the party the host pinned for the
//! connection. Nothing in a body decides who the caller is. Messages and receipts are
//! additionally signed by the peer's pinned link key, which the exchange verifies.
//!
//! # Delivery is at least once, idempotent
//!
//! There is no transport cursor. The outbox is the cursor: a message is handed over again after
//! `resend_after_ms` until a signed receipt moves it on, and the receiver answers a repeat with
//! the earlier outcome. A crash or a lost reply on either side therefore re-delivers, and the
//! inbox's idempotency absorbs it.
//!
//! # Resilience
//!
//! The client uses the ADR-072 kit shapes re-exported by `spot-sync-http`: a per-call deadline,
//! bounded retries with injected jitter and sleeper, a per-link circuit breaker (its open period
//! is the probing period for a long-dead peer) and a shared bulkhead bounding concurrent calls
//! across links. A peer that stays unreachable surfaces as a `LINK_PEER_UNREACHABLE` condition
//! beside the stuck-message conditions of `spot-link-exchange`.
//!
//! Nothing here reads the wall clock (time comes from an injected `Clock`), draws randomness,
//! uses floats, or logs a key, a signature, a payload or a certificate.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod carrier;
pub mod client;
pub mod error;
pub mod host;
pub mod identity;
pub mod intake;
pub mod limits;
pub mod server;
pub mod wire;

pub use carrier::{CarrierTable, LinkCarriers};
pub use client::{ClientConfig, Health, LinkClient, Resilience, StepReport};
pub use error::{Error, Result, codes};
pub use host::{LinkHost, SharedSigner};
pub use identity::{PeerCertificates, PeerTransport, PinnedCertificates};
pub use intake::{IntakeCounters, IntakeRefusal, IntakeSettings, LinkIntake, NonceSweep};
pub use limits::{RateConfig, RateLimiter};
pub use server::{LinkServer, ServerSettings};

/// This crate's name, for diagnostics.
pub const CRATE_NAME: &str = "spot-link-transport";
