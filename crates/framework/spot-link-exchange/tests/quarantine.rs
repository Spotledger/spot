//! The input quarantine's own count (`InputQuarantine::held_count`): what a caller bounds a
//! partner by, so an input an operator released frees its slot and an implementer that cannot
//! count says so instead of answering zero.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{FAR, T0, conversation, message_topic, tenant};
use spot_link_exchange::{
    InputQuarantine, MemoryQuarantine, Message, MessageId, MessageParts, QuarantinedInput,
    RejectCode,
};
use spot_tenant_link::{LinkId, SeedSigner};

fn link(byte: u8) -> LinkId {
    LinkId::new_v7(1_700_000_000_000, [byte; 10])
}

/// A refused input for `link`; `n` makes it a distinct message id.
fn refused_input(link: LinkId, n: u64) -> QuarantinedInput {
    let sender = tenant(1);
    let signer = SeedSigner::from_seed([1u8; 32], sender);
    let mut entropy = [0u8; 10];
    entropy[..8].copy_from_slice(&n.to_be_bytes());
    let message = Message::sign(
        MessageParts {
            id: MessageId::new_v7(T0, entropy),
            link,
            conversation: conversation("quarantine_count"),
            seq: n + 1,
            topic: message_topic(),
            schema_version: 1,
            sent_at: T0,
            expires_at: FAR,
            tombstone: false,
            replaces: None,
            payload: "{\"message.body\":\"refused\"}".to_owned(),
        },
        &signer,
    )
    .unwrap();
    QuarantinedInput {
        link,
        message,
        code: RejectCode::PayloadInvalid,
        at: T0,
    }
}

/// The count is per link and counted when asked: one partner's inputs never count against
/// another's bound.
#[test]
fn the_held_count_is_per_link_and_counted_now() {
    let quarantine = MemoryQuarantine::new();
    let (here, other) = (link(1), link(2));
    assert_eq!(quarantine.held_count(&here).unwrap(), 0);
    quarantine.hold(refused_input(here, 0)).unwrap();
    quarantine.hold(refused_input(here, 1)).unwrap();
    quarantine.hold(refused_input(other, 2)).unwrap();
    assert_eq!(quarantine.held_count(&here).unwrap(), 2);
    assert_eq!(quarantine.held_count(&other).unwrap(), 1);
    assert_eq!(quarantine.held_count(&link(3)).unwrap(), 0);
}

/// An input an operator released from the quarantine stops occupying a slot at once, which is
/// what makes the bound usable again; releasing it twice changes nothing.
#[test]
fn releasing_a_held_input_frees_its_slot() {
    let quarantine = MemoryQuarantine::new();
    let here = link(1);
    let first = refused_input(here, 0);
    let second = refused_input(here, 1);
    quarantine.hold(first.clone()).unwrap();
    quarantine.hold(second.clone()).unwrap();
    assert_eq!(quarantine.held_count(&here).unwrap(), 2);

    let released = quarantine.release(&second.message.id).unwrap();
    assert_eq!(released.message.id, second.message.id);
    assert_eq!(quarantine.held_count(&here).unwrap(), 1);
    assert!(quarantine.release(&second.message.id).is_none());
    assert_eq!(quarantine.held_count(&here).unwrap(), 1);

    // What is released can be held again (a redelivery the operator brought back).
    quarantine.hold(second).unwrap();
    assert_eq!(quarantine.held_count(&here).unwrap(), 2);
}

/// `hold` is idempotent on the message id, so a repeat costs no second slot.
#[test]
fn holding_the_same_message_twice_counts_once() {
    let quarantine = MemoryQuarantine::new();
    let here = link(1);
    let input = refused_input(here, 0);
    quarantine.hold(input.clone()).unwrap();
    quarantine.hold(input).unwrap();
    assert_eq!(quarantine.held_count(&here).unwrap(), 1);
    assert_eq!(quarantine.held().len(), 1);
}

/// An implementer that cannot count answers the port's default, which says so rather than
/// reading as "nothing held": a caller bounds a partner by this number and must fail closed. The
/// answer is the crate's own `Error`, so a caller handles one error type on this port.
#[test]
fn an_implementer_without_a_count_says_so() {
    let quarantine = common::FailingQuarantine;
    let unavailable = quarantine.held_count(&link(1)).unwrap_err();
    assert!(
        matches!(
            unavailable,
            spot_link_exchange::Error::QuarantineCountUnavailable
        ),
        "the count failure is a variant of the crate's `Error`, not a type of its own"
    );
    assert_eq!(
        unavailable.code(),
        "link_exchange.quarantine_count_unavailable",
        "the code is the catalogue's, unchanged"
    );
    // The default is not a licence to hold: this implementer still fails to hold.
    assert!(quarantine.hold(refused_input(link(1), 0)).is_err());
}

/// `holds` answers for the link it is asked about, and stops answering once the input is released:
/// the same scope the count takes.
#[test]
fn holds_is_per_link_and_follows_a_release() {
    let quarantine = MemoryQuarantine::new();
    let (here, other) = (link(1), link(2));
    let input = refused_input(here, 0);
    assert!(!quarantine.holds(&here, &input.message.id).unwrap());
    quarantine.hold(input.clone()).unwrap();
    assert!(quarantine.holds(&here, &input.message.id).unwrap());
    assert!(
        !quarantine.holds(&other, &input.message.id).unwrap(),
        "another link holds nothing of this input"
    );
    assert!(
        !quarantine
            .holds(&here, &refused_input(here, 9).message.id)
            .unwrap(),
        "an id it was never given is not held"
    );

    quarantine.release(&input.message.id).unwrap();
    assert!(
        !quarantine.holds(&here, &input.message.id).unwrap(),
        "a released input is no longer held"
    );
}

/// The default `holds` answers "not held", so a quarantine that cannot answer is still bounded by
/// its count and a redelivery cannot pass a bound it is really at.
#[test]
fn an_implementer_without_holds_says_not_held() {
    let quarantine = common::FailingQuarantine;
    assert!(
        !quarantine
            .holds(&link(1), &MessageId::new_v7(T0, [7; 10]))
            .unwrap()
    );
}
