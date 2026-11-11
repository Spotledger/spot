//! Version diffs, the additive/breaking classification, the compatible range
//! for a running conversation, and `can_continue` (ADR-080 section 5.2).

// Test code may unwrap: every unwrap here is over a value the test itself
// built (spot-seams house pattern for lint-scoped integration tests).
#![allow(clippy::unwrap_used, clippy::expect_used)]

use spot_bpm_contract::{
    Change, Compatibility, Field, FieldKind, MessageDef, Outcome, OutcomeKind, Participant,
    ParticipantKind, ProcessContract, RangeError, Signature, Subject, TimeoutRule, Transition,
    can_continue, compatible_range, contracts, diff, from_text, is_compatible,
};

/// The two-party contract the tests start from.
fn base() -> ProcessContract {
    from_text(contracts::REQUEST_APPROVE).unwrap()
}

/// The changes from `base()` to `changed`, which is what a version bump looks
/// like.
fn changes(changed: &ProcessContract) -> Vec<spot_bpm_contract::ContractChange> {
    let mut before = base();
    // Signatures are about who agreed, not what changed; drop them so a diff
    // test reads as the one change it is.
    before.signatures.clear();
    let mut after = changed.clone();
    after.signatures.clear();
    diff(&before, &after)
}

/// Whether the one change is classified as expected.
fn one_change(changed: &ProcessContract, compatibility: Compatibility) -> bool {
    let found = changes(changed);
    found.len() == 1 && found[0].compatibility == compatibility
}

#[test]
fn an_identical_contract_has_no_changes() {
    assert!(diff(&base(), &base()).is_empty());
    assert!(is_compatible(&base(), &base()));
}

#[test]
fn the_contract_id_and_the_version_are_reported() {
    let mut changed = base();
    changed.id = "other".to_owned();
    let found = diff(&base(), &changed);
    assert!(
        found.iter().any(|change| change.subject == Subject::Id
            && change.compatibility == Compatibility::Breaking)
    );

    let mut bumped = base();
    bumped.version = 2;
    let found = diff(&base(), &bumped);
    assert!(found.iter().any(|change| change.subject == Subject::Version
        && change.compatibility == Compatibility::Additive));
}

#[test]
fn participants_are_classified() {
    let mut added = base();
    added
        .participants
        .push(Participant::new("auditor", ParticipantKind::Tenant));
    // An auditor with no messages is inactive, but the diff is about shape.
    assert!(one_change(&added, Compatibility::Additive));

    let mut removed = base();
    removed.participants.pop();
    assert!(one_change(&removed, Compatibility::Breaking));

    let mut kind = base();
    kind.participants[1].kind = ParticipantKind::Tenant;
    assert!(one_change(&kind, Compatibility::Breaking));
}

#[test]
fn messages_are_classified() {
    let mut added = base();
    added.messages.push(MessageDef::new(
        "note",
        "bpm.request",
        "requester",
        "approver",
        vec![Field::optional("reference", FieldKind::String)],
    ));
    // The new message is unreachable, but the diff is about shape.
    assert!(one_change(&added, Compatibility::Additive));

    let mut removed = base();
    removed.messages.pop();
    assert!(one_change(&removed, Compatibility::Breaking));

    let mut topic = base();
    topic.messages[0].topic = "bpm.other".to_owned();
    assert!(one_change(&topic, Compatibility::Breaking));

    let mut direction = base();
    direction.messages[0].to = "requester".to_owned();
    assert!(one_change(&direction, Compatibility::Breaking));

    let mut newer_schema = base();
    newer_schema.messages[0].schema_version = 2;
    assert!(one_change(&newer_schema, Compatibility::Additive));

    let mut older_schema = base();
    older_schema.messages[0].schema_version = 0;
    assert!(one_change(&older_schema, Compatibility::Breaking));
}

#[test]
fn fields_are_classified() {
    let mut optional_added = base();
    optional_added.messages[0]
        .fields
        .push(Field::optional("currency", FieldKind::String));
    assert!(one_change(&optional_added, Compatibility::Additive));

    let mut required_added = base();
    required_added.messages[0]
        .fields
        .push(Field::required("currency", FieldKind::String));
    assert!(one_change(&required_added, Compatibility::Breaking));

    let mut removed = base();
    removed.messages[0].fields.pop();
    assert!(one_change(&removed, Compatibility::Breaking));

    let mut kind = base();
    kind.messages[0].fields[1].kind = FieldKind::String;
    assert!(one_change(&kind, Compatibility::Breaking));

    let mut made_required = base();
    made_required.messages[0].fields[2].required = true;
    assert!(one_change(&made_required, Compatibility::Breaking));

    let mut made_optional = base();
    made_optional.messages[0].fields[0].required = false;
    assert!(one_change(&made_optional, Compatibility::Additive));

    let mut narrower = base();
    narrower.messages[0].fields[0].max_len = Some(16);
    assert!(one_change(&narrower, Compatibility::Breaking));

    let mut wider = base();
    wider.messages[0].fields[0].max_len = Some(128);
    assert!(one_change(&wider, Compatibility::Additive));

    let mut unbounded = base();
    unbounded.messages[0].fields[0].max_len = None;
    assert!(one_change(&unbounded, Compatibility::Additive));

    let mut bounded = base();
    bounded.messages[0].fields[1].max_len = Some(32);
    assert!(one_change(&bounded, Compatibility::Breaking));
}

#[test]
fn order_rules_are_classified() {
    // A new rule: after `approve`, the requester may `cancel` (a revision
    // path). It is additive.
    let mut added = base();
    added
        .order
        .push(Transition::message("requester", Some("approve"), "cancel"));
    let found = changes(&added);
    assert!(
        found
            .iter()
            .any(|change| change.subject == Subject::Transition
                && change.change == Change::Added
                && change.compatibility == Compatibility::Additive),
        "{found:#?}"
    );

    // Removing it again is breaking.
    let found = diff(&added, &base());
    assert!(
        found
            .iter()
            .any(|change| change.subject == Subject::Transition
                && change.change == Change::Removed
                && change.compatibility == Compatibility::Breaking),
        "{found:#?}"
    );

    let mut bounded = base();
    bounded.order[1].bound = Some(2);
    assert!(one_change(&bounded, Compatibility::Breaking));

    let mut bound = base();
    bound
        .order
        .push(Transition::message("approver", Some("approve"), "reject").with_bound(3));
    let mut lowered = bound.clone();
    lowered.order.last_mut().expect("rule").bound = Some(1);
    let found = diff(&bound, &lowered);
    assert!(
        found
            .iter()
            .any(|change| change.compatibility == Compatibility::Breaking),
        "{found:#?}"
    );
    let mut raised = bound.clone();
    raised.order.last_mut().expect("rule").bound = Some(9);
    let found = diff(&bound, &raised);
    assert!(
        found
            .iter()
            .all(|change| change.compatibility == Compatibility::Additive),
        "{found:#?}"
    );
}

#[test]
fn outcomes_are_classified() {
    let mut added = base();
    added
        .outcomes
        .push(Outcome::new("withdrawn", OutcomeKind::Cancelled));
    assert!(one_change(&added, Compatibility::Additive));

    let mut removed = base();
    removed.outcomes.pop();
    assert!(one_change(&removed, Compatibility::Breaking));

    let mut kind = base();
    kind.outcomes[0].kind = OutcomeKind::Cancelled;
    assert!(one_change(&kind, Compatibility::Breaking));
}

#[test]
fn wait_limits_are_classified() {
    let mut added = base();
    // `cancel` is received by the approver; a limit on it is new, and a new
    // bound can cut a running wait short.
    added
        .timeouts
        .push(TimeoutRule::new("cancel", "approver", 60));
    assert!(one_change(&added, Compatibility::Breaking));

    let mut removed = base();
    removed.timeouts.pop();
    assert!(one_change(&removed, Compatibility::Additive));

    let mut shorter = base();
    shorter.timeouts[0].seconds = 60;
    assert!(one_change(&shorter, Compatibility::Breaking));

    let mut longer = base();
    longer.timeouts[0].seconds = 172_800;
    assert!(one_change(&longer, Compatibility::Additive));

    let mut taken = base();
    taken.timeouts[0].outcome = None;
    assert!(one_change(&taken, Compatibility::Additive));

    let mut given = base();
    given.timeouts[0].outcome = Some("rejected".to_owned());
    assert!(one_change(&given, Compatibility::Breaking));
}

#[test]
fn signatures_never_change_what_a_conversation_may_do() {
    let mut signed = base();
    let hash = signed.content_hash();
    signed.signatures.push(Signature::new(
        "requester",
        "ed25519",
        "key-1",
        hash.as_hex(),
        "00ff",
    ));
    let found = diff(&base(), &signed);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].subject, Subject::Signature);
    assert_eq!(found[0].compatibility, Compatibility::Additive);
}

#[test]
fn a_compatible_range_walks_additive_versions_and_stops_at_a_breaking_one() {
    let v1 = base();
    let mut v2 = v1.clone();
    v2.version = 2;
    v2.messages[0]
        .fields
        .push(Field::optional("currency", FieldKind::String));
    let mut v3 = v2.clone();
    v3.version = 3;
    v3.messages[0]
        .fields
        .push(Field::optional("cost_centre", FieldKind::String));
    let mut v4 = v3.clone();
    v4.version = 4;
    v4.messages[0].fields[0].max_len = Some(8);

    let range = compatible_range(&[v4.clone(), v1.clone(), v3.clone(), v2.clone()], 1)
        .expect("the history holds version 1");
    assert_eq!(range.from, 1);
    assert_eq!(range.to, 3, "version 4 narrows a field and is breaking");

    let range = compatible_range(&[v1.clone(), v2, v3, v4], 3).expect("version 3");
    assert_eq!((range.from, range.to), (3, 3));
}

#[test]
fn a_range_is_refused_when_the_history_is_wrong() {
    let v1 = base();
    let mut v2 = v1.clone();
    v2.version = 2;
    assert_eq!(
        compatible_range(&[v1.clone(), v2.clone()], 7),
        Err(RangeError::UnknownVersion { version: 7 })
    );
    assert_eq!(
        compatible_range(&[v1.clone(), v1.clone()], 1),
        Err(RangeError::DuplicateVersion { version: 1 })
    );
    let mut foreign = v2;
    foreign.id = "other".to_owned();
    assert!(matches!(
        compatible_range(&[v1, foreign], 1),
        Err(RangeError::ForeignContract { .. })
    ));
}

#[test]
fn a_running_conversation_can_continue_when_the_change_is_additive() {
    let v1 = base();
    let mut v2 = v1.clone();
    v2.version = 2;
    v2.messages[0]
        .fields
        .push(Field::optional("currency", FieldKind::String));
    assert!(can_continue(&v1, &v2, None));
    assert!(can_continue(&v1, &v2, Some("request")));
    assert!(can_continue(&v1, &v2, Some("approve")));
}

#[test]
fn a_running_conversation_cannot_continue_past_a_breaking_change() {
    let v1 = base();

    let mut removed = v1.clone();
    removed.version = 2;
    removed.messages.retain(|m| m.name != "reject");
    assert!(!can_continue(&v1, &removed, None));

    let mut narrowed = v1.clone();
    narrowed.version = 2;
    narrowed.messages[0].fields[0].max_len = Some(8);
    // The field is still there with the same kind: a shorter text bound does
    // not change what the conversation may do, only what a payload may hold.
    assert!(can_continue(&v1, &narrowed, None));

    let mut retyped = v1.clone();
    retyped.version = 2;
    retyped.messages[0].fields[1].kind = FieldKind::String;
    assert!(!can_continue(&v1, &retyped, None));

    let mut other = v1.clone();
    other.id = "other".to_owned();
    assert!(!can_continue(&v1, &other, None));
}

#[test]
fn a_conversation_that_already_passed_a_removed_message_can_continue() {
    let v1 = base();
    let mut v2 = v1.clone();
    v2.version = 2;
    // `cancel` is only possible from `request`; a conversation already at
    // `approve` never needs it.
    v2.order
        .retain(|rule| rule.next_message() != Some("cancel"));
    v2.messages.retain(|message| message.name != "cancel");
    assert!(can_continue(&v1, &v2, Some("approve")));
    assert!(
        !can_continue(&v1, &v2, None),
        "a conversation that has not started may still need to cancel"
    );
}

#[test]
fn a_lowered_bound_stops_a_conversation_that_may_still_repeat() {
    let mut v1 = base();
    v1.order
        .push(Transition::message("approver", Some("approve"), "reject").with_bound(3));
    let mut v2 = v1.clone();
    v2.version = 2;
    v2.order.last_mut().expect("rule").bound = Some(1);
    assert!(!can_continue(&v1, &v2, Some("request")));
    assert!(!can_continue(&v1, &v2, Some("approve")));
}
