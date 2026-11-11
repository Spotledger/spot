//! Checkout: turning a paid listing into a hosted provider session.
//!
//! [`Checkout::start`] refuses a free listing (there is nothing to pay for), refuses a
//! listing that is not on sale, and asks the [`PaymentProvider`] for a hosted checkout. The
//! session it records carries **no card data**: only the provider's opaque reference and
//! the hosted URL. A verified webhook (see [`crate::webhook`]) moves the session to
//! [`SessionState::Paid`] and issues the entitlement.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::catalogue::{Listing, Price, PricePlan};
use crate::error::{Result, StoreError, codes};
use crate::payment::{CheckoutRequest, PaymentProvider};
use crate::publisher::check_identifier;

/// Where a checkout session is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Created; the customer has not paid yet.
    Pending,
    /// Paid; the entitlement has been issued.
    Paid,
    /// The payment failed.
    Failed,
    /// The payment was refunded and the entitlement revoked.
    Refunded,
    /// The subscription was cancelled.
    Cancelled,
}

/// One hosted checkout session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckoutSession {
    /// The store's session id.
    pub id: String,
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
    /// The free trial length, in days.
    pub trial_days: u32,
    /// The provider's opaque reference.
    pub provider_ref: String,
    /// The hosted checkout URL.
    pub url: String,
    /// The session state.
    pub state: SessionState,
    /// When the session was created (UTC seconds).
    pub created_at: i64,
    /// When the hosted session expires (UTC seconds).
    pub expires_at: i64,
    /// The order the paid session produced, once paid.
    pub order_id: Option<String>,
}

impl CheckoutSession {
    /// Whether the session is still awaiting payment at `now`.
    #[must_use]
    pub fn pending_at(&self, now: i64) -> bool {
        self.state == SessionState::Pending && now < self.expires_at
    }
}

/// The store's checkout sessions.
#[derive(Debug, Default)]
pub struct Checkout {
    sessions: Mutex<BTreeMap<String, CheckoutSession>>,
}

impl Checkout {
    /// An empty set of sessions.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, CheckoutSession>> {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Starts a hosted checkout for a paid listing.
    ///
    /// # Errors
    /// [`codes::INVALID_INPUT`] for a bad session id or tenant,
    /// [`codes::LISTING_FREE`] for a free listing, [`codes::LISTING_UNREVIEWED`] when the
    /// listing is not on sale, or [`codes::PROVIDER_ERROR`] when the provider refuses.
    #[allow(clippy::too_many_arguments)]
    pub fn start(
        &self,
        session_id: &str,
        tenant: &str,
        listing: &Listing,
        region: Option<&str>,
        provider: &dyn PaymentProvider,
        now: i64,
    ) -> Result<CheckoutSession> {
        check_identifier(session_id, "session id")?;
        check_identifier(tenant, "tenant")?;
        if !listing.price_plan.is_paid() {
            return Err(StoreError::new(
                codes::LISTING_FREE,
                "the listing is free; there is nothing to check out",
            ));
        }
        if !listing.sells_in(region) {
            return Err(StoreError::new(
                codes::LISTING_UNREVIEWED,
                "the listing is not on sale",
            ));
        }
        let amount = listing.price.ok_or_else(|| {
            StoreError::new(codes::PRICE_INVALID, "the paid listing has no price")
        })?;
        let request = CheckoutRequest {
            session_id: session_id.to_owned(),
            tenant: tenant.to_owned(),
            listing_id: listing.id.clone(),
            app: listing.app.as_str().to_owned(),
            plan: listing.price_plan,
            amount,
            trial_days: listing.trial_days,
        };
        let ticket = provider.create_checkout(&request, now)?;
        let session = CheckoutSession {
            id: session_id.to_owned(),
            tenant: tenant.to_owned(),
            listing_id: listing.id.clone(),
            app: listing.app.as_str().to_owned(),
            plan: listing.price_plan,
            amount,
            trial_days: listing.trial_days,
            provider_ref: ticket.provider_ref,
            url: ticket.url,
            state: SessionState::Pending,
            created_at: now,
            expires_at: ticket.expires_at,
            order_id: None,
        };
        let mut sessions = self.lock();
        if sessions.contains_key(session_id) {
            return Err(StoreError::new(
                codes::INVALID_INPUT,
                "the session id is already used",
            ));
        }
        sessions.insert(session_id.to_owned(), session.clone());
        Ok(session)
    }

    /// The session with the given id.
    ///
    /// # Errors
    /// [`codes::SESSION_UNKNOWN`].
    pub fn get(&self, id: &str) -> Result<CheckoutSession> {
        self.lock()
            .get(id)
            .cloned()
            .ok_or_else(|| StoreError::missing(codes::SESSION_UNKNOWN, "the checkout session"))
    }

    /// The session a provider reference names, if any.
    #[must_use]
    pub fn by_provider_ref(&self, provider_ref: &str) -> Option<CheckoutSession> {
        self.lock()
            .values()
            .find(|session| session.provider_ref == provider_ref)
            .cloned()
    }

    /// The tenant's sessions, newest last.
    #[must_use]
    pub fn for_tenant(&self, tenant: &str) -> Vec<CheckoutSession> {
        self.lock()
            .values()
            .filter(|session| session.tenant == tenant)
            .cloned()
            .collect()
    }

    fn transition(
        &self,
        id: &str,
        apply: impl FnOnce(&mut CheckoutSession) -> Result<()>,
    ) -> Result<CheckoutSession> {
        let mut sessions = self.lock();
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| StoreError::missing(codes::SESSION_UNKNOWN, "the checkout session"))?;
        apply(session)?;
        Ok(session.clone())
    }

    /// Marks a pending session paid and records its order id.
    ///
    /// # Errors
    /// [`codes::SESSION_UNKNOWN`], or [`codes::SESSION_NOT_PENDING`] when it is not pending.
    pub fn mark_paid(&self, id: &str, order_id: &str, now: i64) -> Result<CheckoutSession> {
        self.transition(id, |session| {
            if session.state != SessionState::Pending {
                return Err(StoreError::new(
                    codes::SESSION_NOT_PENDING,
                    "the checkout session is not pending",
                ));
            }
            session.state = SessionState::Paid;
            session.order_id = Some(order_id.to_owned());
            let _ = now;
            Ok(())
        })
    }

    /// Marks a pending session failed.
    ///
    /// # Errors
    /// [`codes::SESSION_UNKNOWN`].
    pub fn mark_failed(&self, id: &str) -> Result<CheckoutSession> {
        self.transition(id, |session| {
            session.state = SessionState::Failed;
            Ok(())
        })
    }

    /// Marks a paid session refunded.
    ///
    /// # Errors
    /// [`codes::SESSION_UNKNOWN`].
    pub fn mark_refunded(&self, id: &str) -> Result<CheckoutSession> {
        self.transition(id, |session| {
            session.state = SessionState::Refunded;
            Ok(())
        })
    }

    /// Marks a session's subscription cancelled.
    ///
    /// # Errors
    /// [`codes::SESSION_UNKNOWN`].
    pub fn mark_cancelled(&self, id: &str) -> Result<CheckoutSession> {
        self.transition(id, |session| {
            session.state = SessionState::Cancelled;
            Ok(())
        })
    }

    /// Every session, in id order.
    #[must_use]
    pub fn all(&self) -> Vec<CheckoutSession> {
        self.lock().values().cloned().collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Checkout, SessionState};
    use crate::catalogue::{Catalogue, Interval, Price, PricePlan};
    use crate::error::codes;
    use crate::payment::FakePaymentProvider;
    use crate::test_support::{release, usd};

    fn paid_listing(catalogue: &Catalogue) {
        catalogue
            .create("list-1", "my_app", "acme", "My App", "", "tools", 100)
            .unwrap();
        catalogue
            .set_pricing(
                "list-1",
                PricePlan::Subscription {
                    interval: Interval::Monthly,
                },
                Some(Price::new(990, usd()).unwrap()),
                0,
                vec![],
                110,
            )
            .unwrap();
        catalogue
            .add_version("list-1", release("my_app", "acme", "1.0.0"), 120)
            .unwrap();
        catalogue
            .approve_version("list-1", &"1.0.0".parse().unwrap(), None, 130)
            .unwrap();
    }

    #[test]
    fn starting_a_checkout_needs_a_paid_published_listing() {
        let catalogue = Catalogue::new();
        catalogue
            .create("free-1", "free_app", "acme", "Free App", "", "tools", 100)
            .unwrap();
        let provider = FakePaymentProvider::new();
        let checkout = Checkout::new();
        let free = catalogue.get("free-1").unwrap();
        assert_eq!(
            checkout
                .start("sess-0", "acme", &free, None, &provider, 200)
                .unwrap_err()
                .code(),
            codes::LISTING_FREE
        );
        paid_listing(&catalogue);
        let listing = catalogue.get("list-1").unwrap();
        let session = checkout
            .start("sess-1", "acme", &listing, None, &provider, 200)
            .unwrap();
        assert_eq!(session.state, SessionState::Pending);
        assert_eq!(session.amount.minor_units(), 990);
        assert_eq!(session.provider_ref, "fake-session-sess-1");
        assert!(session.pending_at(300));
    }

    #[test]
    fn a_session_transitions_once() {
        let catalogue = Catalogue::new();
        paid_listing(&catalogue);
        let provider = FakePaymentProvider::new();
        let checkout = Checkout::new();
        let listing = catalogue.get("list-1").unwrap();
        checkout
            .start("sess-1", "acme", &listing, None, &provider, 200)
            .unwrap();
        checkout.mark_paid("sess-1", "order-1", 300).unwrap();
        assert_eq!(
            checkout
                .mark_paid("sess-1", "order-2", 301)
                .unwrap_err()
                .code(),
            codes::SESSION_NOT_PENDING
        );
        assert_eq!(
            checkout.get("sess-1").unwrap().order_id.as_deref(),
            Some("order-1")
        );
    }
}
