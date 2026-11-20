//! WP-CTL-09.T10: errors name a consent actor by an opaque reference, never by the peer's text.
#![allow(missing_docs, clippy::unwrap_used)]

use spot_tenant_link::{Error, actor_ref};

#[test]
fn the_reference_is_stable_opaque_and_distinguishes_actors() {
    let a = actor_ref("user:alice");
    assert_eq!(a, actor_ref("user:alice"));
    assert_ne!(a, actor_ref("user:bob"));
    assert!(a.starts_with("actor-") && a.len() == "actor-".len() + 12);
    assert!(!a.contains("alice"));
}

#[test]
fn display_and_debug_of_the_three_errors_carry_the_reference_only() {
    const CANARY: &str = "user:CANARY-FREE-TEXT-0451";
    let errors = [
        Error::ConsentDuplicate {
            actor: actor_ref(CANARY),
        },
        Error::StepUpRequired {
            actor: actor_ref(CANARY),
        },
        Error::ConsentStale {
            actor: actor_ref(CANARY),
            max_age_ms: 5,
        },
    ];
    for error in &errors {
        let shown = format!("{error} {error:?}");
        assert!(!shown.contains("CANARY"), "{shown}");
    }
    let shown = format!("{:?}", errors[0]);
    assert!(
        shown.contains(&actor_ref(CANARY)),
        "a legitimate actor still appears as its id: {shown}"
    );
}
