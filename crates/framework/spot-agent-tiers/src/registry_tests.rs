//! Unit tests alongside the registry: the deep flag's registry-only guarantee is proved
//! inside the crate (an external test cannot construct the field, and the gate lint proves
//! it — see the `deny(published)` doc test on [`AgentRegistry::register_deep`]).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::action::Domain;
use crate::identity::AgentIdentity;
use crate::registry::AgentRegistry;
use crate::tier::Tier;

#[test]
fn enroll_refuses_a_record_that_arrives_deep() {
    let mut registry = AgentRegistry::new();
    // Built inside the crate over the pub(crate) field: the shape a smuggled data record
    // would take.
    let mut smuggled = AgentIdentity::new("agent-smuggled", "owner", Domain::Support, Tier::T3);
    smuggled.registered_deep = true;
    let result = registry.enroll(smuggled);
    assert!(
        result.is_err(),
        "a deep-flagged data record cannot be enrolled"
    );
    assert!(registry.get("agent-smuggled").is_none());
}

#[test]
fn only_register_deep_raises_the_flag() {
    let mut registry = AgentRegistry::new();
    registry
        .enroll(AgentIdentity::new(
            "agent-a",
            "owner",
            Domain::Support,
            Tier::T3,
        ))
        .expect("enroll");
    let agent = registry.get("agent-a").expect("present");
    assert!(
        !agent.registered_deep(),
        "enrolment never makes an agent deep"
    );

    registry
        .register_deep("agent-a", "person", "evidence", 1)
        .expect("register");
    assert!(registry.get("agent-a").expect("present").registered_deep());

    registry.revoke_deep("agent-a", 2).expect("revoke");
    assert!(!registry.get("agent-a").expect("present").registered_deep());
}
