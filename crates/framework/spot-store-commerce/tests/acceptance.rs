//! WP-STR-08.T1 acceptance: a paid app purchase, its licence check and its renewal work
//! end to end, plus the refusals the brief names.
//!
//! Everything runs offline against the fake payment provider and the in-memory components;
//! the app licence is a real Ed25519 signature over a real `spot-license` payload, verified
//! with the store's public key alone.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use ed25519_dalek::Signer as _;
use serde_json::json;
use spot_bundle_signing::{
    BundleSignature, CertificateChain, KeyId, PublisherCertificate, SignedCertificate,
    generate_keypair,
};
use spot_license::Kind;
use spot_money::{Currency, CurrencyTable};
use spot_registry::{AppName, Digest, Release, SignatureRef, Version};
use spot_store_commerce::{
    FakePaymentProvider, FakeReviewService, Interval, PaymentEvent, PaymentEventKind, Price,
    PricePlan, ReviewStatus, ReviewVerdict, SignedAppLicence, Store, StoreSigner, Submission,
    WebhookVerifier, check_licence, sign_payload, trusted_keys,
};

const SECRET: &[u8] = b"a-provider-webhook-secret-value";
const TENANT: &str = "acme";
const APP: &str = "my_app";
const T0: i64 = 1_770_000_000;
const DAY: i64 = 86_400;

/// A throwaway Ed25519 signer, the store's key in these tests.
struct TestSigner {
    signing: ed25519_dalek::SigningKey,
    key_id: String,
}

impl TestSigner {
    fn new(seed_byte: u8) -> Self {
        let mut seed = [seed_byte; 32];
        seed[0] = 1;
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        let key_id = KeyId::of(
            &spot_bundle_signing::PublicKey::from_bytes(&signing.verifying_key().to_bytes())
                .unwrap(),
        );
        Self {
            signing,
            key_id: key_id.as_str().to_owned(),
        }
    }
}

impl StoreSigner for TestSigner {
    fn key_id(&self) -> &str {
        &self.key_id
    }

    fn public_key_bytes(&self) -> [u8; 32] {
        self.signing.verifying_key().to_bytes()
    }

    fn sign(&self, message: &[u8]) -> [u8; 64] {
        self.signing.sign(message).to_bytes()
    }
}

fn table() -> CurrencyTable {
    CurrencyTable::new([(Currency::new("USD").unwrap(), 2)]).unwrap()
}

fn usd() -> Currency {
    Currency::new("USD").unwrap()
}

/// A signed, active registry release fixture.
fn release(app: &str, publisher: &str, version: &str) -> Release {
    let digest: Digest = "ab".repeat(32).parse().unwrap();
    let key_id = KeyId::parse("0123456789abcdef0123456789abcdef").unwrap();
    let certificate = PublisherCertificate {
        publisher_id: publisher.to_owned(),
        display_name: publisher.to_owned(),
        key: generate_keypair(&[7u8; 32]).public(),
        valid_from: 0,
        valid_to: 4_000_000_000,
        issued_by: key_id.clone(),
        serial: 1,
    };
    let chain = CertificateChain::new(SignedCertificate::from_parts(certificate, [0u8; 64]));
    let signature = BundleSignature::from_parts(
        app.to_owned(),
        version.to_owned(),
        publisher.to_owned(),
        key_id,
        1_000,
        [0u8; 64],
    );
    Release {
        app: AppName::parse(app).unwrap(),
        version: version.parse::<Version>().unwrap(),
        digest,
        size: 10,
        requires_host: None,
        depends: Vec::new(),
        published_at: 1_000,
        publisher: publisher.to_owned(),
        yanked: None,
        withdrawn: None,
        signature_ref: SignatureRef { signature, chain },
        title: "My App".to_owned(),
        description: String::new(),
        category: String::new(),
        capabilities: Default::default(),
    }
}

/// A store with one verified publisher and one paid, published subscription listing.
fn store_with_paid_listing() -> Store {
    let signer = Box::new(TestSigner::new(7));
    let store = Store::new(
        signer,
        Box::new(FakePaymentProvider::new()),
        WebhookVerifier::new(SECRET),
        Box::new(FakeReviewService::new(ReviewVerdict::LooksClean)),
    )
    .unwrap();
    store
        .onboard_publisher("acme", "Acme Ltd", "payout-1", T0)
        .unwrap();
    store.publishers().verify("acme", T0 + 10).unwrap();
    store
        .register_signing_key("acme", &TestSigner::new(7).public_key_bytes(), T0 + 20)
        .unwrap();
    store
        .catalogue()
        .create("list-1", APP, "acme", "My App", "A test app", "tools", T0)
        .unwrap();
    store
        .catalogue()
        .set_pricing(
            "list-1",
            PricePlan::Subscription {
                interval: Interval::Monthly,
            },
            Some(Price::new(990, usd()).unwrap()),
            0,
            vec![],
            T0 + 30,
        )
        .unwrap();
    let submission = Submission {
        publisher: "acme".to_owned(),
        listing_id: "list-1".to_owned(),
        release: release(APP, "acme", "1.0.0"),
        bytes: b"not a zip".to_vec(),
    };
    store.submit_version(&submission, T0 + 40).unwrap();
    store
        .approve_version("reviewer-1", "list-1", &"1.0.0".parse().unwrap(), T0 + 50)
        .unwrap();
    store
}

/// The webhook header and body for a payment event.
fn webhook(event: &PaymentEvent, now: i64) -> (String, Vec<u8>) {
    let body = serde_json::to_vec(event).unwrap();
    (sign_payload(SECRET, now, &body), body)
}

fn succeeded(event_id: &str, provider_ref: &str, renewal: bool, at: i64) -> PaymentEvent {
    PaymentEvent {
        event_id: event_id.to_owned(),
        kind: PaymentEventKind::PaymentSucceeded,
        provider_ref: provider_ref.to_owned(),
        amount: Some(Price::new(990, usd()).unwrap()),
        tax_lines: Some(json!({"vat": "provider-handled"})),
        renewal,
        occurred_at: at,
    }
}

/// Drives a paid purchase and returns the store and the session id.
fn purchase(store: &Store, at: i64) -> String {
    let session = store
        .start_checkout("sess-1", TENANT, "list-1", None, at)
        .unwrap();
    assert_eq!(session.provider_ref, "fake-session-sess-1");
    let event = succeeded("evt-1", &session.provider_ref, false, at + 5);
    let (header, body) = webhook(&event, at + 5);
    let outcome = store.handle_webhook(&header, &body, at + 5).unwrap();
    assert!(outcome.applied);
    session.id
}

#[test]
fn a_free_listing_installs_without_a_checkout() {
    let signer = Box::new(TestSigner::new(7));
    let store = Store::new(
        signer,
        Box::new(FakePaymentProvider::new()),
        WebhookVerifier::new(SECRET),
        Box::new(FakeReviewService::new(ReviewVerdict::LooksClean)),
    )
    .unwrap();
    store
        .onboard_publisher("acme", "Acme Ltd", "payout-1", T0)
        .unwrap();
    store.publishers().verify("acme", T0 + 10).unwrap();
    store
        .catalogue()
        .create("list-1", APP, "acme", "My App", "", "tools", T0)
        .unwrap();
    let submission = Submission {
        publisher: "acme".to_owned(),
        listing_id: "list-1".to_owned(),
        release: release(APP, "acme", "1.0.0"),
        bytes: b"not a zip".to_vec(),
    };
    store.submit_version(&submission, T0 + 20).unwrap();
    store
        .approve_version("reviewer-1", "list-1", &"1.0.0".parse().unwrap(), T0 + 30)
        .unwrap();

    let listing = store.catalogue().get("list-1").unwrap();
    assert_eq!(listing.price_plan, PricePlan::Free);
    assert_eq!(listing.listable_versions().len(), 1);
    // A free listing has no checkout, and no entitlement is created for it.
    assert_eq!(
        store
            .start_checkout("sess-free", TENANT, "list-1", None, T0 + 40)
            .unwrap_err()
            .code(),
        "store_commerce.listing_free"
    );
    assert!(store.entitlements().all().is_empty());
}

#[test]
fn a_paid_purchase_issues_a_licence_end_to_end() {
    let store = store_with_paid_listing();
    purchase(&store, T0 + 100);

    let entitlement = store.entitlements().get(TENANT, APP).unwrap();
    assert_eq!(entitlement.plan, "app_monthly");
    assert_eq!(entitlement.licence_kind, Kind::Subscription);
    assert!(entitlement.in_force_at(T0 + 101));

    // The ledger records the order and its provider and platform fees.
    assert_eq!(store.ledger().orders().len(), 1);
    assert_eq!(store.ledger().fees().len(), 2);

    // The signed licence checks offline with the store's key alone.
    let signed = store.issue_licence(TENANT, APP, T0 + 110).unwrap();
    let check = check_licence(&signed, store.trusted_keys(), TENANT, APP, T0 + 111).unwrap();
    assert!(check.paid_features_enabled);
    assert!(check.data_readable);

    // The finance export nets the gross against the fees.
    let export = store.export_finance(&table()).unwrap();
    assert_eq!(export.net[0].gross_minor, 990);
    assert_eq!(export.net[0].fee_minor, 78);
}

#[test]
fn a_duplicate_webhook_is_applied_once() {
    let store = store_with_paid_listing();
    purchase(&store, T0 + 100);
    let event = succeeded("evt-1", "fake-session-sess-1", false, T0 + 105);
    let (header, body) = webhook(&event, T0 + 105);
    let outcome = store.handle_webhook(&header, &body, T0 + 106).unwrap();
    assert!(!outcome.applied);
    assert_eq!(store.ledger().orders().len(), 1);
    assert_eq!(store.ledger().fees().len(), 2);
    assert_eq!(store.entitlements().all().len(), 1);
}

#[test]
fn a_subscription_renewal_extends_the_licence() {
    let store = store_with_paid_listing();
    purchase(&store, T0 + 100);
    let before = store.entitlements().get(TENANT, APP).unwrap();

    let event = succeeded(
        "evt-renewal",
        "fake-session-sess-1",
        true,
        before.valid_until.unwrap(),
    );
    let (header, body) = webhook(&event, before.valid_until.unwrap());
    let outcome = store
        .handle_webhook(&header, &body, before.valid_until.unwrap())
        .unwrap();
    assert!(outcome.applied);

    let after = store.entitlements().get(TENANT, APP).unwrap();
    assert_eq!(after.serial, 2);
    assert_eq!(
        after.valid_until,
        Some(before.valid_until.unwrap() + 30 * DAY)
    );
    assert_eq!(store.ledger().orders().len(), 2);
    assert_eq!(
        store.ledger().orders()[1].kind,
        spot_store_commerce::OrderKind::Renewal
    );
    // The renewed licence verifies at an instant past the old expiry.
    let signed = store
        .issue_licence(TENANT, APP, before.valid_until.unwrap())
        .unwrap();
    let check = check_licence(
        &signed,
        store.trusted_keys(),
        TENANT,
        APP,
        before.valid_until.unwrap() + DAY,
    )
    .unwrap();
    assert!(check.paid_features_enabled);
}

#[test]
fn a_failed_payment_enters_grace_then_lapses_without_hiding_data() {
    let store = store_with_paid_listing();
    purchase(&store, T0 + 100);
    let entitlement = store.entitlements().get(TENANT, APP).unwrap();
    let expires = entitlement.valid_until.unwrap();

    // The provider reports the renewal payment failed; nothing changes structurally, the
    // entitlement's own window drives grace and lapse.
    let event = PaymentEvent {
        event_id: "evt-failed".to_owned(),
        kind: PaymentEventKind::PaymentFailed,
        provider_ref: "fake-session-sess-1".to_owned(),
        amount: None,
        tax_lines: None,
        renewal: true,
        occurred_at: expires,
    };
    let (header, body) = webhook(&event, expires);
    assert!(
        store
            .handle_webhook(&header, &body, expires)
            .unwrap()
            .applied
    );

    let signed = store.issue_licence(TENANT, APP, T0 + 100).unwrap();
    let grace = check_licence(&signed, store.trusted_keys(), TENANT, APP, expires + 1).unwrap();
    assert!(grace.paid_features_enabled);
    assert_eq!(grace.status.display_reason(), Some("licence.grace"));

    let lapsed = check_licence(
        &signed,
        store.trusted_keys(),
        TENANT,
        APP,
        expires + 7 * DAY + 1,
    )
    .unwrap();
    assert!(!lapsed.paid_features_enabled);
    assert!(lapsed.data_readable, "data stays readable after a lapse");
    assert_eq!(lapsed.status.display_reason(), Some("licence.restricted"));
}

#[test]
fn a_refund_revokes_the_licence() {
    let store = store_with_paid_listing();
    let session_id = purchase(&store, T0 + 100);
    let signed = store.issue_licence(TENANT, APP, T0 + 110).unwrap();

    let receipt = store
        .refund(&session_id, "customer changed their mind", T0 + 200)
        .unwrap();
    assert!(receipt.refund_ref.starts_with("fake-refund-"));

    let entitlement = store.entitlements().get(TENANT, APP).unwrap();
    assert!(entitlement.revoked);
    let list = store.revocations().unwrap();
    assert!(list.revokes(&entitlement.licence_id, T0 + 200));

    // The previously issued licence is now refused, offline, by the signed revocation list.
    assert_eq!(
        store
            .check_licence(&signed, TENANT, APP, T0 + 201)
            .unwrap_err()
            .code(),
        "store_commerce.licence_revoked"
    );
}

#[test]
fn a_tampered_licence_is_refused() {
    let store = store_with_paid_listing();
    purchase(&store, T0 + 100);
    let signed = store.issue_licence(TENANT, APP, T0 + 110).unwrap();
    let text = signed.to_text().unwrap();

    // Flip one character of the signature segment; the payload is untouched, so only the
    // signature check can refuse it.
    let (prefix, rest) = text.split_once('.').unwrap();
    let (payload, signature) = rest.split_once('.').unwrap();
    let mut tampered: Vec<char> = signature.chars().collect();
    tampered[0] = if tampered[0] == 'A' { 'B' } else { 'A' };
    let tampered = format!(
        "{prefix}.{payload}.{}",
        tampered.into_iter().collect::<String>()
    );

    assert_eq!(
        store
            .check_licence_text(&tampered, TENANT, APP, T0 + 111)
            .unwrap_err()
            .code(),
        "store_commerce.licence_invalid"
    );
}

#[test]
fn an_offline_check_needs_only_the_licence_and_the_store_key() {
    let store = store_with_paid_listing();
    purchase(&store, T0 + 100);
    let text = store
        .issue_licence(TENANT, APP, T0 + 110)
        .unwrap()
        .to_text()
        .unwrap();

    // Rebuild a verifier from the licence text and the store's public keys alone: no store,
    // no network, no clock read.
    let signed = SignedAppLicence::from_text(&text).unwrap();
    let check = check_licence(&signed, store.trusted_keys(), TENANT, APP, T0 + 111).unwrap();
    assert!(check.paid_features_enabled);
    assert_eq!(check.app, APP);

    // A key the store never used refuses the same licence.
    let other = TestSigner::new(9);
    let foreign = trusted_keys(&[&other as &dyn StoreSigner]).unwrap();
    assert_eq!(
        check_licence(&signed, &foreign, TENANT, APP, T0 + 111)
            .unwrap_err()
            .code(),
        "store_commerce.licence_invalid"
    );
}

#[test]
fn a_publisher_cannot_list_an_unreviewed_version() {
    let signer = Box::new(TestSigner::new(7));
    let store = Store::new(
        signer,
        Box::new(FakePaymentProvider::new()),
        WebhookVerifier::new(SECRET),
        Box::new(FakeReviewService::new(ReviewVerdict::LooksClean)),
    )
    .unwrap();
    store
        .onboard_publisher("acme", "Acme Ltd", "payout-1", T0)
        .unwrap();
    store.publishers().verify("acme", T0 + 10).unwrap();
    store
        .register_signing_key("acme", &TestSigner::new(7).public_key_bytes(), T0 + 20)
        .unwrap();
    store
        .catalogue()
        .create("list-1", APP, "acme", "My App", "", "tools", T0)
        .unwrap();
    let submission = Submission {
        publisher: "acme".to_owned(),
        listing_id: "list-1".to_owned(),
        release: release(APP, "acme", "1.0.0"),
        bytes: b"not a zip".to_vec(),
    };
    let submitted = store.submit_version(&submission, T0 + 30).unwrap();
    assert_eq!(submitted.review, ReviewStatus::NeedsHumanReview);

    let listing = store.catalogue().get("list-1").unwrap();
    assert!(listing.listable_versions().is_empty());
    assert_eq!(
        listing.versions[0].listable().unwrap_err().code(),
        "store_commerce.listing_unreviewed"
    );

    // Only a human approval lists it.
    store
        .approve_version("reviewer-1", "list-1", &"1.0.0".parse().unwrap(), T0 + 40)
        .unwrap();
    assert_eq!(
        store
            .catalogue()
            .get("list-1")
            .unwrap()
            .listable_versions()
            .len(),
        1
    );
}
