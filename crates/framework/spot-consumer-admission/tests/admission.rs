//! The admission rules, against the in-process coordinator with a manual clock.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::sync::Arc;
use std::time::Duration;

use spot_consumer_admission::{
    AdmissionError, AdmissionPolicy, BAD_DESTINATION, BAD_POLICY, BAD_REQUEST, ConsumerAdmission,
    Destination, Purpose, RATE_LIMITED, Request, UNAVAILABLE,
};
use spot_coordinator::{Coordinator, InMemoryPresence, InProcessCoordinator, NodeId, RateLimit};
use spot_core::ManualClock;
use spot_throttle_coordinator::{
    CoordinatorThrottle, SaltRing, SourceAddr, ThrottlePolicy, ThrottleSalt,
};
use support::{Failing, Handle, Recording};
use time::OffsetDateTime;

const HOUR: Duration = Duration::from_secs(3600);

fn limit(capacity: u64, per: Duration) -> RateLimit {
    RateLimit { capacity, per }
}

/// A throttle with the given per-destination capacity and permissive everything else.
fn throttle_policy(destination: u64, source: u64, account: u64) -> ThrottlePolicy {
    ThrottlePolicy::new(
        limit(account, HOUR),
        limit(source, HOUR),
        limit(destination, HOUR),
        limit(1_000_000, HOUR),
    )
    .unwrap()
}

fn salt() -> ThrottleSalt {
    ThrottleSalt::new(&[7_u8; 32]).unwrap()
}

/// One admission layer over a recording in-process coordinator, plus the clock that moves time.
struct Env {
    admission: ConsumerAdmission,
    recording: Arc<Recording>,
    clock: Arc<ManualClock>,
}

async fn env_with(
    admission: AdmissionPolicy,
    throttle: ThrottlePolicy,
    coordinator: Arc<dyn Coordinator>,
    clock: Arc<ManualClock>,
) -> Env {
    let recording = Recording::new(coordinator);
    let throttle = Arc::new(CoordinatorThrottle::with_ring(
        recording.handle(),
        SaltRing::new(salt()),
        throttle,
    ));
    let admission = ConsumerAdmission::with_coordinator(throttle, recording.handle(), admission)
        .expect("the admission layer builds");
    Env {
        admission,
        recording,
        clock,
    }
}

async fn start() -> Arc<dyn Coordinator> {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("node-1").unwrap(),
        Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
    )
    .await
    .unwrap()
}

async fn env(admission: AdmissionPolicy, throttle: ThrottlePolicy) -> Env {
    // The clock the coordinator counts with is its own; the admission tests that need time to
    // pass use `Env::clock`, so the coordinator is started here with a clock we keep.
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let coordinator = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("node-1").unwrap(),
        Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
    )
    .await
    .unwrap();
    let mut env = env_with(admission, throttle, coordinator, Arc::clone(&clock)).await;
    env.clock = clock;
    env
}

fn send_code<'a>(tenant: &'a str, destination: Destination<'a>, source: &'a str) -> Request<'a> {
    Request {
        purpose: Purpose::SendCode,
        tenant,
        account: None,
        destination: Some(destination),
        source: SourceAddr::new(source),
    }
}

/// No plus tags, no cooldown, one code costing the whole destination bucket: the cheapest way to
/// see which string a bucket is keyed by.
fn wiring() -> (AdmissionPolicy, ThrottlePolicy) {
    (
        AdmissionPolicy::new(true, 3, None).unwrap(),
        throttle_policy(3, 1000, 10),
    )
}

#[tokio::test]
async fn email_spellings_share_one_destination_bucket() {
    // A destination bucket of three units and a code that costs three: exactly one code fits, so
    // the second spelling to arrive has to be refused *if* it lands in the same bucket.
    let (admission, throttle) = wiring();
    let env = env(admission, throttle).await;

    env.admission
        .admit(&send_code(
            "acme",
            Destination::Email("Alice@Example.com"),
            "10.0.0.1",
        ))
        .await
        .expect("the first code fits the bucket");

    for spelling in ["alice@example.com ", " alice@example.com"] {
        let refused = env
            .admission
            .admit(&send_code("acme", Destination::Email(spelling), "10.0.0.2"))
            .await
            .expect_err("the same mailbox in another spelling is the same bucket");
        assert_eq!(refused.code(), RATE_LIMITED);
    }

    let tagged = env
        .admission
        .admit(&send_code(
            "acme",
            Destination::Email("alice+x@example.com"),
            "10.0.0.3",
        ))
        .await
        .expect_err("a plus tag is not a new mailbox");
    assert_eq!(tagged.code(), RATE_LIMITED);

    // A different domain really is a different destination.
    env.admission
        .admit(&send_code(
            "acme",
            Destination::Email("alice@other.example"),
            "10.0.0.4",
        ))
        .await
        .expect("another domain has its own bucket");
}

#[tokio::test]
async fn phone_spellings_share_one_destination_bucket() {
    let (admission, throttle) = wiring();
    let env = env(admission, throttle).await;

    env.admission
        .admit(&send_code(
            "acme",
            Destination::Phone("+92 300-1234567"),
            "10.0.0.1",
        ))
        .await
        .expect("the first code fits the bucket");

    let refused = env
        .admission
        .admit(&send_code(
            "acme",
            Destination::Phone("+923001234567"),
            "10.0.0.2",
        ))
        .await
        .expect_err("punctuation is not a new number");
    assert_eq!(refused.code(), RATE_LIMITED);

    let refused = env
        .admission
        .admit(&send_code(
            "acme",
            Destination::Phone("+92 (300) 123 4567"),
            "10.0.0.3",
        ))
        .await
        .expect_err("parentheses are not a new number");
    assert_eq!(refused.code(), RATE_LIMITED);
}

#[tokio::test]
async fn code_pumping_is_denied_at_the_destination_while_each_source_stays_under_its_own() {
    // Six units per destination, three per code: two codes, and the third source is refused at
    // the destination although every source has made one attempt of its own.
    let admission = AdmissionPolicy::new(true, 3, None).unwrap();
    let env = env(admission, throttle_policy(6, 20, 10)).await;

    for index in 0..2 {
        let source = format!("203.0.113.{}", index + 1);
        env.admission
            .admit(&send_code(
                "acme",
                Destination::Email("victim@example.com"),
                &source,
            ))
            .await
            .expect("the first two codes fit the victim's bucket");
    }

    let mut refusals = 0;
    for index in 2..50 {
        let source = format!("203.0.113.{}", index + 1);
        match env
            .admission
            .admit(&send_code(
                "acme",
                Destination::Email("victim@example.com"),
                &source,
            ))
            .await
        {
            Ok(_) => panic!("the victim's bucket must refuse"),
            Err(error) => {
                assert_eq!(error.code(), RATE_LIMITED);
                refusals += 1;
            }
        }
    }
    assert_eq!(refusals, 48);

    // Each source has spent one attempt, so none of them is near its own limit: the pumping host
    // can still sign in.
    let sign_in = Request {
        purpose: Purpose::SignIn,
        tenant: "acme",
        account: Some("alice@example.com"),
        destination: None,
        source: SourceAddr::new("203.0.113.50"),
    };
    env.admission
        .admit(&sign_in)
        .await
        .expect("the source bucket is not the one that refused");
}

#[tokio::test]
async fn cooldown_denies_a_resend_inside_the_window_and_allows_it_after() {
    // Buckets that would happily allow every resend: only the cooldown can refuse the second.
    let admission = AdmissionPolicy::new(true, 1, Some(Duration::from_secs(60))).unwrap();
    let env = env(admission, throttle_policy(1000, 1000, 100)).await;

    env.admission
        .admit(&send_code(
            "acme",
            Destination::Email("alice@example.com"),
            "10.0.0.1",
        ))
        .await
        .expect("the first code goes out");

    let refused = env
        .admission
        .admit(&send_code(
            "acme",
            Destination::Email("alice@example.com"),
            "10.0.0.2",
        ))
        .await
        .expect_err("a resend inside the cooldown is refused");
    assert_eq!(refused.code(), RATE_LIMITED);
    let retry_after = refused.retry_after().expect("a refusal carries a wait");
    assert!(
        retry_after <= Duration::from_secs(60) && !retry_after.is_zero(),
        "the wait is the rest of the cooldown: {retry_after:?}"
    );

    env.clock.advance(Duration::from_secs(60));

    env.admission
        .admit(&send_code(
            "acme",
            Destination::Email("alice@example.com"),
            "10.0.0.3",
        ))
        .await
        .expect("after the cooldown the same destination may ask again");
}

#[tokio::test]
async fn unknown_and_known_accounts_are_indistinguishable() {
    let admission = AdmissionPolicy::new(true, 1, None).unwrap();
    let env = env(admission, throttle_policy(100, 100, 2)).await;

    let attempt = |account: &'static str| Request {
        purpose: Purpose::SignIn,
        tenant: "acme",
        account: Some(account),
        destination: None,
        source: SourceAddr::new("10.0.0.1"),
    };

    // A real name and a name nobody has take the same bucket count to be refused.
    for _ in 0..2 {
        env.admission
            .admit(&attempt("alice@example.com"))
            .await
            .expect("the first two sign-ins fit");
        env.admission
            .admit(&attempt("ghost@example.com"))
            .await
            .expect("the first two sign-ins fit");
    }
    let known = env
        .admission
        .admit(&attempt("alice@example.com"))
        .await
        .expect_err("the account bucket refuses");
    let unknown = env
        .admission
        .admit(&attempt("ghost@example.com"))
        .await
        .expect_err("the account bucket refuses");
    assert_eq!(known.code(), unknown.code());
    assert_eq!(known.retry_after(), unknown.retry_after());
}

#[tokio::test]
async fn a_failing_coordinator_refuses() {
    let clock = Arc::new(ManualClock::new(OffsetDateTime::UNIX_EPOCH));
    let started = InProcessCoordinator::start(
        Arc::new(InMemoryPresence::new()),
        NodeId::new("node-1").unwrap(),
        Arc::clone(&clock) as Arc<dyn spot_core::Clock>,
    )
    .await
    .unwrap();
    let failing = Failing::new(started, 1);
    let throttle = Arc::new(CoordinatorThrottle::with_ring(
        failing.handle(),
        SaltRing::new(salt()),
        throttle_policy(10, 10, 10),
    ));
    let admission = ConsumerAdmission::with_coordinator(
        throttle,
        failing.handle(),
        AdmissionPolicy::new(true, 1, None).unwrap(),
    )
    .unwrap();

    let refused = admission
        .admit(&send_code(
            "acme",
            Destination::Email("alice@example.com"),
            "10.0.0.1",
        ))
        .await
        .expect_err("a coordinator that cannot count is a refusal");
    assert_eq!(refused.code(), UNAVAILABLE);
}

#[tokio::test]
async fn no_raw_value_reaches_a_key_or_an_error() {
    let (admission, throttle) = wiring();
    let env = env(admission, throttle).await;

    env.admission
        .admit(&send_code(
            "acme",
            Destination::Email("Alice@Example.com"),
            "198.51.100.7",
        ))
        .await
        .expect("the first code fits the bucket");

    let keys = env.recording.keys();
    assert!(!keys.is_empty(), "the throttle took something");
    for key in &keys {
        for raw in [
            "alice",
            "Alice",
            "example.com",
            "Example",
            "198.51.100.7",
            "+1",
        ] {
            assert!(
                !key.contains(raw),
                "a bucket key carries a raw value: {key}"
            );
        }
        assert!(
            key.starts_with("thr:acme:"),
            "the tenant is in the key: {key}"
        );
    }

    // An error names the rule, never the value.
    let bad = env
        .admission
        .admit(&send_code(
            "acme",
            Destination::Email("ali\u{00e7}e@example.com"),
            "198.51.100.8",
        ))
        .await
        .expect_err("a non-ASCII local part is refused");
    assert_eq!(bad.code(), BAD_DESTINATION);
    let text = format!("{bad} {bad:?}");
    assert!(!text.contains("ali"), "an error carries the value: {text}");

    // Neither does `Debug` of a request or of an admission.
    let request = send_code(
        "acme",
        Destination::Email("alice@example.com"),
        "198.51.100.9",
    );
    let debug = format!("{request:?}");
    assert!(!debug.contains("alice@example.com"), "{debug}");
    assert!(!debug.contains("198.51.100.9"), "{debug}");
}

#[tokio::test]
async fn destinations_that_cannot_be_canonicalised_are_refused() {
    let (admission, throttle) = wiring();
    let env = env(admission, throttle).await;

    let cases = [
        Destination::Email(""),
        Destination::Email("   "),
        Destination::Email("alice"),
        Destination::Email("alice@localhost"),
        Destination::Email("@example.com"),
        Destination::Email("alice@"),
        Destination::Email("ali ce@example.com"),
        Destination::Email("ali\u{00e7}e@example.com"),
        Destination::Email("+tag@example.com"),
        Destination::Phone("03001234567"),
        Destination::Phone("+1234567"),
        Destination::Phone("+1234567890123456"),
        Destination::Phone("+92 300 123 4567 ext 9"),
    ];
    for destination in cases {
        let refused = env
            .admission
            .admit(&send_code("acme", destination, "10.0.0.1"))
            .await
            .expect_err("this destination has no canonical form");
        assert_eq!(refused.code(), BAD_DESTINATION, "{destination:?}");
    }
}

#[tokio::test]
async fn a_bad_policy_is_refused_at_construction() {
    assert_eq!(
        AdmissionPolicy::new(true, 0, None)
            .expect_err("a free code request is not a policy")
            .code(),
        BAD_POLICY
    );
    assert_eq!(
        AdmissionPolicy::new(true, 1, Some(Duration::ZERO))
            .expect_err("a zero cooldown refuses nothing")
            .code(),
        BAD_POLICY
    );
    assert_eq!(
        AdmissionPolicy::new(true, 1, Some(Duration::from_secs(86_401)))
            .expect_err("a cooldown over a day is refused")
            .code(),
        BAD_POLICY
    );

    // A cooldown cannot be armed without the coordinator that would count it.
    let throttle = Arc::new(CoordinatorThrottle::with_ring(
        start().await,
        SaltRing::new(salt()),
        throttle_policy(10, 10, 10),
    ));
    let refused = ConsumerAdmission::new(throttle, AdmissionPolicy::consumer_default())
        .expect_err("a cooldown needs the coordinator");
    assert_eq!(refused.code(), BAD_POLICY);
}

#[tokio::test]
async fn a_request_missing_what_its_purpose_counts_is_refused() {
    let (admission, throttle) = wiring();
    let env = env(admission, throttle).await;

    let code_without_destination = Request {
        purpose: Purpose::SendCode,
        tenant: "acme",
        account: Some("alice@example.com"),
        destination: None,
        source: SourceAddr::new("10.0.0.1"),
    };
    let refused = env
        .admission
        .admit(&code_without_destination)
        .await
        .expect_err("a code needs a destination");
    assert_eq!(refused.code(), BAD_REQUEST);

    let sign_in_without_account = Request {
        purpose: Purpose::SignIn,
        tenant: "acme",
        account: None,
        destination: Some(Destination::Email("alice@example.com")),
        source: SourceAddr::new("10.0.0.1"),
    };
    let refused = env
        .admission
        .admit(&sign_in_without_account)
        .await
        .expect_err("a sign-in needs an account");
    assert_eq!(refused.code(), BAD_REQUEST);

    let bad_tenant = Request {
        purpose: Purpose::SignIn,
        tenant: "ac:me",
        account: Some("alice@example.com"),
        destination: None,
        source: SourceAddr::new("10.0.0.1"),
    };
    let refused = env
        .admission
        .admit(&bad_tenant)
        .await
        .expect_err("a tenant with a colon is not a plain id");
    assert_eq!(refused.code(), BAD_REQUEST);
}

#[tokio::test]
async fn a_registration_counts_the_destination_not_the_account() {
    // The account bucket is tiny: if a registration counted it, the second one would be refused.
    let admission = AdmissionPolicy::new(true, 1, None).unwrap();
    let env = env(admission, throttle_policy(100, 100, 1)).await;

    for (index, source) in ["10.0.0.1", "10.0.0.2"].iter().enumerate() {
        let request = Request {
            purpose: Purpose::Register,
            tenant: "acme",
            account: Some("alice@example.com"),
            destination: Some(Destination::Email("alice@example.com")),
            source: SourceAddr::new(source),
        };
        env.admission
            .admit(&request)
            .await
            .unwrap_or_else(|error| panic!("registration {index} refused: {error}"));
    }

    // ... and the destination is counted, so the same mailbox cannot register forever.
    let refused = env
        .admission
        .admit(&Request {
            purpose: Purpose::Register,
            tenant: "acme",
            account: Some("bob@example.com"),
            destination: Some(Destination::Email("alice@example.com")),
            source: SourceAddr::new("10.0.0.3"),
        })
        .await;
    // Two registrations of 100 units leave plenty; the point is the shape, so this one passes.
    assert!(refused.is_ok(), "the destination bucket is far from full");
}

#[tokio::test]
async fn the_canonical_form_is_what_the_caller_receives() {
    let (admission, throttle) = wiring();
    let env = env(admission, throttle).await;

    let admitted = env
        .admission
        .admit(&send_code(
            "acme",
            Destination::Email(" Alice+news@Example.COM "),
            "10.0.0.1",
        ))
        .await
        .expect("the first code fits");
    assert_eq!(
        admitted
            .destination()
            .map(|destination| destination.as_str()),
        Some("alice@example.com")
    );

    let admitted = env
        .admission
        .admit(&send_code(
            "acme",
            Destination::Phone("+92 (300) 123-4567"),
            "10.0.0.2",
        ))
        .await
        .expect("the second destination has its own bucket");
    assert_eq!(
        admitted
            .destination()
            .map(|destination| destination.as_str()),
        Some("+923001234567")
    );
}

#[tokio::test]
async fn the_tenant_is_part_of_every_key() {
    let (admission, throttle) = wiring();
    let env = env(admission, throttle).await;

    // The same destination in two tenants is two buckets: the second tenant is not refused by the
    // first tenant's spend.
    env.admission
        .admit(&send_code(
            "acme",
            Destination::Email("alice@example.com"),
            "10.0.0.1",
        ))
        .await
        .expect("the first tenant's code fits");
    env.admission
        .admit(&send_code(
            "globex",
            Destination::Email("alice@example.com"),
            "10.0.0.1",
        ))
        .await
        .expect("another tenant has its own bucket");

    assert!(
        env.recording
            .keys()
            .iter()
            .any(|key| key.starts_with("thr:globex:")),
        "no key was written for the second tenant"
    );
}

#[tokio::test]
async fn an_admission_error_is_a_refusal_not_a_verdict() {
    let error = AdmissionError::RateLimited {
        retry_after: Duration::from_secs(5),
    };
    assert_eq!(error.retry_after(), Some(Duration::from_secs(5)));
    assert!(
        error
            .to_string()
            .contains("consumer_admission.rate_limited")
    );
    assert!(
        AdmissionError::Unavailable {
            cause: "coordinator.unavailable"
        }
        .to_string()
        .contains("refuse")
    );
}
