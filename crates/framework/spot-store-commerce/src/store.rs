//! The store facade: the components wired together into one back end.
//!
//! [`Store`] owns the publisher registry, the catalogue, checkout sessions, entitlements,
//! the ledger and the signed revocation list, and it is the one place the payment flow is
//! driven. A purchase is: [`Store::start_checkout`] (a hosted provider session), a verified
//! [`Store::handle_webhook`] (which issues the signed app licence, records the order and its
//! fees), and [`Store::check_licence`] offline at install and at start. A renewal is a
//! subscription `payment_succeeded` event; a refund or chargeback revokes the entitlement
//! and appends a signed revocation.

use std::sync::Mutex;

use spot_bundle_signing::PublicKey;
use spot_license::{SignedLicence, TrustedKeys};

use crate::catalogue::{Listing, Price, PricePlan};
use crate::checkout::{Checkout, CheckoutSession, SessionState};
use crate::entitlement::{
    Entitlement, EntitlementRevocation, EntitlementStore, RevocationBody, RevocationList,
    issue_revocation_list,
};
use crate::error::{Result, StoreError, codes};
use crate::ledger::{Fee, FeeKind, Ledger, Order, OrderKind, Payout};
use crate::licence::{
    AppLicence, LicenceCheck, LicenceIssuer, SignedAppLicence, StoreSigner, default_grace,
    trusted_keys,
};
use crate::payment::{
    CancelRequest, PaymentEvent, PaymentEventKind, PaymentProvider, RefundReceipt, RefundRequest,
};
use crate::portal::{Portal, ReviewService, Submission};
use crate::publisher::{Publisher, PublisherRegistry, SigningKeyRef};
use crate::webhook::{EventIdStore, MemoryEventIdStore, WebhookVerifier};

/// What handling one provider webhook did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookOutcome {
    /// The provider event id.
    pub event_id: String,
    /// What kind of event it was.
    pub kind: PaymentEventKind,
    /// Whether the event was applied; `false` for a duplicate delivery.
    pub applied: bool,
    /// The order the event produced, when it produced one.
    pub order_id: Option<String>,
    /// The entitlement the event touched, when there was one.
    pub entitlement: Option<Entitlement>,
}

/// The app store's back end.
pub struct Store {
    signer: Box<dyn StoreSigner>,
    root: PublicKey,
    trusted: TrustedKeys,
    provider: Box<dyn PaymentProvider>,
    verifier: WebhookVerifier,
    events: Box<dyn EventIdStore>,
    review: Box<dyn ReviewService>,
    publishers: PublisherRegistry,
    catalogue: crate::catalogue::Catalogue,
    checkout: Checkout,
    entitlements: EntitlementStore,
    ledger: Ledger,
    revocations: Mutex<Option<RevocationList>>,
    generation: Mutex<u64>,
    provider_fee_bps: i64,
    platform_fee_bps: i64,
}

impl Store {
    /// Wires a store. The trusted-key list is derived from `signer`, so verification uses
    /// exactly the key the store mints with.
    ///
    /// # Errors
    /// [`codes::SIGNING_ERROR`] when the signer's public key is not a valid curve point.
    pub fn new(
        signer: Box<dyn StoreSigner>,
        provider: Box<dyn PaymentProvider>,
        verifier: WebhookVerifier,
        review: Box<dyn ReviewService>,
    ) -> Result<Self> {
        let root = PublicKey::from_bytes(&signer.public_key_bytes())?;
        let trusted = trusted_keys(&[signer.as_ref()])?;
        Ok(Self {
            signer,
            root,
            trusted,
            provider,
            verifier,
            events: Box::new(MemoryEventIdStore::new()),
            review,
            publishers: PublisherRegistry::new(),
            catalogue: crate::catalogue::Catalogue::new(),
            checkout: Checkout::new(),
            entitlements: EntitlementStore::new(),
            ledger: Ledger::new(),
            revocations: Mutex::new(None),
            generation: Mutex::new(0),
            provider_fee_bps: 300,
            platform_fee_bps: 500,
        })
    }

    /// Replaces the event-id store (for a durable adapter).
    #[must_use]
    pub fn with_events(mut self, events: Box<dyn EventIdStore>) -> Self {
        self.events = events;
        self
    }

    /// Sets the provider and platform fee rates, in basis points of the order amount.
    #[must_use]
    pub fn with_fee_rates(mut self, provider_bps: i64, platform_bps: i64) -> Self {
        self.provider_fee_bps = provider_bps.max(0);
        self.platform_fee_bps = platform_bps.max(0);
        self
    }

    /// The publisher registry.
    #[must_use]
    pub fn publishers(&self) -> &PublisherRegistry {
        &self.publishers
    }

    /// The catalogue.
    #[must_use]
    pub fn catalogue(&self) -> &crate::catalogue::Catalogue {
        &self.catalogue
    }

    /// The checkout sessions.
    #[must_use]
    pub fn checkout(&self) -> &Checkout {
        &self.checkout
    }

    /// The entitlements.
    #[must_use]
    pub fn entitlements(&self) -> &EntitlementStore {
        &self.entitlements
    }

    /// The revenue ledger.
    #[must_use]
    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    /// The store's trusted public keys (the offline trust anchor).
    #[must_use]
    pub fn trusted_keys(&self) -> &TrustedKeys {
        &self.trusted
    }

    /// The store's signing key, for a caller that needs to mint its own envelope.
    #[must_use]
    pub fn signer(&self) -> &dyn StoreSigner {
        self.signer.as_ref()
    }

    /// The latest signed entitlement revocation list, if any has been issued.
    #[must_use]
    pub fn revocations(&self) -> Option<RevocationList> {
        self.revocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// A portal over this store's components.
    #[must_use]
    pub fn portal(&self) -> Portal<'_> {
        Portal::new(&self.publishers, &self.catalogue, self.review.as_ref())
    }

    /// Onboards a publisher (see [`PublisherRegistry::onboard`]).
    ///
    /// # Errors
    /// As [`PublisherRegistry::onboard`].
    pub fn onboard_publisher(
        &self,
        id: &str,
        org: &str,
        payout_ref: &str,
        now: i64,
    ) -> Result<Publisher> {
        self.publishers.onboard(id, org, payout_ref, now)
    }

    /// Registers a publisher signing key (see [`PublisherRegistry::register_signing_key`]).
    ///
    /// # Errors
    /// As [`PublisherRegistry::register_signing_key`].
    pub fn register_signing_key(
        &self,
        publisher: &str,
        public_key: &[u8],
        now: i64,
    ) -> Result<SigningKeyRef> {
        self.publishers
            .register_signing_key(publisher, public_key, now)
    }

    /// Starts a hosted checkout for a listing.
    ///
    /// # Errors
    /// [`codes::LISTING_UNKNOWN`], or as [`Checkout::start`].
    pub fn start_checkout(
        &self,
        session_id: &str,
        tenant: &str,
        listing_id: &str,
        region: Option<&str>,
        now: i64,
    ) -> Result<CheckoutSession> {
        let listing = self.catalogue.get(listing_id)?;
        self.checkout.start(
            session_id,
            tenant,
            &listing,
            region,
            self.provider.as_ref(),
            now,
        )
    }

    /// Submits an app version through the portal.
    ///
    /// # Errors
    /// As [`Portal::submit_version`].
    pub fn submit_version(
        &self,
        submission: &Submission,
        now: i64,
    ) -> Result<crate::catalogue::ListingVersion> {
        self.portal().submit_version(submission, now)
    }

    /// A store reviewer approves an app version through the portal.
    ///
    /// # Errors
    /// As [`Portal::approve_version`].
    pub fn approve_version(
        &self,
        reviewer: &str,
        listing_id: &str,
        version: &spot_registry::Version,
        now: i64,
    ) -> Result<crate::catalogue::ListingVersion> {
        self.portal()
            .approve_version(reviewer, listing_id, version, now)
    }

    /// Handles one provider webhook: verifies the signature, deduplicates by event id and
    /// applies the event.
    ///
    /// # Errors
    /// [`codes::WEBHOOK_SIGNATURE_INVALID`] or [`codes::WEBHOOK_STALE`] when the signature
    /// does not verify, [`codes::WEBHOOK_MALFORMED`] when the body is not an event or names
    /// no known session, or the refusal of the operation the event triggers.
    pub fn handle_webhook(&self, header: &str, body: &[u8], now: i64) -> Result<WebhookOutcome> {
        self.verifier.verify(header, body, now)?;
        let event = self.provider.parse_event(body)?;
        if !self.events.record(&event.event_id, now) {
            let order_id = self
                .ledger
                .order_by_event(&event.event_id)
                .map(|o| o.order_id);
            return Ok(WebhookOutcome {
                event_id: event.event_id,
                kind: event.kind,
                applied: false,
                order_id,
                entitlement: self.entitlement_for_ref(&event.provider_ref),
            });
        }
        let session = self.checkout.by_provider_ref(&event.provider_ref);
        if session.is_none()
            && matches!(
                event.kind,
                PaymentEventKind::PaymentSucceeded
                    | PaymentEventKind::RefundSucceeded
                    | PaymentEventKind::Chargeback
            )
        {
            // Nothing was applied: do not let the retry of money-moving event be taken for a
            // duplicate and lost.
            self.events.forget(&event.event_id);
        }
        match event.kind {
            PaymentEventKind::PaymentSucceeded => self.apply_payment(&event, session.as_ref(), now),
            PaymentEventKind::PaymentFailed => {
                if let Some(session) = &session {
                    self.checkout.mark_failed(&session.id)?;
                }
                Ok(WebhookOutcome {
                    event_id: event.event_id,
                    kind: event.kind,
                    applied: true,
                    order_id: None,
                    entitlement: session
                        .as_ref()
                        .and_then(|s| self.entitlements.get(&s.tenant, &s.app).ok()),
                })
            }
            PaymentEventKind::RefundSucceeded | PaymentEventKind::Chargeback => {
                self.apply_revocation(&event, session.as_ref(), now)
            }
            PaymentEventKind::SubscriptionCancelled => {
                if let Some(session) = &session {
                    self.checkout.mark_cancelled(&session.id)?;
                }
                Ok(WebhookOutcome {
                    event_id: event.event_id,
                    kind: event.kind,
                    applied: true,
                    order_id: None,
                    entitlement: session
                        .as_ref()
                        .and_then(|s| self.entitlements.get(&s.tenant, &s.app).ok()),
                })
            }
        }
    }

    fn entitlement_for_ref(&self, provider_ref: &str) -> Option<Entitlement> {
        self.checkout
            .by_provider_ref(provider_ref)
            .and_then(|session| self.entitlements.get(&session.tenant, &session.app).ok())
    }

    fn apply_payment(
        &self,
        event: &PaymentEvent,
        session: Option<&CheckoutSession>,
        now: i64,
    ) -> Result<WebhookOutcome> {
        let session = session.ok_or_else(|| {
            StoreError::new(
                codes::WEBHOOK_MALFORMED,
                "the payment event names no known checkout session",
            )
        })?;
        let listing = self.catalogue.get(&session.listing_id)?;
        let existing = self.entitlements.get(&session.tenant, &session.app).ok();
        let (entitlement, order_kind, serial) = match existing {
            Some(current) => {
                let period = if current.interval_seconds > 0 {
                    current.interval_seconds
                } else {
                    listing
                        .price_plan
                        .interval()
                        .map_or(0, crate::catalogue::Interval::seconds)
                };
                let base = current.valid_until.unwrap_or(now).max(now);
                let valid_until = base.saturating_add(period);
                let serial = current.serial.saturating_add(1);
                let renewed = self.entitlements.renew(
                    &session.tenant,
                    &session.app,
                    valid_until,
                    serial,
                    now,
                )?;
                (renewed, OrderKind::Renewal, serial)
            }
            None => {
                let plan = listing.price_plan;
                let kind = plan.licence_kind();
                let grace = if kind == spot_license::Kind::Subscription {
                    default_grace()
                } else {
                    0
                };
                let interval_seconds = plan
                    .interval()
                    .map_or(0, crate::catalogue::Interval::seconds);
                let valid_until = if kind == spot_license::Kind::Perpetual {
                    None
                } else {
                    let trial = i64::from(session.trial_days) * 86_400;
                    Some(now.saturating_add(trial).saturating_add(interval_seconds))
                };
                let entitlement = Entitlement {
                    entitlement_id: format!("ent-{}", session.id),
                    tenant: session.tenant.clone(),
                    app: session.app.clone(),
                    listing_id: session.listing_id.clone(),
                    plan: plan_slug(plan),
                    currency: session.amount.currency(),
                    licence_id: format!("lic-{}", session.id),
                    serial: 1,
                    valid_until,
                    grace,
                    licence_kind: kind,
                    interval_seconds,
                    revoked: false,
                    updated_at: now,
                };
                let entitlement = self.entitlements.grant(entitlement)?;
                (entitlement, OrderKind::Purchase, 1)
            }
        };

        // Mint the signed app licence the tenant installs and verifies offline.
        let app_licence = entitlement.app_licence(serial, now)?;
        let _signed = LicenceIssuer::new(self.signer.as_ref()).issue(&app_licence)?;

        let order_id = format!("order-{}-{serial}", session.id);
        let order = Order {
            order_id: order_id.clone(),
            tenant: session.tenant.clone(),
            listing_id: session.listing_id.clone(),
            app: session.app.clone(),
            publisher: listing.publisher.clone(),
            plan: entitlement.plan.clone(),
            amount: session.amount,
            tax_lines: event.tax_lines.clone(),
            provider_ref: event.provider_ref.clone(),
            event_id: event.event_id.clone(),
            kind: order_kind,
            at: now,
        };
        self.ledger.record_order(order)?;
        self.record_fees(&order_id, session.amount, now)?;
        if session.state == SessionState::Pending {
            self.checkout.mark_paid(&session.id, &order_id, now)?;
        }
        Ok(WebhookOutcome {
            event_id: event.event_id.clone(),
            kind: event.kind,
            applied: true,
            order_id: Some(order_id),
            entitlement: Some(entitlement),
        })
    }

    fn apply_revocation(
        &self,
        event: &PaymentEvent,
        session: Option<&CheckoutSession>,
        now: i64,
    ) -> Result<WebhookOutcome> {
        let session = session.ok_or_else(|| {
            StoreError::new(
                codes::WEBHOOK_MALFORMED,
                "the revocation event names no known checkout session",
            )
        })?;
        let entitlement = self
            .entitlements
            .revoke(&session.tenant, &session.app, now)?;
        self.append_revocation(&entitlement, now, revocation_reason(event.kind))?;
        let order_id = format!("order-{}-refund-{}", session.id, event.event_id);
        let order = Order {
            order_id: order_id.clone(),
            tenant: session.tenant.clone(),
            listing_id: session.listing_id.clone(),
            app: session.app.clone(),
            publisher: self
                .catalogue
                .get(&session.listing_id)
                .map_or_else(|_| String::new(), |listing| listing.publisher),
            plan: entitlement.plan.clone(),
            amount: session.amount,
            tax_lines: event.tax_lines.clone(),
            provider_ref: event.provider_ref.clone(),
            event_id: event.event_id.clone(),
            kind: OrderKind::Refund,
            at: now,
        };
        self.ledger.record_order(order)?;
        if session.state == SessionState::Paid {
            self.checkout.mark_refunded(&session.id)?;
        }
        Ok(WebhookOutcome {
            event_id: event.event_id.clone(),
            kind: event.kind,
            applied: true,
            order_id: Some(order_id),
            entitlement: Some(entitlement),
        })
    }

    /// Refunds a paid session through the provider and revokes the entitlement.
    ///
    /// # Errors
    /// [`codes::SESSION_UNKNOWN`], [`codes::ENTITLEMENT_UNKNOWN`],
    /// [`codes::PROVIDER_ERROR`], or a ledger refusal.
    pub fn refund(&self, session_id: &str, reason: &str, now: i64) -> Result<RefundReceipt> {
        let session = self.checkout.get(session_id)?;
        let receipt = self.provider.refund(
            &RefundRequest {
                provider_ref: session.provider_ref.clone(),
                amount: session.amount,
                reason: reason.to_owned(),
            },
            now,
        )?;
        let entitlement = self
            .entitlements
            .revoke(&session.tenant, &session.app, now)?;
        self.append_revocation(&entitlement, now, reason)?;
        let order = Order {
            order_id: format!("order-{}-refund", session.id),
            tenant: session.tenant.clone(),
            listing_id: session.listing_id.clone(),
            app: session.app.clone(),
            publisher: self
                .catalogue
                .get(&session.listing_id)
                .map_or_else(|_| String::new(), |listing| listing.publisher),
            plan: entitlement.plan.clone(),
            amount: session.amount,
            tax_lines: None,
            provider_ref: receipt.refund_ref.clone(),
            event_id: format!("refund-{}", receipt.refund_ref),
            kind: OrderKind::Refund,
            at: now,
        };
        self.ledger.record_order(order)?;
        if session.state == SessionState::Paid {
            self.checkout.mark_refunded(&session.id)?;
        }
        Ok(receipt)
    }

    /// Cancels a subscription through the provider; the entitlement runs to its expiry.
    ///
    /// # Errors
    /// [`codes::SESSION_UNKNOWN`] or [`codes::PROVIDER_ERROR`].
    pub fn cancel_subscription(&self, session_id: &str, reason: &str, now: i64) -> Result<()> {
        let session = self.checkout.get(session_id)?;
        self.provider.cancel_subscription(&CancelRequest {
            provider_ref: session.provider_ref.clone(),
            reason: reason.to_owned(),
        })?;
        self.checkout.mark_cancelled(&session.id)?;
        let _ = now;
        Ok(())
    }

    /// Issues the current signed app licence for a tenant's entitlement.
    ///
    /// # Errors
    /// [`codes::ENTITLEMENT_UNKNOWN`] or [`codes::LICENCE_INVALID`].
    pub fn issue_licence(&self, tenant: &str, app: &str, now: i64) -> Result<SignedAppLicence> {
        let entitlement = self.entitlements.get(tenant, app)?;
        let app_licence = entitlement.app_licence(entitlement.serial, now)?;
        LicenceIssuer::new(self.signer.as_ref()).issue(&app_licence)
    }

    /// The app-licence view a tenant currently holds.
    ///
    /// # Errors
    /// [`codes::ENTITLEMENT_UNKNOWN`].
    pub fn entitlement_licence(&self, tenant: &str, app: &str) -> Result<AppLicence> {
        let entitlement = self.entitlements.get(tenant, app)?;
        entitlement.app_licence(entitlement.serial, entitlement.updated_at)
    }

    /// Checks an app licence offline, consulting the latest signed revocation list.
    ///
    /// # Errors
    /// As [`crate::licence::check_licence_with_state`].
    pub fn check_licence(
        &self,
        signed: &SignedAppLicence,
        tenant: &str,
        app: &str,
        now: i64,
    ) -> Result<LicenceCheck> {
        let revocations = self
            .revocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        crate::licence::check_licence_with_state(
            signed,
            &self.trusted,
            tenant,
            app,
            now,
            None,
            revocations.as_ref(),
            Some(&self.root),
        )
    }

    /// Checks the `SPOT-LICENCE-1` text form of an app licence offline.
    ///
    /// # Errors
    /// As [`crate::licence::check_licence_text`].
    pub fn check_licence_text(
        &self,
        text: &str,
        tenant: &str,
        app: &str,
        now: i64,
    ) -> Result<LicenceCheck> {
        let signed = SignedAppLicence::from_text(text)?;
        self.check_licence(&signed, tenant, app, now)
    }

    /// Records a payout to a publisher (see [`Ledger::record_payout`]).
    ///
    /// # Errors
    /// As [`Ledger::record_payout`].
    pub fn record_payout(&self, payout: Payout) -> Result<Payout> {
        self.ledger.record_payout(payout)
    }

    /// Exports the revenue ledger for finance.
    ///
    /// # Errors
    /// As [`Ledger::export`].
    pub fn export_finance(
        &self,
        table: &spot_money::CurrencyTable,
    ) -> Result<crate::ledger::FinanceExport> {
        self.ledger.export(table)
    }

    fn record_fees(&self, order_id: &str, amount: Price, now: i64) -> Result<()> {
        for (kind, bps, label) in [
            (FeeKind::Provider, self.provider_fee_bps, "provider"),
            (FeeKind::Platform, self.platform_fee_bps, "platform"),
        ] {
            let fee_minor = fee_minor_units(amount.minor_units(), bps);
            if fee_minor == 0 {
                continue;
            }
            self.ledger.record_fee(Fee {
                fee_id: format!("{order_id}-{label}"),
                order_id: order_id.to_owned(),
                kind,
                amount: Price::new(fee_minor, amount.currency())?,
                at: now,
            })?;
        }
        Ok(())
    }

    fn append_revocation(&self, entitlement: &Entitlement, now: i64, reason: &str) -> Result<()> {
        let mut generation = self
            .generation
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *generation = generation.saturating_add(1);
        let mut entries = self
            .revocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map_or_else(Vec::new, |list| list.body().entries.clone());
        entries.push(EntitlementRevocation {
            licence_id: entitlement.licence_id.clone(),
            tenant: entitlement.tenant.clone(),
            app: entitlement.app.clone(),
            effective_from: now,
            reason: reason.to_owned(),
        });
        let body = RevocationBody {
            generation: *generation,
            issued_at: now,
            issued_by: self.signer.key_id().to_owned(),
            entries,
        };
        let list = issue_revocation_list(body, self.signer.as_ref());
        *self
            .revocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(list);
        Ok(())
    }

    /// The raw signed envelope for a text form, for a caller that wants the `spot-license`
    /// type directly.
    ///
    /// # Errors
    /// [`codes::LICENCE_INVALID`] when the text is malformed.
    pub fn parse_licence_text(text: &str) -> Result<SignedLicence> {
        SignedLicence::from_text(text).map_err(StoreError::from)
    }
}

/// The plan slug an app licence carries for a price plan.
fn plan_slug(plan: PricePlan) -> String {
    match plan {
        PricePlan::Free => "app_free".to_owned(),
        PricePlan::OneTime => "app_one_time".to_owned(),
        PricePlan::Subscription {
            interval: crate::catalogue::Interval::Monthly,
        } => "app_monthly".to_owned(),
        PricePlan::Subscription {
            interval: crate::catalogue::Interval::Annual,
        } => "app_annual".to_owned(),
    }
}

/// A fee in minor units: `amount * bps / 10_000`, rounded down, exactly (no floats).
fn fee_minor_units(amount_minor: i64, bps: i64) -> i64 {
    if bps <= 0 || amount_minor <= 0 {
        return 0;
    }
    let product = i128::from(amount_minor) * i128::from(bps);
    i64::try_from(product / 10_000).unwrap_or(i64::MAX)
}

/// The operator-facing reason a revocation event carries.
fn revocation_reason(kind: PaymentEventKind) -> &'static str {
    match kind {
        PaymentEventKind::Chargeback => "chargeback",
        _ => "refund",
    }
}

/// A listing the store can sell, for the UI: the app, its plan and price.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sellable {
    /// The app.
    pub app: String,
    /// The listing id.
    pub listing_id: String,
    /// The plan.
    pub plan: PricePlan,
    /// The price, for a paid plan.
    pub price: Option<Price>,
    /// The free trial length, in days.
    pub trial_days: u32,
}

impl From<&Listing> for Sellable {
    fn from(listing: &Listing) -> Self {
        Self {
            app: listing.app.as_str().to_owned(),
            listing_id: listing.id.clone(),
            plan: listing.price_plan,
            price: listing.price,
            trial_days: listing.trial_days,
        }
    }
}
