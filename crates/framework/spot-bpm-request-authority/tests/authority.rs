//! The production authority: what authenticates, and everything that must not.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, RwLock};

use common::{ACTOR, DOCTYPE, Fixture, OTHER, REQUEST, SECRET, SECRET_NAME, STEP, at, now, parts};
use spot_bpm_governance::RequestAuthority;
use spot_bpm_request_authority::{
    FixedSecrets, GRANT_PREFIX, Grant, GrantError, GrantSecrets, MAX_GRANT_LIFETIME,
    ProductionRequestAuthority, SecretRef, refuse_grant_shaped,
};
use spot_core::ManualClock;
use spot_secrets::SecretBytes;

#[test]
fn a_grant_round_trips_for_its_request_step_attempt_and_person() {
    let fixture = Fixture::new();
    let grant = fixture.issue(1);
    assert!(grant.starts_with(GRANT_PREFIX), "{grant}");

    let step = fixture
        .authority
        .authenticate(&grant, ACTOR)
        .expect("the grant it just issued authenticates");
    assert_eq!(step.request, REQUEST);
    assert_eq!(step.node, STEP);
    assert_eq!(step.attempt, 1);
    assert_eq!(
        fixture.authority.process_of(REQUEST).as_deref(),
        Some("purchase")
    );
    assert!(fixture.authority.may_create(&step, DOCTYPE));
    assert!(!fixture.authority.may_create(&step, "Sales Invoice"));
    assert_eq!(fixture.authority.process_of("req-unknown"), None);
}

#[test]
fn a_forged_tag_is_refused() {
    let fixture = Fixture::new();
    // Two grants over the same key, differing only in their expiry: each tag is real, but each
    // is the tag of the *other* payload.
    let short = fixture.issue(1);
    let long = fixture
        .authority
        .issue_until(REQUEST, STEP, 1, ACTOR, at(700))
        .unwrap();
    let (_, short_payload, short_tag) = parts(&short);
    let (_, long_payload, long_tag) = parts(&long);
    assert_ne!(short_payload, long_payload);

    let swapped = format!("{GRANT_PREFIX}{short_payload}.{long_tag}");
    assert!(fixture.authority.authenticate(&swapped, ACTOR).is_none());
    let swapped_back = format!("{GRANT_PREFIX}{long_payload}.{short_tag}");
    assert!(
        fixture
            .authority
            .authenticate(&swapped_back, ACTOR)
            .is_none()
    );

    // A tag is not optional and not empty.
    assert!(
        fixture
            .authority
            .authenticate(&format!("{GRANT_PREFIX}{short_payload}."), ACTOR)
            .is_none()
    );
    assert!(
        fixture
            .authority
            .authenticate(&format!("{GRANT_PREFIX}{short_payload}"), ACTOR)
            .is_none()
    );
    // Another format, another prefix, a payload that is not base64url: all refused.
    let wrong_prefix = format!("rg2{short_payload}.{short_tag}");
    assert!(
        fixture
            .authority
            .authenticate(&wrong_prefix, ACTOR)
            .is_none()
    );
    assert!(
        fixture
            .authority
            .authenticate(&format!("{GRANT_PREFIX}{short_payload}!{short_tag}"), ACTOR)
            .is_none()
    );
}

#[test]
fn a_changed_payload_is_refused() {
    let fixture = Fixture::new();
    let mine = fixture.issue(1);
    let (_, payload, tag) = parts(&mine);
    let grant = Grant::parse_base64(payload).expect("the payload decodes");

    // Same tag, different attempt.
    let other_attempt = Grant {
        attempt: 2,
        ..grant.clone()
    };
    let forged = format!("{GRANT_PREFIX}{}.{tag}", other_attempt.payload_base64());
    assert!(fixture.authority.authenticate(&forged, ACTOR).is_none());

    // Same tag, different person: the tag covers the actor too.
    let other_actor = Grant {
        actor: OTHER.to_owned(),
        ..grant.clone()
    };
    let forged = format!("{GRANT_PREFIX}{}.{tag}", other_actor.payload_base64());
    assert!(fixture.authority.authenticate(&forged, OTHER).is_none());

    // Same tag, a later expiry.
    let later = Grant {
        expires_at: at(900),
        ..grant
    };
    let forged = format!("{GRANT_PREFIX}{}.{tag}", later.payload_base64());
    assert!(fixture.authority.authenticate(&forged, ACTOR).is_none());
}

#[test]
fn another_persons_grant_is_refused() {
    let fixture = Fixture::new();
    let mine = fixture.issue(1);
    assert!(fixture.authority.authenticate(&mine, OTHER).is_none());
    assert!(fixture.authority.authenticate(&mine, "").is_none());
    assert!(
        fixture
            .authority
            .authenticate(&mine, "user:asha ")
            .is_none()
    );
    assert!(fixture.authority.authenticate(&mine, ACTOR).is_some());
}

#[test]
fn a_replayed_old_attempt_is_refused() {
    let fixture = Fixture::new();
    let first = fixture.issue(1);
    assert!(fixture.authority.authenticate(&first, ACTOR).is_some());

    // The engine moves the step to its next attempt and issues for it.
    fixture.store.start_attempt(REQUEST, STEP, 2);
    let second = fixture.issue(2);
    assert!(
        fixture.authority.authenticate(&first, ACTOR).is_none(),
        "the older attempt's grant must stop authenticating"
    );
    assert!(fixture.authority.authenticate(&second, ACTOR).is_some());

    // And the older attempt cannot be reissued: the store is no longer at it.
    assert_eq!(
        fixture
            .authority
            .issue_until(REQUEST, STEP, 1, ACTOR, at(600)),
        Err(GrantError::AttemptNotCurrent { attempt: 1 })
    );
}

#[test]
fn issuing_for_an_attempt_the_store_does_not_hold_is_refused() {
    let fixture = Fixture::new();
    assert_eq!(
        fixture
            .authority
            .issue_until(REQUEST, STEP, 2, ACTOR, at(600)),
        Err(GrantError::AttemptNotCurrent { attempt: 2 })
    );
    assert_eq!(
        fixture
            .authority
            .issue_until("req-unknown", STEP, 1, ACTOR, at(600)),
        Err(GrantError::AttemptNotCurrent { attempt: 1 })
    );
    assert_eq!(
        fixture
            .authority
            .issue_until(REQUEST, "close", 1, ACTOR, at(600)),
        Err(GrantError::AttemptNotCurrent { attempt: 1 })
    );
}

#[test]
fn a_blank_identifier_issues_nothing() {
    let fixture = Fixture::new();
    for (request, node, actor) in [
        ("", STEP, ACTOR),
        (" ", STEP, ACTOR),
        (REQUEST, "", ACTOR),
        (REQUEST, STEP, ""),
        (REQUEST, STEP, "  "),
    ] {
        assert_eq!(
            fixture
                .authority
                .issue_until(request, node, 1, actor, at(600)),
            Err(GrantError::IdentifierBlank),
            "{request:?} {node:?} {actor:?}"
        );
    }
}

#[test]
fn an_expired_grant_is_refused() {
    let fixture = Fixture::new();
    let grant = fixture
        .authority
        .issue_until(REQUEST, STEP, 1, ACTOR, at(600))
        .unwrap();
    assert!(fixture.authority.authenticate(&grant, ACTOR).is_some());

    fixture.clock.advance(std::time::Duration::from_secs(599));
    assert!(
        fixture.authority.authenticate(&grant, ACTOR).is_some(),
        "one second before the expiry it is still valid"
    );
    fixture.clock.advance(std::time::Duration::from_secs(1));
    assert!(
        fixture.authority.authenticate(&grant, ACTOR).is_none(),
        "at the expiry instant it is dead"
    );
}

#[test]
fn the_lifetime_is_capped_at_fifteen_minutes() {
    let fixture = Fixture::new();
    let cap = MAX_GRANT_LIFETIME.whole_seconds();
    assert_eq!(cap, 900);

    assert_eq!(
        fixture
            .authority
            .issue_until(REQUEST, STEP, 1, ACTOR, at(cap + 1)),
        Err(GrantError::LifetimeTooLong { max_minutes: 15 })
    );
    assert!(
        fixture
            .authority
            .issue_until(REQUEST, STEP, 1, ACTOR, at(cap))
            .is_ok()
    );
    assert_eq!(
        fixture
            .authority
            .issue_until(REQUEST, STEP, 1, ACTOR, now()),
        Err(GrantError::LifetimeTooLong { max_minutes: 15 })
    );
    assert_eq!(
        fixture
            .authority
            .issue_until(REQUEST, STEP, 1, ACTOR, at(-1)),
        Err(GrantError::LifetimeTooLong { max_minutes: 15 })
    );
}

#[test]
fn the_ports_issue_takes_the_cap_from_the_clock() {
    let fixture = Fixture::new();
    let grant = fixture.authority.issue(REQUEST, STEP, 1, ACTOR).unwrap();
    assert!(fixture.authority.authenticate(&grant, ACTOR).is_some());

    fixture.clock.advance(std::time::Duration::from_secs(900));
    assert!(
        fixture.authority.authenticate(&grant, ACTOR).is_none(),
        "the port issues at the cap, measured from the clock"
    );
}

#[test]
fn a_leaked_grant_stops_working_when_the_request_closes() {
    let fixture = Fixture::new();
    let grant = fixture.issue(1);
    assert!(fixture.authority.authenticate(&grant, ACTOR).is_some());

    fixture.store.close(REQUEST);
    assert!(fixture.authority.authenticate(&grant, ACTOR).is_none());
    assert_eq!(fixture.authority.process_of(REQUEST), None);
    assert_eq!(
        fixture
            .authority
            .issue_until(REQUEST, STEP, 1, ACTOR, at(600)),
        Err(GrantError::AttemptNotCurrent { attempt: 1 })
    );
}

#[test]
fn may_create_refuses_a_step_whose_attempt_moved_on() {
    let fixture = Fixture::new();
    let grant = fixture.issue(1);
    let step = fixture.authority.authenticate(&grant, ACTOR).unwrap();
    assert!(fixture.authority.may_create(&step, DOCTYPE));

    fixture.store.start_attempt(REQUEST, STEP, 2);
    assert!(
        !fixture.authority.may_create(&step, DOCTYPE),
        "an authenticated step creates nothing once its attempt has moved on"
    );
}

#[test]
fn issuing_twice_under_the_same_conditions_gives_the_same_grant() {
    let fixture = Fixture::new();
    assert_eq!(fixture.issue(1), fixture.issue(1));

    // A second authority over the same store, secret and clock agrees: nothing here is random.
    let second = ProductionRequestAuthority::new(
        fixture.store.clone(),
        fixture.secrets.clone(),
        SecretRef::new(SECRET_NAME),
        fixture.clock.clone(),
    );
    assert_eq!(
        fixture.issue(1),
        second
            .issue_until(REQUEST, STEP, 1, ACTOR, at(600))
            .unwrap()
    );
}

#[test]
fn the_secret_never_appears_in_a_grant_a_debug_line_or_an_error() {
    let fixture = Fixture::new();
    let grant = fixture.issue(1);

    assert!(!grant.contains(SECRET));
    assert!(!grant.contains("correct"));
    let debug = format!("{:?}", fixture.authority);
    assert!(!debug.contains(SECRET), "{debug}");
    assert!(!debug.contains("correct"), "{debug}");
    // The reference's name is not a secret and may show; the value may not.
    assert!(debug.contains(SECRET_NAME), "{debug}");
    let resolver = format!("{:?}", fixture.secrets);
    assert!(!resolver.contains(SECRET), "{resolver}");
    assert!(resolver.contains("[redacted]"), "{resolver}");

    let refusal = fixture
        .authority
        .issue_until(REQUEST, STEP, 1, ACTOR, at(901))
        .unwrap_err();
    assert!(!refusal.to_string().contains(SECRET));
    assert!(!refusal.to_app_error().message.contains(SECRET));
    assert!(!format!("{refusal:?}").contains(SECRET));

    let reserved = refuse_grant_shaped(&grant).unwrap_err();
    assert!(!reserved.to_string().contains(grant.as_str()));
    assert!(!reserved.to_string().contains(SECRET));
}

#[test]
fn a_short_or_missing_secret_fails_closed() {
    let mut short = FixedSecrets::new();
    short.insert(SECRET_NAME, b"ninebytes");
    let store = Fixture::new().store;
    let clock = Arc::new(ManualClock::new(now()));
    let weak = ProductionRequestAuthority::new(
        store.clone(),
        short.shared(),
        SecretRef::new(SECRET_NAME),
        clock.clone(),
    );
    assert_eq!(
        weak.issue_until(REQUEST, STEP, 1, ACTOR, at(600)),
        Err(GrantError::SecretUnavailable)
    );
    assert!(
        weak.issue(REQUEST, STEP, 1, ACTOR).is_err(),
        "the port's issue refuses when the secret is too short"
    );

    let missing = ProductionRequestAuthority::new(
        store,
        FixedSecrets::new().shared(),
        SecretRef::new("bpm.request_grant_missing"),
        clock,
    );
    assert_eq!(
        missing.issue_until(REQUEST, STEP, 1, ACTOR, at(600)),
        Err(GrantError::SecretUnavailable)
    );
}

/// A resolver that counts its calls: proves the secret is resolved per call, never cached.
struct CountingSecrets {
    inner: FixedSecrets,
    calls: AtomicUsize,
}

impl GrantSecrets for CountingSecrets {
    fn resolve(&self, reference: &SecretRef) -> Result<SecretBytes, GrantError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.inner.resolve(reference)
    }
}

#[test]
fn the_secret_is_resolved_on_every_call() {
    let fixture = Fixture::new();
    let counting = Arc::new(CountingSecrets {
        inner: {
            let mut fixed = FixedSecrets::new();
            fixed.insert(SECRET_NAME, SECRET.as_bytes());
            fixed
        },
        calls: AtomicUsize::new(0),
    });
    let authority = ProductionRequestAuthority::new(
        fixture.store.clone(),
        counting.clone(),
        SecretRef::new(SECRET_NAME),
        fixture.clock.clone(),
    );

    let grant = authority
        .issue_until(REQUEST, STEP, 1, ACTOR, at(600))
        .unwrap();
    assert_eq!(
        counting.calls.load(Ordering::SeqCst),
        1,
        "issue resolves once"
    );
    assert!(authority.authenticate(&grant, ACTOR).is_some());
    assert_eq!(
        counting.calls.load(Ordering::SeqCst),
        2,
        "authenticate resolves its own copy"
    );
    let (_, payload, tag) = parts(&grant);
    assert!(
        authority
            .authenticate(&format!("{GRANT_PREFIX}{payload}.{tag}x"), ACTOR)
            .is_none()
    );
    assert_eq!(
        counting.calls.load(Ordering::SeqCst),
        3,
        "a tag check still resolves the secret"
    );
}

/// A resolver whose value a host rotates, as the vault would.
struct RotatingSecrets {
    value: RwLock<Vec<u8>>,
}

impl GrantSecrets for RotatingSecrets {
    fn resolve(&self, _reference: &SecretRef) -> Result<SecretBytes, GrantError> {
        let value = self.value.read().unwrap().clone();
        Ok(SecretBytes::new(value))
    }
}

#[test]
fn a_rotated_secret_kills_the_grants_signed_under_the_old_one() {
    let fixture = Fixture::new();
    let rotating = Arc::new(RotatingSecrets {
        value: RwLock::new(SECRET.as_bytes().to_vec()),
    });
    let authority = ProductionRequestAuthority::new(
        fixture.store.clone(),
        rotating.clone(),
        SecretRef::new(SECRET_NAME),
        fixture.clock.clone(),
    );
    let under_old = authority
        .issue_until(REQUEST, STEP, 1, ACTOR, at(600))
        .unwrap();
    assert!(authority.authenticate(&under_old, ACTOR).is_some());

    *rotating.value.write().unwrap() = b"a completely different server secret".to_vec();
    assert!(
        authority.authenticate(&under_old, ACTOR).is_none(),
        "a grant signed under the retired secret is worth nothing"
    );
    let under_new = authority
        .issue_until(REQUEST, STEP, 1, ACTOR, at(600))
        .unwrap();
    assert!(authority.authenticate(&under_new, ACTOR).is_some());
    assert_ne!(under_old, under_new);
}

// --- the typed principal reference (ADR-083 section 2.8 item 4, ADR-080 amendment) ----------

/// A consumer's id: the same text as a user's, under the other kind.
const CONSUMER_ID: &str = "0192f1c0-0000-7000-8000-0000000000c1";

#[test]
fn a_grant_for_a_consumer_verifies_for_that_consumer_and_never_for_a_user_of_the_same_id() {
    let fixture = Fixture::new();
    let consumer = format!("consumer:{CONSUMER_ID}");
    let user = format!("user:{CONSUMER_ID}");
    let grant = fixture
        .authority
        .issue_until(REQUEST, STEP, 1, &consumer, at(600))
        .expect("a consumer reference is a typed principal");

    let step = fixture
        .authority
        .authenticate(&grant, &consumer)
        .expect("the consumer it was issued to is authenticated");
    assert_eq!((step.request.as_str(), step.node.as_str()), (REQUEST, STEP));
    assert!(
        fixture.authority.authenticate(&grant, &user).is_none(),
        "the same id as a user is another principal"
    );
    assert!(fixture.authority.authenticate(&grant, ACTOR).is_none());
    assert!(
        fixture
            .authority
            .authenticate(&grant, CONSUMER_ID)
            .is_none()
    );

    // The payload carries the typed text unchanged.
    let (_, payload, _) = parts(&grant);
    let decoded = Grant::parse_base64(payload).expect("a payload this crate wrote decodes");
    assert_eq!(decoded.actor, consumer);
}

#[test]
fn an_actor_that_is_not_a_typed_principal_reference_is_refused_at_issue_and_at_decode() {
    let fixture = Fixture::new();
    for bare in [
        "asha",
        CONSUMER_ID,
        "agent:asha",
        "Consumer:abc",
        "consumer:",
        "user:",
        "user: asha",
        "consumer:a b",
    ] {
        let refused = fixture
            .authority
            .issue_until(REQUEST, STEP, 1, bare, at(600))
            .expect_err(bare);
        assert_eq!(refused, GrantError::ActorNotTyped, "{bare:?}");
        assert_eq!(refused.code(), "bpm_request_authority.actor_not_typed");

        // A payload spelled by hand with such an actor is not a grant either, whatever its tag.
        let payload = Grant::new(REQUEST, STEP, 1, bare, at(600)).encode();
        assert!(Grant::parse(&payload).is_none(), "{bare:?}");
    }
    // `system` and the two kinds are the typed forms (what `actor_text` writes).
    for good in ["system", ACTOR, OTHER, "consumer:abc"] {
        assert!(
            fixture
                .authority
                .issue_until(REQUEST, STEP, 1, good, at(600))
                .is_ok(),
            "{good}"
        );
    }
}
