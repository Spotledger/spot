//! App licences: the signed grant a paid purchase mints, and the offline check for it.
//!
//! An app licence **is a `spot-license` licence**: the same canonical, versioned JSON
//! payload, the same `SPOT-LICENCE-1` signed envelope and the same offline verification
//! against a trusted-key list. The store adds only a vocabulary on top:
//!
//! | App-licence field | `spot-license` payload |
//! | --- | --- |
//! | `app` | `entitlements.apps` (exactly one) |
//! | `tenant` | `tenant` |
//! | `plan` | `edition` (and the `plan` entitlement) |
//! | `valid_until` | `expires_at` (`None` for a one-time grant) |
//! | `grace` | `grace` |
//!
//! The payload also carries the [`APP_LICENCE_FEATURE`] flag in `entitlements.features`, so
//! a verifier can tell an app licence from a platform licence without guessing. This is the
//! crate's "new payload kind": `spot-license`'s `Kind` is a closed enum, so the kind is a
//! signed entitlement rather than a new enum variant.
//!
//! [`check_licence`] is what `spot-installer` calls at install and the runtime calls at
//! start: it verifies the signature **offline** with only the licence and the store's
//! public key, then reports the lifecycle status (from `spot-license`'s state machine) and
//! whether the paid features are enabled. A lapsed subscription disables paid features but
//! never hides data: [`LicenceCheck::data_readable`] stays true in every status.

use std::collections::BTreeMap;

use spot_license::model::{Edition, ExtraEntitlement, RawLicence};
use spot_license::{
    DEFAULT_GRACE, Entitlements, Kind, Licence, MAX_GRACE, SignedLicence, Status, TrustedKey,
    TrustedKeys,
};

use crate::error::{Result, StoreError, codes};
use crate::publisher::check_identifier;

/// The `entitlements.features` flag that marks a payload as an app licence.
pub const APP_LICENCE_FEATURE: &str = "app-licence";

/// The store's signing key, behind a port so the deployment can hold it in hardware.
///
/// One key serves both the licence envelope and the entitlement revocation list: the id is
/// the `spot-bundle-signing` [`KeyId`](spot_bundle_signing::KeyId) text (hex), which is
/// also a valid `spot-license` `issuer_key_id`. The secret never leaves the implementation
/// and is never printed or logged.
pub trait StoreSigner: Send + Sync {
    /// The key id, hex, as `spot-bundle-signing` derives it from the public key.
    fn key_id(&self) -> &str;

    /// The raw 32-byte Ed25519 public key.
    fn public_key_bytes(&self) -> [u8; 32];

    /// The raw 64-byte Ed25519 signature over `message`.
    fn sign(&self, message: &[u8]) -> [u8; 64];
}

/// The trusted-key entry a verifier builds from a signer's public key.
///
/// # Errors
/// [`codes::SIGNING_ERROR`] when the public key bytes are not a valid curve point.
pub fn trusted_key(signer: &dyn StoreSigner) -> Result<TrustedKey> {
    TrustedKey::from_bytes(signer.key_id(), &signer.public_key_bytes(), 0, None)
        .map_err(StoreError::from)
}

/// The trust anchor over one or more store signers (a rotation keeps both).
///
/// # Errors
/// [`codes::SIGNING_ERROR`] when a public key is not a valid curve point.
pub fn trusted_keys(signers: &[&dyn StoreSigner]) -> Result<TrustedKeys> {
    let mut keys = Vec::with_capacity(signers.len());
    for signer in signers {
        keys.push(trusted_key(*signer)?);
    }
    Ok(TrustedKeys::new(keys))
}

/// The store's view of an app licence: the fields the portal and the runtime use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppLicence {
    /// The stable grant id the licence travels under, forever.
    pub licence_id: String,
    /// The app the licence grants.
    pub app: String,
    /// The tenant the licence belongs to.
    pub tenant: String,
    /// The plan slug the issuer minted (the `spot-license` edition).
    pub plan: String,
    /// The grant kind: perpetual for a one-time purchase, subscription for a plan.
    pub kind: Kind,
    /// The last instant the licence grants full use; `None` for a perpetual grant.
    pub valid_until: Option<i64>,
    /// The grace period after `valid_until`, in seconds.
    pub grace: i64,
    /// The grant's serial within its licence id; renewals strictly increase it.
    pub serial: u64,
    /// When the issuer signed this payload (UTC seconds).
    pub issued_at: i64,
}

impl AppLicence {
    /// Builds an app licence, validating every field the payload needs.
    ///
    /// # Errors
    /// [`codes::INVALID_INPUT`] for a bad id, app or tenant, [`codes::PLAN_INVALID`] for a
    /// plan that is not an edition slug, [`codes::PRICE_INVALID`] for an inconsistent
    /// validity window, or [`codes::LICENCE_INVALID`] for a grace period over the cap.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        licence_id: &str,
        app: &str,
        tenant: &str,
        plan: &str,
        kind: Kind,
        valid_until: Option<i64>,
        grace: i64,
        serial: u64,
        issued_at: i64,
    ) -> Result<Self> {
        check_identifier(licence_id, "licence id")?;
        check_identifier(app, "app id")?;
        check_identifier(tenant, "tenant")?;
        Edition::new(plan)
            .map_err(|_| StoreError::new(codes::PLAN_INVALID, "the plan is invalid"))?;
        if serial == 0 {
            return Err(StoreError::invalid("licence serial"));
        }
        if grace < 0 || grace > i64::try_from(MAX_GRACE.as_secs()).unwrap_or(i64::MAX) {
            return Err(StoreError::new(
                codes::LICENCE_INVALID,
                "the grace period is outside the accepted range",
            ));
        }
        match kind {
            Kind::Perpetual => {
                if valid_until.is_some() {
                    return Err(StoreError::new(
                        codes::PRICE_INVALID,
                        "a perpetual licence has no expiry",
                    ));
                }
            }
            _ => {
                let Some(valid_until) = valid_until else {
                    return Err(StoreError::new(
                        codes::PRICE_INVALID,
                        "a subscription licence needs an expiry",
                    ));
                };
                if valid_until < issued_at {
                    return Err(StoreError::new(
                        codes::PRICE_INVALID,
                        "the validity window ends before it starts",
                    ));
                }
            }
        }
        if kind != Kind::Subscription && grace != 0 {
            return Err(StoreError::new(
                codes::LICENCE_INVALID,
                "only a subscription carries a grace period",
            ));
        }
        Ok(Self {
            licence_id: licence_id.to_owned(),
            app: app.to_owned(),
            tenant: tenant.to_owned(),
            plan: plan.to_owned(),
            kind,
            valid_until,
            grace,
            serial,
            issued_at,
        })
    }

    /// Reads the app-licence view out of a verified `spot-license` payload.
    ///
    /// # Errors
    /// [`codes::LICENCE_INVALID`] when the payload is not an app licence: it names an
    /// installation instead of a tenant, lacks the [`APP_LICENCE_FEATURE`] flag, or does
    /// not grant exactly one app.
    pub fn from_licence(licence: &Licence) -> Result<Self> {
        let tenant = licence.tenant.clone().ok_or_else(|| {
            StoreError::new(
                codes::LICENCE_INVALID,
                "the payload is not a tenant app licence",
            )
        })?;
        if !licence
            .entitlements
            .features()
            .iter()
            .any(|feature| feature == APP_LICENCE_FEATURE)
        {
            return Err(StoreError::new(
                codes::LICENCE_INVALID,
                "the payload is not an app licence",
            ));
        }
        let apps = licence.entitlements.apps();
        let [app] = apps else {
            return Err(StoreError::new(
                codes::LICENCE_INVALID,
                "an app licence grants exactly one app",
            ));
        };
        Ok(Self {
            licence_id: licence.licence_id.clone(),
            app: app.clone(),
            tenant,
            plan: licence.edition.as_str().to_owned(),
            kind: licence.kind,
            valid_until: licence.expires_at,
            grace: i64::try_from(licence.grace.as_secs()).unwrap_or(i64::MAX),
            serial: licence.serial,
            issued_at: licence.issued_at,
        })
    }

    /// Renders the app licence as the `spot-license` payload signed by `key_id`.
    ///
    /// # Errors
    /// [`codes::LICENCE_INVALID`] when the fields do not form a valid licence.
    pub fn to_licence(&self, key_id: &str) -> Result<Licence> {
        let edition = Edition::new(&self.plan)
            .map_err(|_| StoreError::new(codes::PLAN_INVALID, "the plan is invalid"))?;
        let mut extra = BTreeMap::new();
        extra.insert("plan".to_owned(), ExtraEntitlement::Name(self.plan.clone()));
        let entitlements = Entitlements {
            users: None,
            apps: vec![self.app.clone()],
            features: vec![APP_LICENCE_FEATURE.to_owned()],
            extra,
        };
        Licence::new(RawLicence {
            v: spot_license::model::PAYLOAD_VERSION,
            licence_id: self.licence_id.clone(),
            tenant: Some(self.tenant.clone()),
            installation_id: None,
            edition,
            ent: entitlements,
            kind: self.kind,
            issued_at: self.issued_at,
            not_before: self.issued_at,
            expires_at: self.valid_until,
            renew_after: None,
            grace: u64::try_from(self.grace).unwrap_or_default(),
            serial: self.serial,
            key_id: key_id.to_owned(),
        })
        .map_err(StoreError::from)
    }
}

/// A signed app licence: the `spot-license` envelope plus the decoded view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignedAppLicence {
    signed: SignedLicence,
    licence: AppLicence,
}

impl SignedAppLicence {
    /// The `SPOT-LICENCE-1.<payload>.<signature>` text form a customer receives.
    ///
    /// # Errors
    /// [`codes::LICENCE_INVALID`] when the envelope cannot be encoded.
    pub fn to_text(&self) -> Result<String> {
        self.signed.to_text().map_err(StoreError::from)
    }

    /// Parses the text form without verifying it; [`check_licence`] is what verifies.
    ///
    /// # Errors
    /// [`codes::LICENCE_INVALID`] for malformed text or a payload that is not an app
    /// licence.
    pub fn from_text(text: &str) -> Result<Self> {
        let signed = SignedLicence::from_text(text).map_err(StoreError::from)?;
        let licence = AppLicence::from_licence(&signed.claimed_licence()?)?;
        Ok(Self { signed, licence })
    }

    /// The decoded app-licence view (untrusted until [`check_licence`] verifies it).
    #[must_use]
    pub fn licence(&self) -> &AppLicence {
        &self.licence
    }

    /// The underlying `spot-license` envelope.
    #[must_use]
    pub fn envelope(&self) -> &SignedLicence {
        &self.signed
    }
}

/// The issuing side: mints a signed app licence with the store's key.
pub struct LicenceIssuer<'a> {
    signer: &'a dyn StoreSigner,
}

impl<'a> LicenceIssuer<'a> {
    /// An issuer over the store's signing key.
    #[must_use]
    pub fn new(signer: &'a dyn StoreSigner) -> Self {
        Self { signer }
    }

    /// Signs an app licence: the canonical payload bytes and the Ed25519 signature over
    /// them, in the same `SPOT-LICENCE-1` envelope `spot-license` verifies.
    ///
    /// # Errors
    /// [`codes::LICENCE_INVALID`] when the licence cannot be rendered or the envelope
    /// cannot be built.
    pub fn issue(&self, licence: &AppLicence) -> Result<SignedAppLicence> {
        let payload = licence.to_licence(self.signer.key_id())?;
        let bytes = payload.to_canonical_bytes().map_err(StoreError::from)?;
        let signature = self.signer.sign(&bytes);
        let signed = SignedLicence::from_parts(bytes, signature.to_vec(), self.signer.key_id())
            .map_err(StoreError::from)?;
        Ok(SignedAppLicence {
            signed,
            licence: licence.clone(),
        })
    }
}

/// What [`check_licence`] reports about a verified app licence at `now`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LicenceCheck {
    /// The app the licence grants.
    pub app: String,
    /// The tenant the licence belongs to.
    pub tenant: String,
    /// The plan slug.
    pub plan: String,
    /// The lifecycle status `spot-license`'s state machine computed.
    pub status: Status,
    /// The last instant the licence grants full use.
    pub valid_until: Option<i64>,
    /// The grace period after `valid_until`, in seconds.
    pub grace: i64,
    /// Whether the paid features are enabled (full function, including grace).
    pub paid_features_enabled: bool,
    /// Whether data stays readable. **Always true**: a lapse disables paid features, never
    /// access to the tenant's own data.
    pub data_readable: bool,
}

impl LicenceCheck {
    /// Whether the licence grants full function (active, renewal-due or in grace).
    #[must_use]
    pub fn grants_use(&self) -> bool {
        self.status.grants_use()
    }
}

/// Verifies an app licence **offline** and reports whether it grants `app` to `tenant`.
///
/// The only inputs are the licence, the store's trusted public keys and `now`: no network,
/// no clock read. The checks are: the envelope verifies against a trusted key; the payload
/// is an app licence; its tenant and app match; and (when supplied) no revocation list
/// names it.
///
/// # Errors
/// [`codes::LICENCE_INVALID`] when the envelope or payload fails verification,
/// [`codes::LICENCE_WRONG_TENANT`] or [`codes::LICENCE_WRONG_APP`] when it is not this
/// grant, or [`codes::LICENCE_REVOKED`] when a revocation list names it.
pub fn check_licence(
    signed: &SignedAppLicence,
    trusted: &TrustedKeys,
    tenant: &str,
    app: &str,
    now: i64,
) -> Result<LicenceCheck> {
    check_licence_with_state(signed, trusted, tenant, app, now, None, None, None)
}

/// Verifies the `SPOT-LICENCE-1` text form of an app licence offline.
///
/// # Errors
/// As [`check_licence`], plus [`codes::LICENCE_INVALID`] for malformed text.
pub fn check_licence_text(
    text: &str,
    trusted: &TrustedKeys,
    tenant: &str,
    app: &str,
    now: i64,
) -> Result<LicenceCheck> {
    let signed = SignedAppLicence::from_text(text)?;
    check_licence(&signed, trusted, tenant, app, now)
}

/// [`check_licence`] with the caller's last-seen observation and, optionally, a signed
/// revocation list and the store root's public key to verify it.
///
/// # Errors
/// As [`check_licence`], plus [`codes::REVOCATION_INVALID`] when the list does not verify.
#[allow(clippy::too_many_arguments)]
pub fn check_licence_with_state(
    signed: &SignedAppLicence,
    trusted: &TrustedKeys,
    tenant: &str,
    app: &str,
    now: i64,
    last_seen: Option<i64>,
    revocations: Option<&crate::entitlement::RevocationList>,
    revocation_root: Option<&spot_bundle_signing::PublicKey>,
) -> Result<LicenceCheck> {
    let licence = spot_license::verify(&signed.signed, trusted).map_err(StoreError::from)?;
    let view = AppLicence::from_licence(&licence)?;
    if view.tenant != tenant {
        return Err(StoreError::new(
            codes::LICENCE_WRONG_TENANT,
            "the app licence belongs to another tenant",
        ));
    }
    if view.app != app {
        return Err(StoreError::new(
            codes::LICENCE_WRONG_APP,
            "the app licence grants a different app",
        ));
    }
    if revocations.is_some() && revocation_root.is_none() {
        // A revocation list that cannot be verified must not be silently skipped.
        return Err(StoreError::new(
            codes::LICENCE_INVALID,
            "a revocation list was given without the root key to verify it",
        ));
    }
    if let (Some(list), Some(root)) = (revocations, revocation_root) {
        list.verify(root, 0)?;
        if list.revokes(&view.licence_id, now) {
            return Err(StoreError::new(
                codes::LICENCE_REVOKED,
                "the app licence has been revoked",
            ));
        }
    }
    let status = spot_license::evaluate(&licence, now, last_seen);
    Ok(LicenceCheck {
        app: view.app,
        tenant: view.tenant,
        plan: view.plan,
        status,
        valid_until: view.valid_until,
        grace: view.grace,
        paid_features_enabled: status.grants_use(),
        data_readable: true,
    })
}

/// The default grace period an app subscription gets, in seconds.
#[must_use]
pub fn default_grace() -> i64 {
    i64::try_from(DEFAULT_GRACE.as_secs()).unwrap_or(7 * 86_400)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        APP_LICENCE_FEATURE, AppLicence, LicenceIssuer, SignedAppLicence, StoreSigner,
        check_licence, check_licence_text, trusted_keys,
    };
    use crate::error::codes;
    use crate::test_support::TestSigner;
    use spot_license::{Kind, Status};
    const TENANT: &str = "acme";
    const APP: &str = "my_app";
    const ISSUED: i64 = 1_770_000_000;

    fn subscription(valid_until: i64, serial: u64) -> AppLicence {
        AppLicence::new(
            "lic-1",
            APP,
            TENANT,
            "pro_monthly",
            Kind::Subscription,
            Some(valid_until),
            7 * 86_400,
            serial,
            ISSUED,
        )
        .unwrap()
    }

    #[test]
    fn a_signed_app_licence_verifies_offline_and_names_its_fields() {
        let signer = TestSigner::new(7);
        let trusted = trusted_keys(&[&signer as &dyn StoreSigner]).unwrap();
        let issuer = LicenceIssuer::new(&signer);
        let signed = issuer.issue(&subscription(ISSUED + 86_400, 1)).unwrap();

        let check = check_licence(&signed, &trusted, TENANT, APP, ISSUED + 10).unwrap();
        assert_eq!(check.app, APP);
        assert_eq!(check.tenant, TENANT);
        assert_eq!(check.plan, "pro_monthly");
        assert_eq!(check.status, Status::Active);
        assert!(check.paid_features_enabled);
        assert!(check.data_readable);

        // The text form round trips and checks the same way.
        let text = signed.to_text().unwrap();
        assert!(text.starts_with("SPOT-LICENCE-1."));
        let check = check_licence_text(&text, &trusted, TENANT, APP, ISSUED + 10).unwrap();
        assert_eq!(check.status, Status::Active);

        // The payload carries the app-licence marker.
        let payload = signed.envelope().claimed_licence().unwrap();
        assert!(
            payload
                .entitlements
                .features()
                .iter()
                .any(|feature| feature == APP_LICENCE_FEATURE)
        );
        assert_eq!(payload.entitlements.apps().to_vec(), vec![APP.to_owned()]);
    }

    #[test]
    fn a_tampered_licence_is_refused() {
        let signer = TestSigner::new(7);
        let trusted = trusted_keys(&[&signer as &dyn StoreSigner]).unwrap();
        let issuer = LicenceIssuer::new(&signer);
        let signed = issuer.issue(&subscription(ISSUED + 86_400, 1)).unwrap();

        // Flip one byte of the signature: the payload is untouched, but the signature no
        // longer verifies, so verification refuses it.
        let mut signature = signed.envelope().signature_bytes().to_vec();
        signature[0] ^= 0x01;
        let envelope = spot_license::SignedLicence::from_parts(
            signed.envelope().payload_bytes().to_vec(),
            signature,
            signed.envelope().key_id(),
        )
        .unwrap();
        let tampered = SignedAppLicence {
            signed: envelope,
            licence: signed.licence().clone(),
        };
        assert_eq!(
            check_licence_text(
                &tampered.to_text().unwrap(),
                &trusted,
                TENANT,
                APP,
                ISSUED + 10
            )
            .unwrap_err()
            .code(),
            codes::LICENCE_INVALID
        );

        // Another key's signature is also refused.
        let other = TestSigner::new(9);
        let other_trusted = trusted_keys(&[&other as &dyn StoreSigner]).unwrap();
        assert_eq!(
            check_licence(&signed, &other_trusted, TENANT, APP, ISSUED + 10)
                .unwrap_err()
                .code(),
            codes::LICENCE_INVALID
        );
    }

    #[test]
    fn the_wrong_tenant_or_app_is_refused() {
        let signer = TestSigner::new(7);
        let trusted = trusted_keys(&[&signer as &dyn StoreSigner]).unwrap();
        let signed = LicenceIssuer::new(&signer)
            .issue(&subscription(ISSUED + 86_400, 1))
            .unwrap();
        assert_eq!(
            check_licence(&signed, &trusted, "globex", APP, ISSUED + 10)
                .unwrap_err()
                .code(),
            codes::LICENCE_WRONG_TENANT
        );
        assert_eq!(
            check_licence(&signed, &trusted, TENANT, "other_app", ISSUED + 10)
                .unwrap_err()
                .code(),
            codes::LICENCE_WRONG_APP
        );
    }

    #[test]
    fn a_lapse_enters_grace_then_disables_paid_features_but_not_data() {
        let signer = TestSigner::new(7);
        let trusted = trusted_keys(&[&signer as &dyn StoreSigner]).unwrap();
        let expires = ISSUED + 86_400;
        let signed = LicenceIssuer::new(&signer)
            .issue(&subscription(expires, 1))
            .unwrap();

        let grace = check_licence(&signed, &trusted, TENANT, APP, expires + 1).unwrap();
        assert!(matches!(grace.status, Status::Grace { .. }));
        assert!(grace.paid_features_enabled);

        let lapsed = check_licence(&signed, &trusted, TENANT, APP, expires + 8 * 86_400).unwrap();
        assert_eq!(lapsed.status, Status::Restricted);
        assert!(!lapsed.paid_features_enabled);
        assert!(lapsed.data_readable);
    }

    #[test]
    fn a_perpetual_one_time_licence_never_expires() {
        let signer = TestSigner::new(7);
        let trusted = trusted_keys(&[&signer as &dyn StoreSigner]).unwrap();
        let licence = AppLicence::new(
            "lic-2",
            APP,
            TENANT,
            "pro_once",
            Kind::Perpetual,
            None,
            0,
            1,
            ISSUED,
        )
        .unwrap();
        let signed = LicenceIssuer::new(&signer).issue(&licence).unwrap();
        let check = check_licence(&signed, &trusted, TENANT, APP, ISSUED + 100_000_000).unwrap();
        assert_eq!(check.status, Status::Active);
        assert!(check.paid_features_enabled);
    }

    #[test]
    fn a_plan_that_is_not_an_edition_is_refused() {
        let bad = AppLicence::new(
            "lic-3",
            APP,
            TENANT,
            "not a plan",
            Kind::Perpetual,
            None,
            0,
            1,
            ISSUED,
        );
        assert_eq!(bad.unwrap_err().code(), codes::PLAN_INVALID);
    }
}
