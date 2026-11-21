//! Stable error codes for the app store's back end, in the `store_commerce.*` family.
//!
//! Every refusal is a [`StoreError`] carrying one of the codes in [`codes`] and a
//! plain-language message. Messages never echo a secret, a signing key, a signature or a
//! card reference; a message names the record or the field, not its value.

use std::fmt;

/// The stable, machine-readable codes this crate raises. Catalogued in
/// `crates/framework/spot-errors/catalogue/store_commerce.toml`.
pub mod codes {
    /// A caller-supplied shape is invalid (a blank id, a bad range).
    pub const INVALID_INPUT: &str = "store_commerce.invalid_input";
    /// A record the caller named does not exist.
    pub const NOT_FOUND: &str = "store_commerce.not_found";
    /// The publisher is unknown to the store.
    pub const PUBLISHER_UNKNOWN: &str = "store_commerce.publisher_unknown";
    /// The publisher has not finished onboarding.
    pub const PUBLISHER_NOT_VERIFIED: &str = "store_commerce.publisher_not_verified";
    /// The publisher is suspended and may not publish.
    pub const PUBLISHER_SUSPENDED: &str = "store_commerce.publisher_suspended";
    /// A registered signing key is not a valid Ed25519 public key.
    pub const SIGNING_KEY_INVALID: &str = "store_commerce.signing_key_invalid";
    /// A signing key is already registered for the publisher.
    pub const SIGNING_KEY_DUPLICATE: &str = "store_commerce.signing_key_duplicate";
    /// The listing is unknown to the catalogue.
    pub const LISTING_UNKNOWN: &str = "store_commerce.listing_unknown";
    /// The app version has not been approved by a human reviewer.
    pub const LISTING_UNREVIEWED: &str = "store_commerce.listing_unreviewed";
    /// The app version carries no verified signature.
    pub const LISTING_UNSIGNED: &str = "store_commerce.listing_unsigned";
    /// The listing is free; there is nothing to check out.
    pub const LISTING_FREE: &str = "store_commerce.listing_free";
    /// The listing is paid; a free install path cannot grant it.
    pub const LISTING_PAID: &str = "store_commerce.listing_paid";
    /// The app version is unknown to the listing.
    pub const VERSION_UNKNOWN: &str = "store_commerce.version_unknown";
    /// No review report is recorded for the app version.
    pub const REVIEW_NOT_FOUND: &str = "store_commerce.review_not_found";
    /// A price is not a positive whole number of minor units.
    pub const PRICE_INVALID: &str = "store_commerce.price_invalid";
    /// A plan is not a valid edition slug.
    pub const PLAN_INVALID: &str = "store_commerce.plan_invalid";
    /// A checkout session is unknown.
    pub const SESSION_UNKNOWN: &str = "store_commerce.session_unknown";
    /// A checkout session is not pending, so it cannot complete again.
    pub const SESSION_NOT_PENDING: &str = "store_commerce.session_not_pending";
    /// The payment provider reported a failed payment.
    pub const PAYMENT_FAILED: &str = "store_commerce.payment_failed";
    /// A webhook's signature did not verify.
    pub const WEBHOOK_SIGNATURE_INVALID: &str = "store_commerce.webhook_signature_invalid";
    /// A webhook body is not the event shape the store expects.
    pub const WEBHOOK_MALFORMED: &str = "store_commerce.webhook_malformed";
    /// A webhook's timestamp is outside the accepted tolerance.
    pub const WEBHOOK_STALE: &str = "store_commerce.webhook_stale";
    /// The tenant holds no entitlement for the app.
    pub const ENTITLEMENT_UNKNOWN: &str = "store_commerce.entitlement_unknown";
    /// The entitlement has been revoked.
    pub const ENTITLEMENT_REVOKED: &str = "store_commerce.entitlement_revoked";
    /// An app licence is structurally invalid or fails verification.
    pub const LICENCE_INVALID: &str = "store_commerce.licence_invalid";
    /// The app licence names a different tenant.
    pub const LICENCE_WRONG_TENANT: &str = "store_commerce.licence_wrong_tenant";
    /// The app licence does not grant the app asked about.
    pub const LICENCE_WRONG_APP: &str = "store_commerce.licence_wrong_app";
    /// The app licence is named by a revocation list.
    pub const LICENCE_REVOKED: &str = "store_commerce.licence_revoked";
    /// A revocation list is not validly signed or is stale.
    pub const REVOCATION_INVALID: &str = "store_commerce.revocation_invalid";
    /// A renewal does not strictly extend the entitlement.
    pub const RENEWAL_STALE: &str = "store_commerce.renewal_stale";
    /// The payment provider refused an operation.
    pub const PROVIDER_ERROR: &str = "store_commerce.provider_error";
    /// The registry refused an operation.
    pub const REGISTRY_ERROR: &str = "store_commerce.registry_error";
    /// A money operation failed.
    pub const MONEY_ERROR: &str = "store_commerce.money_error";
    /// A ledger record would violate the append-only contract.
    pub const LEDGER_ERROR: &str = "store_commerce.ledger_error";
    /// A signing operation failed.
    pub const SIGNING_ERROR: &str = "store_commerce.signing_error";
}

/// A refusal from the store's back end, with a stable [`code`](StoreError::code).
#[derive(Clone, PartialEq, Eq)]
pub struct StoreError {
    code: &'static str,
    message: String,
}

impl StoreError {
    /// A refusal with a code from [`codes`] and a plain-language message.
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// A refusal with the given code and a message built by `f`.
    pub fn with(code: &'static str, message: impl FnOnce() -> String) -> Self {
        Self::new(code, message())
    }

    /// The stable machine-readable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.code
    }

    /// The human-readable message.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }

    /// A missing record with the named id.
    pub fn missing(code: &'static str, what: &str) -> Self {
        Self::new(code, format!("{what} is not known to the store"))
    }

    /// An invalid caller-supplied value, naming the field but not its value.
    pub fn invalid(field: &str) -> Self {
        Self::new(codes::INVALID_INPUT, format!("{field} is invalid"))
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl fmt::Debug for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Errors are never secret; Debug stays deliberate and short.
        f.write_str(&self.to_string())
    }
}

impl std::error::Error for StoreError {}

impl From<spot_license::LicenceError> for StoreError {
    fn from(error: spot_license::LicenceError) -> Self {
        Self::new(codes::LICENCE_INVALID, error.to_string())
    }
}

impl From<spot_license::RenewalError> for StoreError {
    fn from(error: spot_license::RenewalError) -> Self {
        match error {
            spot_license::RenewalError::Stale { .. } => {
                Self::new(codes::RENEWAL_STALE, error.to_string())
            }
            _ => Self::new(codes::LICENCE_INVALID, error.to_string()),
        }
    }
}

impl From<spot_money::MoneyError> for StoreError {
    fn from(error: spot_money::MoneyError) -> Self {
        Self::new(codes::MONEY_ERROR, error.to_string())
    }
}

impl From<spot_registry::RegistryError> for StoreError {
    fn from(error: spot_registry::RegistryError) -> Self {
        Self::new(codes::REGISTRY_ERROR, error.to_string())
    }
}

impl From<spot_bundle_signing::SigningError> for StoreError {
    fn from(error: spot_bundle_signing::SigningError) -> Self {
        Self::new(codes::SIGNING_ERROR, error.to_string())
    }
}

/// The store's result type.
pub type Result<T> = std::result::Result<T, StoreError>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{StoreError, codes};

    #[test]
    fn a_code_is_its_own_and_stable() {
        let error = StoreError::new(codes::LISTING_UNREVIEWED, "not reviewed yet");
        assert_eq!(error.code(), "store_commerce.listing_unreviewed");
        assert_eq!(error.message(), "not reviewed yet");
        assert_eq!(
            error.to_string(),
            "store_commerce.listing_unreviewed: not reviewed yet"
        );
    }

    #[test]
    fn dependency_errors_map_onto_store_codes() {
        let money = spot_money::MoneyError::BadCurrency("usd".to_owned());
        let mapped: StoreError = money.into();
        assert_eq!(mapped.code(), "store_commerce.money_error");

        let registry = spot_registry::RegistryError::new("registry.not_found", "absent");
        let mapped: StoreError = registry.into();
        assert_eq!(mapped.code(), "store_commerce.registry_error");
    }
}
