//! The payment-provider port and a fake provider for tests and local use.
//!
//! **No card data ever reaches this crate.** A purchase goes through the provider's
//! *hosted* checkout: the store creates a [`CheckoutTicket`] with a provider reference and a
//! hosted URL, the customer pays on the provider's page, and the provider calls back with a
//! signed webhook. The store sees an amount, an opaque [`PaymentEvent::tax_lines`] value it
//! never interprets, and provider references — never a card number, never a CVV.
//!
//! [`PaymentProvider`] is the port; [`FakePaymentProvider`] is a deterministic
//! implementation that mints references, parses JSON events and records refunds and
//! cancellations in memory.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalogue::{Price, PricePlan};
use crate::error::{Result, StoreError, codes};

/// What the store asks the provider to open a hosted checkout for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutRequest {
    /// The store's checkout session id.
    pub session_id: String,
    /// The tenant buying.
    pub tenant: String,
    /// The listing bought from.
    pub listing_id: String,
    /// The app bought.
    pub app: String,
    /// The plan.
    pub plan: PricePlan,
    /// The amount, in integer minor units.
    pub amount: Price,
    /// The free trial length, in days (0 for none).
    pub trial_days: u32,
}

/// The provider's hosted checkout: an opaque reference and the URL the customer visits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckoutTicket {
    /// The provider's opaque reference for this checkout.
    pub provider_ref: String,
    /// The hosted checkout URL (on the provider's domain; the store never sees the card).
    pub url: String,
    /// When the hosted session expires (UTC seconds).
    pub expires_at: i64,
}

/// What a provider webhook reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaymentEventKind {
    /// A payment succeeded: issue or renew the entitlement.
    PaymentSucceeded,
    /// A payment failed: the entitlement lapses after grace.
    PaymentFailed,
    /// A refund succeeded: revoke the entitlement.
    RefundSucceeded,
    /// A chargeback was raised: revoke the entitlement.
    Chargeback,
    /// The subscription was cancelled: stop renewing.
    SubscriptionCancelled,
}

impl PaymentEventKind {
    /// Whether this event revokes the entitlement.
    #[must_use]
    pub fn revokes(self) -> bool {
        matches!(self, Self::RefundSucceeded | Self::Chargeback)
    }
}

/// One provider event, parsed from a verified webhook body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaymentEvent {
    /// The provider's unique event id; the idempotency key.
    pub event_id: String,
    /// What happened.
    pub kind: PaymentEventKind,
    /// The provider reference the event is about (a checkout or a subscription).
    pub provider_ref: String,
    /// The amount, when the event carries one.
    pub amount: Option<Price>,
    /// Opaque tax/VAT lines: a provider/Foundation concern this crate never interprets.
    pub tax_lines: Option<Value>,
    /// Whether this is a subscription renewal rather than the first payment.
    #[serde(default)]
    pub renewal: bool,
    /// When the provider recorded the event (UTC seconds).
    pub occurred_at: i64,
}

/// A refund request the store sends to the provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundRequest {
    /// The provider reference to refund.
    pub provider_ref: String,
    /// The amount to refund.
    pub amount: Price,
    /// Why, for the operator and the ledger.
    pub reason: String,
}

/// The provider's receipt for a refund.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefundReceipt {
    /// The provider's opaque refund reference.
    pub refund_ref: String,
    /// The refunded amount.
    pub amount: Price,
    /// When the refund was recorded (UTC seconds).
    pub at: i64,
}

/// A subscription cancellation request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CancelRequest {
    /// The subscription's provider reference.
    pub provider_ref: String,
    /// Why, for the operator.
    pub reason: String,
}

/// The payment-provider port: hosted checkout, webhook parsing, refund, cancel.
///
/// Implementations talk to a real provider over its API; this crate never does. The port
/// is synchronous and every instant is an argument, so a fake and a real adapter are driven
/// the same way.
pub trait PaymentProvider: Send + Sync {
    /// Opens a hosted checkout session.
    ///
    /// # Errors
    /// [`codes::PROVIDER_ERROR`] when the provider refuses.
    fn create_checkout(&self, request: &CheckoutRequest, now: i64) -> Result<CheckoutTicket>;

    /// Parses a **verified** webhook body into an event.
    ///
    /// The caller verifies the signature first ([`crate::webhook::WebhookVerifier`]); this
    /// method only decodes the provider's payload.
    ///
    /// # Errors
    /// [`codes::WEBHOOK_MALFORMED`] when the body is not the event shape.
    fn parse_event(&self, body: &[u8]) -> Result<PaymentEvent>;

    /// Refunds a payment.
    ///
    /// # Errors
    /// [`codes::PROVIDER_ERROR`] when the provider refuses.
    fn refund(&self, request: &RefundRequest, now: i64) -> Result<RefundReceipt>;

    /// Cancels a subscription so it will not renew.
    ///
    /// # Errors
    /// [`codes::PROVIDER_ERROR`] when the provider refuses.
    fn cancel_subscription(&self, request: &CancelRequest) -> Result<()>;
}

/// A deterministic in-memory provider for tests and local development.
#[derive(Debug, Default)]
pub struct FakePaymentProvider {
    refunds: Mutex<Vec<RefundReceipt>>,
    cancellations: Mutex<Vec<CancelRequest>>,
    /// When set, every operation is refused with [`codes::PROVIDER_ERROR`].
    fail: Mutex<bool>,
}

impl FakePaymentProvider {
    /// A provider that succeeds.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A provider that refuses every operation, to exercise the failure paths.
    #[must_use]
    pub fn failing() -> Self {
        Self {
            refunds: Mutex::new(Vec::new()),
            cancellations: Mutex::new(Vec::new()),
            fail: Mutex::new(true),
        }
    }

    fn refusing(&self) -> bool {
        *self
            .fail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The refunds the provider recorded, in order.
    #[must_use]
    pub fn refunds(&self) -> Vec<RefundReceipt> {
        self.refunds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The cancellations the provider recorded, in order.
    #[must_use]
    pub fn cancellations(&self) -> Vec<CancelRequest> {
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl PaymentProvider for FakePaymentProvider {
    fn create_checkout(&self, request: &CheckoutRequest, now: i64) -> Result<CheckoutTicket> {
        if self.refusing() {
            return Err(StoreError::new(
                codes::PROVIDER_ERROR,
                "the payment provider refused the checkout",
            ));
        }
        let provider_ref = format!("fake-session-{}", request.session_id);
        Ok(CheckoutTicket {
            url: format!("https://payments.example/checkout/{provider_ref}"),
            provider_ref,
            expires_at: now + 3_600,
        })
    }

    fn parse_event(&self, body: &[u8]) -> Result<PaymentEvent> {
        serde_json::from_slice(body).map_err(|_| {
            StoreError::new(
                codes::WEBHOOK_MALFORMED,
                "the provider webhook body is not a payment event",
            )
        })
    }

    fn refund(&self, request: &RefundRequest, now: i64) -> Result<RefundReceipt> {
        if self.refusing() {
            return Err(StoreError::new(
                codes::PROVIDER_ERROR,
                "the payment provider refused the refund",
            ));
        }
        let receipt = RefundReceipt {
            refund_ref: format!("fake-refund-{}", request.provider_ref),
            amount: request.amount,
            at: now,
        };
        self.refunds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(receipt.clone());
        Ok(receipt)
    }

    fn cancel_subscription(&self, request: &CancelRequest) -> Result<()> {
        if self.refusing() {
            return Err(StoreError::new(
                codes::PROVIDER_ERROR,
                "the payment provider refused the cancellation",
            ));
        }
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(request.clone());
        Ok(())
    }
}

/// A convenience map from provider reference to whatever the caller tracks.
pub type ProviderRefs = BTreeMap<String, String>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        CheckoutRequest, FakePaymentProvider, PaymentEvent, PaymentEventKind, PaymentProvider,
    };
    use crate::catalogue::{Interval, Price, PricePlan};
    use crate::error::codes;
    use crate::test_support::usd;

    fn request() -> CheckoutRequest {
        CheckoutRequest {
            session_id: "sess-1".to_owned(),
            tenant: "acme".to_owned(),
            listing_id: "list-1".to_owned(),
            app: "my_app".to_owned(),
            plan: PricePlan::Subscription {
                interval: Interval::Monthly,
            },
            amount: Price::new(990, usd()).unwrap(),
            trial_days: 0,
        }
    }

    #[test]
    fn the_fake_provider_opens_a_hosted_checkout() {
        let provider = FakePaymentProvider::new();
        let ticket = provider.create_checkout(&request(), 100).unwrap();
        assert_eq!(ticket.provider_ref, "fake-session-sess-1");
        assert!(ticket.url.starts_with("https://payments.example/"));
        assert_eq!(ticket.expires_at, 3_700);
    }

    #[test]
    fn a_failing_provider_refuses_every_operation() {
        let provider = FakePaymentProvider::failing();
        assert_eq!(
            provider
                .create_checkout(&request(), 100)
                .unwrap_err()
                .code(),
            codes::PROVIDER_ERROR
        );
    }

    #[test]
    fn events_parse_from_json_and_malformed_bodies_are_refused() {
        let provider = FakePaymentProvider::new();
        let body = br#"{"event_id":"evt-1","kind":"payment_succeeded","provider_ref":"fake-session-sess-1","amount":{"minor_units":990,"currency":"USD"},"tax_lines":{"vat":"x"},"renewal":false,"occurred_at":200}"#;
        let event = provider.parse_event(body).unwrap();
        assert_eq!(event.event_id, "evt-1");
        assert_eq!(event.kind, PaymentEventKind::PaymentSucceeded);
        assert!(!event.renewal);
        assert_eq!(event.amount.unwrap().minor_units(), 990);
        assert!(event.tax_lines.is_some());

        assert_eq!(
            provider.parse_event(b"not json").unwrap_err().code(),
            codes::WEBHOOK_MALFORMED
        );
    }

    #[test]
    fn a_round_trip_event_is_stable() {
        let event = PaymentEvent {
            event_id: "evt-2".to_owned(),
            kind: PaymentEventKind::RefundSucceeded,
            provider_ref: "fake-session-sess-1".to_owned(),
            amount: Some(Price::new(990, usd()).unwrap()),
            tax_lines: None,
            renewal: false,
            occurred_at: 300,
        };
        let provider = FakePaymentProvider::new();
        let body = serde_json::to_vec(&event).unwrap();
        assert_eq!(provider.parse_event(&body).unwrap(), event);
        assert!(event.kind.revokes());
    }
}
