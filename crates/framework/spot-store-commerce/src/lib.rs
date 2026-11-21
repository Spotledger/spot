//! The Spotledger app store's back end (WP-STR-08.T1; blueprint WS11, ADR-21).
//!
//! This crate is the store front's server side: the store UI pages and the publisher
//! portal screens come later, over the API this crate exposes as plain service
//! functions. Nothing here talks to a network, reads a clock or stores card data:
//! payments go through a provider's hosted flow and its signed webhooks, and every
//! instant is an argument.
//!
//! # What is here
//!
//! - [`publisher`]: publisher onboarding ([`Publisher`], verification states, signing-key
//!   registration through `spot-bundle-signing`) and the portal API in [`portal`].
//! - [`catalogue`]: [`Listing`]s with [`PricePlan`]s, browsed over `spot-registry`
//!   metadata; only reviewed and signed versions are listable.
//! - [`payment`]: the [`PaymentProvider`] port (hosted checkout, webhook parsing, refund,
//!   subscription cancel) with a [`FakePaymentProvider`] for tests and local use.
//! - [`checkout`]: [`Checkout::start`] turning a listing into a hosted
//!   [`CheckoutSession`], and [`webhook`] intake that verifies the provider signature and
//!   handles each event **idempotently**.
//! - [`licence`]: the app licence — a `spot-license` payload, minted with the same signed
//!   envelope and verified **offline** with the store's public key — and
//!   [`check_licence`], the check `spot-installer` and the runtime call at install and at
//!   start.
//! - [`entitlement`]: the entitlement a payment grants, its renewal, its grace and lapse,
//!   and the signed, distributable revocation list refunds and chargebacks add to.
//! - [`ledger`]: append-only orders, fees and payouts, with a finance export. No
//!   accounting posting happens here.
//!
//! # The end-to-end acceptance
//!
//! A paid app purchase, its licence check and its renewal work end to end: see
//! `tests/acceptance.rs`, which drives a purchase through the fake provider, a duplicate
//! webhook, a renewal, a failed payment into grace and lapse, a refund that revokes, a
//! tampered licence that is refused and an offline check with only the licence and the
//! store key.
//!
//! # Hard rules
//!
//! No card data ever reaches this crate: a [`CheckoutSession`] carries only the provider's
//! hosted URL and an opaque provider reference, and the webhook carries an opaque
//! `tax_lines` value the store never interprets. No floats (prices are integer minor
//! units, `spot-money` does the arithmetic), no wall clock (every instant is an argument),
//! no `unsafe`, and no secret in a message or a `Debug` output.

#![forbid(unsafe_code)]

pub mod catalogue;
pub mod checkout;
pub mod entitlement;
pub mod error;
pub mod ledger;
pub mod licence;
pub mod payment;
pub mod portal;
pub mod publisher;
pub mod store;
pub mod webhook;

#[cfg(test)]
pub(crate) mod test_support;

pub use catalogue::{
    Catalogue, Interval, Listing, ListingState, ListingVersion, Price, PricePlan, ReviewStatus,
};
pub use checkout::{Checkout, CheckoutSession, SessionState};
pub use entitlement::{
    Entitlement, EntitlementRevocation, EntitlementState, EntitlementStore, RevocationBody,
    RevocationList, issue_revocation_list,
};
pub use error::{StoreError, codes};
pub use ledger::{CurrencyNet, Fee, FeeKind, FinanceExport, Ledger, Order, OrderKind, Payout};
pub use licence::{
    APP_LICENCE_FEATURE, AppLicence, LicenceCheck, LicenceIssuer, SignedAppLicence, StoreSigner,
    check_licence, check_licence_text, check_licence_with_state, default_grace, trusted_key,
    trusted_keys,
};
pub use payment::{
    CancelRequest, CheckoutRequest, CheckoutTicket, FakePaymentProvider, PaymentEvent,
    PaymentEventKind, PaymentProvider, RefundReceipt, RefundRequest,
};
pub use portal::{
    FakeReviewService, Portal, ReviewOutcome, ReviewService, ReviewVerdict, SpotReviewService,
    Submission,
};
pub use publisher::{
    Publisher, PublisherRegistry, PublisherStatus, SigningKeyRef, VerificationState,
};
pub use store::{Sellable, Store, WebhookOutcome};
pub use webhook::{
    DEFAULT_TOLERANCE_SECS, EventIdStore, MemoryEventIdStore, WebhookVerifier, sign_payload,
};

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-store-commerce";

/// The crate's semver-excluded version, for the error catalogue's `since` rows.
pub const VERSION: &str = "0.1.0";
