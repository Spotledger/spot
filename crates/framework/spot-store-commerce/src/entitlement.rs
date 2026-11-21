//! Entitlements: what a payment grants, how it renews and lapses, and how it is revoked.
//!
//! An [`Entitlement`] is the store's record that `tenant` bought `app` under a plan. Its
//! lifecycle is time-derived, never stored as a mutable flag: [`Entitlement::state_at`]
//! reads the raw validity window, so the same record answers "what may this tenant do" at
//! any instant the caller supplies.
//!
//! Refunds and chargebacks revoke an entitlement and add it to a **signed revocation
//! list**. The list reuses the `spot-bundle-signing` mechanism: a root-signed,
//! generation-strict, canonically-encoded document that a verifier refuses if it is stale
//! or unsigned. `spot-installer` and the runtime consult the list through
//! [`crate::licence::check_licence_with_state`].

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde_json::Value;
use spot_bundle_signing::PublicKey;
use spot_bundle_signing::canonical;
use spot_money::Currency;

use crate::error::{Result, StoreError, codes};
use crate::licence::StoreSigner;

/// The domain prefix of an entitlement revocation list's signed bytes.
pub const REVOCATION_DOMAIN: &str = "SPOT-ENTITLEMENT-REVOCATION-V1\n";

/// The lifecycle state of an entitlement at one instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntitlementState {
    /// In its validity window: paid features are enabled.
    Active,
    /// Past `valid_until` but inside the grace period: paid features are still enabled,
    /// with a warning.
    Grace,
    /// Past the grace: paid features are disabled; the tenant's data stays readable.
    Lapsed,
    /// Revoked by a refund or chargeback: paid features are disabled.
    Revoked,
}

impl EntitlementState {
    /// Whether the paid features are enabled in this state.
    #[must_use]
    pub fn paid_features_enabled(self) -> bool {
        matches!(self, Self::Active | Self::Grace)
    }
}

/// One tenant's grant of one app under one plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entitlement {
    /// The stable entitlement id.
    pub entitlement_id: String,
    /// The tenant that holds it.
    pub tenant: String,
    /// The app it grants.
    pub app: String,
    /// The listing it was bought from.
    pub listing_id: String,
    /// The plan slug.
    pub plan: String,
    /// The currency the plan is priced in.
    pub currency: Currency,
    /// The `spot-license` grant id its app licences travel under.
    pub licence_id: String,
    /// The grant serial of the latest issued app licence.
    pub serial: u64,
    /// The last instant the entitlement grants full use; `None` for a perpetual grant.
    pub valid_until: Option<i64>,
    /// The grace period after `valid_until`, in seconds.
    pub grace: i64,
    /// The `spot-license` kind an app licence for this plan mints.
    pub licence_kind: spot_license::Kind,
    /// The subscription billing period in seconds; 0 for a one-time grant.
    pub interval_seconds: i64,
    /// Whether a refund or chargeback revoked it.
    pub revoked: bool,
    /// When the entitlement was last changed (UTC seconds).
    pub updated_at: i64,
}

impl Entitlement {
    /// The state at `now`.
    #[must_use]
    pub fn state_at(&self, now: i64) -> EntitlementState {
        if self.revoked {
            return EntitlementState::Revoked;
        }
        let Some(valid_until) = self.valid_until else {
            return EntitlementState::Active;
        };
        if now <= valid_until {
            return EntitlementState::Active;
        }
        let grace_end = valid_until.saturating_add(self.grace);
        if now <= grace_end {
            EntitlementState::Grace
        } else {
            EntitlementState::Lapsed
        }
    }

    /// Whether the entitlement is in force at `now`.
    #[must_use]
    pub fn in_force_at(&self, now: i64) -> bool {
        self.state_at(now).paid_features_enabled()
    }

    /// The app-licence view of this entitlement at `serial`.
    ///
    /// # Errors
    /// [`codes::LICENCE_INVALID`] when the entitlement cannot form a valid app licence.
    pub fn app_licence(&self, serial: u64, issued_at: i64) -> Result<crate::licence::AppLicence> {
        crate::licence::AppLicence::new(
            &self.licence_id,
            &self.app,
            &self.tenant,
            &self.plan,
            self.licence_kind,
            self.valid_until,
            self.grace,
            serial,
            issued_at,
        )
    }
}

/// The store's entitlements.
#[derive(Debug, Default)]
pub struct EntitlementStore {
    entitlements: Mutex<BTreeMap<(String, String), Entitlement>>,
}

impl EntitlementStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<(String, String), Entitlement>> {
        self.entitlements
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Grants (or replaces) an entitlement.
    ///
    /// # Errors
    /// [`codes::INVALID_INPUT`] for a bad id or tenant.
    pub fn grant(&self, entitlement: Entitlement) -> Result<Entitlement> {
        crate::publisher::check_identifier(&entitlement.entitlement_id, "entitlement id")?;
        crate::publisher::check_identifier(&entitlement.tenant, "tenant")?;
        self.lock().insert(
            (entitlement.tenant.clone(), entitlement.app.clone()),
            entitlement.clone(),
        );
        Ok(entitlement)
    }

    /// The entitlement for `(tenant, app)`.
    ///
    /// # Errors
    /// [`codes::ENTITLEMENT_UNKNOWN`] when the tenant holds none.
    pub fn get(&self, tenant: &str, app: &str) -> Result<Entitlement> {
        self.lock()
            .get(&(tenant.to_owned(), app.to_owned()))
            .cloned()
            .ok_or_else(|| StoreError::missing(codes::ENTITLEMENT_UNKNOWN, "the entitlement"))
    }

    /// The entitlement a licence id belongs to, if any.
    #[must_use]
    pub fn by_licence(&self, licence_id: &str) -> Option<Entitlement> {
        self.lock()
            .values()
            .find(|entitlement| entitlement.licence_id == licence_id)
            .cloned()
    }

    /// Extends an entitlement on a subscription renewal.
    ///
    /// # Errors
    /// [`codes::ENTITLEMENT_UNKNOWN`], [`codes::RENEWAL_STALE`] when the new window or
    /// serial does not strictly extend the grant, or [`codes::ENTITLEMENT_REVOKED`] when it
    /// was revoked.
    pub fn renew(
        &self,
        tenant: &str,
        app: &str,
        valid_until: i64,
        serial: u64,
        now: i64,
    ) -> Result<Entitlement> {
        let mut entitlements = self.lock();
        let entitlement = entitlements
            .get_mut(&(tenant.to_owned(), app.to_owned()))
            .ok_or_else(|| StoreError::missing(codes::ENTITLEMENT_UNKNOWN, "the entitlement"))?;
        if entitlement.revoked {
            return Err(StoreError::new(
                codes::ENTITLEMENT_REVOKED,
                "the entitlement has been revoked",
            ));
        }
        let extended = entitlement
            .valid_until
            .is_none_or(|current| valid_until > current);
        if !extended || serial <= entitlement.serial {
            return Err(StoreError::new(
                codes::RENEWAL_STALE,
                "a renewal must strictly extend the validity window and the serial",
            ));
        }
        entitlement.valid_until = Some(valid_until);
        entitlement.serial = serial;
        entitlement.updated_at = now;
        Ok(entitlement.clone())
    }

    /// Revokes an entitlement, shortening its validity to `now`.
    ///
    /// # Errors
    /// [`codes::ENTITLEMENT_UNKNOWN`].
    pub fn revoke(&self, tenant: &str, app: &str, now: i64) -> Result<Entitlement> {
        let mut entitlements = self.lock();
        let entitlement = entitlements
            .get_mut(&(tenant.to_owned(), app.to_owned()))
            .ok_or_else(|| StoreError::missing(codes::ENTITLEMENT_UNKNOWN, "the entitlement"))?;
        entitlement.revoked = true;
        if entitlement.valid_until.is_some() {
            entitlement.valid_until = Some(now);
        }
        entitlement.updated_at = now;
        Ok(entitlement.clone())
    }

    /// Every entitlement of a tenant, in app order.
    #[must_use]
    pub fn for_tenant(&self, tenant: &str) -> Vec<Entitlement> {
        self.lock()
            .values()
            .filter(|entitlement| entitlement.tenant == tenant)
            .cloned()
            .collect()
    }

    /// Every entitlement, in `(tenant, app)` order.
    #[must_use]
    pub fn all(&self) -> Vec<Entitlement> {
        self.lock().values().cloned().collect()
    }
}

/// One revoked entitlement in a [`RevocationList`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntitlementRevocation {
    /// The revoked `spot-license` grant id.
    pub licence_id: String,
    /// The tenant the grant belonged to.
    pub tenant: String,
    /// The app the grant covered.
    pub app: String,
    /// The instant the revocation takes effect (UTC seconds).
    pub effective_from: i64,
    /// Why, for the operator.
    pub reason: String,
}

/// The unsigned body of an entitlement revocation list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevocationBody {
    /// Strictly increasing per issued list.
    pub generation: u64,
    /// When the list was issued (UTC seconds).
    pub issued_at: i64,
    /// The root key id that signs it.
    pub issued_by: String,
    /// The revoked entitlements, sorted by licence id.
    pub entries: Vec<EntitlementRevocation>,
}

impl RevocationBody {
    /// The domain-separated canonical bytes the root signs.
    #[must_use]
    pub fn signed_bytes(&self) -> Vec<u8> {
        let entries: Vec<Value> = self
            .entries
            .iter()
            .map(|entry| {
                let mut fields = BTreeMap::new();
                fields.insert("app", entry.app.as_str().into());
                fields.insert("effective_from", entry.effective_from.into());
                fields.insert("licence_id", entry.licence_id.as_str().into());
                fields.insert("reason", entry.reason.as_str().into());
                fields.insert("tenant", entry.tenant.as_str().into());
                canonical::object(fields)
            })
            .collect();
        let mut fields = BTreeMap::new();
        fields.insert("entries", entries.into());
        fields.insert("generation", self.generation.into());
        fields.insert("issued_at", self.issued_at.into());
        fields.insert("issued_by", self.issued_by.as_str().into());
        canonical::encode(REVOCATION_DOMAIN, &fields)
    }
}

/// An entitlement revocation list with the root's signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevocationList {
    body: RevocationBody,
    signature: [u8; 64],
}

impl RevocationList {
    /// Assembles a list from its parts without checking the signature; [`RevocationList::verify`]
    /// checks it.
    #[must_use]
    pub fn from_parts(body: RevocationBody, signature: [u8; 64]) -> Self {
        Self { body, signature }
    }

    /// The list's body.
    #[must_use]
    pub fn body(&self) -> &RevocationBody {
        &self.body
    }

    /// The root's signature over [`RevocationBody::signed_bytes`].
    #[must_use]
    pub fn signature(&self) -> &[u8; 64] {
        &self.signature
    }

    /// Verifies the list against the store root, refusing one that is stale or unsigned.
    ///
    /// # Errors
    /// [`codes::REVOCATION_INVALID`] when the list names another root, is not strictly
    /// newer than `held_generation`, or its signature does not verify.
    pub fn verify(&self, root: &PublicKey, held_generation: u64) -> Result<()> {
        if root.key_id().as_str() != self.body.issued_by {
            return Err(StoreError::new(
                codes::REVOCATION_INVALID,
                "the revocation list is signed by another root",
            ));
        }
        if self.body.generation <= held_generation {
            return Err(StoreError::new(
                codes::REVOCATION_INVALID,
                "the revocation list is not newer than the one held",
            ));
        }
        root.verify(&self.body.signed_bytes(), &self.signature)
            .map_err(|_| {
                StoreError::new(
                    codes::REVOCATION_INVALID,
                    "the revocation list signature does not verify",
                )
            })
    }

    /// Whether the list revokes `licence_id` at `now`.
    #[must_use]
    pub fn revokes(&self, licence_id: &str, now: i64) -> bool {
        self.body
            .entries
            .iter()
            .any(|entry| entry.licence_id == licence_id && entry.effective_from <= now)
    }
}

/// Issues a revocation list: the store root signs the canonical body. Entries are sorted
/// here, so equal content always signs the same bytes.
#[must_use]
pub fn issue_revocation_list(mut body: RevocationBody, root: &dyn StoreSigner) -> RevocationList {
    body.entries.sort_by(|a, b| a.licence_id.cmp(&b.licence_id));
    let signature = root.sign(&body.signed_bytes());
    RevocationList { body, signature }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        Entitlement, EntitlementRevocation, EntitlementState, EntitlementStore, RevocationBody,
        issue_revocation_list,
    };
    use crate::error::codes;
    use crate::licence::StoreSigner;
    use crate::test_support::{TestSigner, usd};

    fn entitlement(valid_until: Option<i64>) -> Entitlement {
        Entitlement {
            entitlement_id: "ent-1".to_owned(),
            tenant: "acme".to_owned(),
            app: "my_app".to_owned(),
            listing_id: "list-1".to_owned(),
            plan: "pro_monthly".to_owned(),
            currency: usd(),
            licence_id: "lic-1".to_owned(),
            serial: 1,
            valid_until,
            grace: 7 * 86_400,
            licence_kind: spot_license::Kind::Subscription,
            interval_seconds: 30 * 86_400,
            revoked: false,
            updated_at: 100,
        }
    }

    #[test]
    fn the_state_is_time_derived() {
        let entitlement = entitlement(Some(1_000));
        assert_eq!(entitlement.state_at(1_000), EntitlementState::Active);
        assert_eq!(entitlement.state_at(1_001), EntitlementState::Grace);
        assert_eq!(
            entitlement.state_at(1_000 + 7 * 86_400),
            EntitlementState::Grace
        );
        assert_eq!(
            entitlement.state_at(1_000 + 7 * 86_400 + 1),
            EntitlementState::Lapsed
        );
        assert!(entitlement.in_force_at(1_001));
        assert!(!entitlement.in_force_at(1_000 + 8 * 86_400));
    }

    #[test]
    fn a_renewal_must_strictly_extend() {
        let store = EntitlementStore::new();
        store.grant(entitlement(Some(1_000))).unwrap();
        assert_eq!(
            store
                .renew("acme", "my_app", 900, 2, 500)
                .unwrap_err()
                .code(),
            codes::RENEWAL_STALE
        );
        assert_eq!(
            store
                .renew("acme", "my_app", 2_000, 1, 500)
                .unwrap_err()
                .code(),
            codes::RENEWAL_STALE
        );
        let renewed = store.renew("acme", "my_app", 2_000, 2, 500).unwrap();
        assert_eq!(renewed.valid_until, Some(2_000));
        assert_eq!(renewed.serial, 2);
    }

    #[test]
    fn revoking_shortens_and_refuses_renewal() {
        let store = EntitlementStore::new();
        store.grant(entitlement(Some(1_000))).unwrap();
        let revoked = store.revoke("acme", "my_app", 400).unwrap();
        assert_eq!(revoked.state_at(400), EntitlementState::Revoked);
        assert_eq!(revoked.valid_until, Some(400));
        assert_eq!(
            store
                .renew("acme", "my_app", 5_000, 2, 500)
                .unwrap_err()
                .code(),
            codes::ENTITLEMENT_REVOKED
        );
    }

    #[test]
    fn a_signed_revocation_list_verifies_and_revokes() {
        let signer = TestSigner::new(7);
        let root = signer.public();
        let body = RevocationBody {
            generation: 2,
            issued_at: 500,
            issued_by: signer.key_id().to_owned(),
            entries: vec![EntitlementRevocation {
                licence_id: "lic-1".to_owned(),
                tenant: "acme".to_owned(),
                app: "my_app".to_owned(),
                effective_from: 500,
                reason: "refund".to_owned(),
            }],
        };
        let list = issue_revocation_list(body, &signer);
        list.verify(&root, 1).unwrap();
        assert!(list.revokes("lic-1", 500));
        assert!(!list.revokes("lic-1", 499));
        assert!(!list.revokes("lic-2", 500));

        // A stale generation is refused.
        assert_eq!(
            list.verify(&root, 2).unwrap_err().code(),
            codes::REVOCATION_INVALID
        );
        // A tampered body is refused.
        let mut tampered = list.clone();
        tampered.body.generation = 3;
        assert_eq!(
            tampered.verify(&root, 1).unwrap_err().code(),
            codes::REVOCATION_INVALID
        );
        // Another root is refused.
        let other = TestSigner::new(9);
        assert_eq!(
            list.verify(&other.public(), 1).unwrap_err().code(),
            codes::REVOCATION_INVALID
        );
    }
}
