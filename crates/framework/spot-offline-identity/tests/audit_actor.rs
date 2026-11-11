//! The audit actor namespace: `offline-identity:<kind>:<id>`, never `user:<id>`.
//!
//! The stamp parser trusts the `user:<id>` prefix for the user who acted. If an audit actor
//! used the same prefix, a hand-built actor string could pass for a stamped user, so the two
//! namespaces are kept apart and neither parses as the other.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use spot_offline_identity::audit::{
    ACTOR_NAMESPACE, ACTOR_NODE, ACTOR_USER, STAMP_ACTOR_PREFIX, actor, actor_parts,
};
use spot_offline_identity::*;

fn stamp() -> OfflineStamp {
    OfflineStamp {
        tenant: common::tenant(),
        node: common::node(),
        author: "clerk".to_owned(),
        assignment_id: "a-clerk".to_owned(),
        tier: Tier::Three,
        epoch: 2,
        epoch_age_ms: 1_000,
        device_seq: 1,
    }
}

#[test]
fn an_audit_actor_is_in_this_crates_namespace() {
    assert_eq!(ACTOR_NAMESPACE, "offline-identity");
    assert_eq!(actor(ACTOR_USER, "sup"), "offline-identity:user:sup");
    assert_eq!(
        actor(ACTOR_NODE, "branch-1"),
        "offline-identity:node:branch-1"
    );
    assert_eq!(
        actor_parts("offline-identity:user:sup"),
        Some(("user", "sup"))
    );
    assert_eq!(
        actor_parts("offline-identity:node:branch-1"),
        Some(("node", "branch-1"))
    );
    // The id may itself hold a colon: it is split at the first one.
    assert_eq!(
        actor_parts(&actor(ACTOR_NODE, "branch-1:7")),
        Some(("node", "branch-1:7"))
    );
}

#[test]
fn a_hand_built_actor_in_the_other_namespace_is_not_accepted() {
    // A stamp's actor is a user, and is never read as an offline-identity actor.
    assert_eq!(actor_parts("user:sup"), None);
    assert_eq!(actor_parts("user:"), None);
    // A malformed offline-identity actor is not accepted either.
    assert_eq!(actor_parts("offline-identity:user:"), None);
    assert_eq!(actor_parts("offline-identity::sup"), None);
    assert_eq!(actor_parts("offline-identity:sup"), None);
    assert_eq!(actor_parts("Offline-Identity:user:sup"), None);
}

#[test]
fn a_stamp_actor_stays_a_user_and_the_audit_namespace_is_not_a_stamp_user() {
    let s = stamp();
    let record = s.to_record(7);
    assert_eq!(record.actor, format!("{STAMP_ACTOR_PREFIX}clerk"));
    assert_eq!(OfflineStamp::from_record(&record).unwrap(), s);

    // A hand-built actor in the audit namespace must not be read as a stamp's user.
    let mut forged = record.clone();
    forged.actor = actor(ACTOR_USER, "clerk");
    assert_eq!(
        OfflineStamp::from_record(&forged),
        Err(OfflineError::StampMalformed),
        "an audit actor is not a stamp user"
    );
    // And the same holds for a node-kind actor.
    forged.actor = actor(ACTOR_NODE, "branch-1");
    assert_eq!(
        OfflineStamp::from_record(&forged),
        Err(OfflineError::StampMalformed)
    );
}
