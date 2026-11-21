//! A consumer actor (ADR-083 section 2.2 item 4): typed in text, audited as a consumer, and never
//! a principal with a role.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_app_api::context::Actor;
use spot_app_api::prelude::{OffsetDateTime, RecordId};
use spot_bpm_governance::{
    GovernanceAudit, MemoryPrincipals, PrincipalSource, actor_text, outbox_event_of,
};
use spot_events_contract::PayloadDocument;

fn consumer() -> (RecordId, Actor) {
    let id = RecordId::new_v7();
    (id, Actor::Consumer { id })
}

#[test]
fn a_consumer_actor_is_typed_text_and_never_a_user() {
    let (id, actor) = consumer();
    assert_eq!(actor_text(&actor), format!("consumer:{id}"));
    assert_ne!(
        actor_text(&actor),
        actor_text(&Actor::User { id, via: None }),
        "the same id as a user is another principal"
    );
}

#[test]
fn a_consumer_resolves_to_a_principal_without_any_role() {
    let (id, actor) = consumer();
    let principals = MemoryPrincipals::new();
    // Even a user row that shares the id, with roles, is not what a consumer resolves to.
    principals.insert(
        id,
        spot_permissions::Principal::with_roles("staff", &["Accounts Manager"]),
    );
    let principal = principals.principal(&actor);
    assert!(principal.assignments.is_empty(), "{principal:?}");
    assert!(!principal.is_system);
    assert_eq!(principal.user, format!("consumer:{id}"));
}

#[test]
fn the_audit_record_and_event_name_the_consumer_not_a_user() {
    let (id, actor) = consumer();
    let entry = GovernanceAudit::refused(
        &actor,
        "acme",
        "web_order",
        RecordId::new_v7(),
        spot_bpm_governance::GovernanceMode::Governed,
        "not a declared entry point",
        Some("idem-1"),
        OffsetDateTime::UNIX_EPOCH,
    );
    let record = entry.to_record();
    assert_eq!(record.actor, format!("consumer:{id}"));
    assert_eq!(record.on_behalf_of, None);

    let event = outbox_event_of(&entry).unwrap();
    let payload = PayloadDocument::from_outbox_event(&event).unwrap();
    assert_eq!(
        payload.meta.actor,
        spot_events_contract::Actor::Consumer { id }
    );
}
