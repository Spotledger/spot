//! The two-sided offer match: the intersection both sides listed, and the refusal of a grant
//! wider than either offer.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::tenant;
use spot_tenant_link::{
    Direction, Error, FieldList, FieldName, Grant, Offer, Party, SchemaRange, Topic, TopicKind,
    grant_allowed, grants_digest, grants_match,
};

fn topic(name: &str) -> Topic {
    Topic::new(name).unwrap()
}

fn fields(names: &[&str]) -> FieldList {
    FieldList::only(names.iter().map(|name| FieldName::new(*name).unwrap()))
}

fn grant_on(
    name: &str,
    direction: Direction,
    min: u32,
    max: u32,
    allowed: FieldList,
    expires: u64,
) -> Grant {
    Grant {
        topic: topic(name),
        kind: TopicKind::Message,
        direction,
        schema_range: SchemaRange::new(min, max).unwrap(),
        fields: allowed,
        expires,
        require_peer_region: None,
        notify: false,
    }
}

fn offer(side: &Party, grants: Vec<Grant>) -> Offer {
    Offer::new(side.clone(), grants).unwrap()
}

#[test]
fn only_topics_both_sides_listed_cross() {
    let a = tenant(1);
    let b = tenant(2);
    let local = offer(
        &a,
        vec![
            grant_on("bpm.message", Direction::Send, 1, 2, fields(&["a"]), 9_000),
            grant_on("bpm.only_a", Direction::Send, 1, 2, fields(&["a"]), 9_000),
        ],
    );
    let remote = offer(
        &b,
        vec![
            grant_on(
                "bpm.message",
                Direction::Receive,
                1,
                2,
                fields(&["a"]),
                9_000,
            ),
            grant_on("bpm.only_b", Direction::Send, 1, 2, fields(&["b"]), 9_000),
        ],
    );
    let agreed = grants_match(&local, &remote).unwrap();
    assert_eq!(agreed.len(), 1);
    assert_eq!(agreed[0].topic, topic("bpm.message"));
    assert_eq!(agreed[0].direction, Direction::Send);
}

#[test]
fn directions_must_pair_from_both_views() {
    let a = tenant(1);
    let b = tenant(2);
    let both = |d| {
        offer(
            &a,
            vec![grant_on("bpm.message", d, 1, 2, fields(&["a"]), 9_000)],
        )
    };
    let peer = |d| {
        offer(
            &b,
            vec![grant_on("bpm.message", d, 1, 2, fields(&["a"]), 9_000)],
        )
    };
    assert_eq!(
        grants_match(&both(Direction::Send), &peer(Direction::Receive)).unwrap()[0].direction,
        Direction::Send
    );
    assert_eq!(
        grants_match(&both(Direction::Receive), &peer(Direction::Send)).unwrap()[0].direction,
        Direction::Receive
    );
    assert_eq!(
        grants_match(&both(Direction::Both), &peer(Direction::Both)).unwrap()[0].direction,
        Direction::Both
    );
    assert_eq!(
        grants_match(&both(Direction::Both), &peer(Direction::Send)).unwrap()[0].direction,
        Direction::Receive
    );
    assert!(matches!(
        grants_match(&both(Direction::Send), &peer(Direction::Send)),
        Err(Error::OfferConflict {
            reason: "direction",
            ..
        })
    ));
    assert!(matches!(
        grants_match(&both(Direction::Receive), &peer(Direction::Receive)),
        Err(Error::OfferConflict {
            reason: "direction",
            ..
        })
    ));
}

#[test]
fn schema_ranges_and_expiries_are_intersected() {
    let a = tenant(1);
    let b = tenant(2);
    let local = offer(
        &a,
        vec![grant_on(
            "bpm.message",
            Direction::Send,
            1,
            3,
            fields(&["a"]),
            9_000,
        )],
    );
    let remote = offer(
        &b,
        vec![grant_on(
            "bpm.message",
            Direction::Receive,
            2,
            5,
            fields(&["a"]),
            7_000,
        )],
    );
    let agreed = grants_match(&local, &remote).unwrap();
    assert_eq!(agreed[0].schema_range, SchemaRange::new(2, 3).unwrap());
    assert_eq!(agreed[0].expires, 7_000, "the earlier expiry wins");

    let remote = offer(
        &b,
        vec![grant_on(
            "bpm.message",
            Direction::Receive,
            4,
            5,
            fields(&["a"]),
            7_000,
        )],
    );
    assert!(matches!(
        grants_match(&local, &remote),
        Err(Error::OfferConflict {
            reason: "schema_range",
            ..
        })
    ));
}

#[test]
fn a_field_one_side_did_not_list_never_crosses() {
    let a = tenant(1);
    let b = tenant(2);
    let local = offer(
        &a,
        vec![grant_on(
            "bpm.message",
            Direction::Send,
            1,
            2,
            fields(&["message.body", "message.subject"]),
            9_000,
        )],
    );
    let remote = offer(
        &b,
        vec![grant_on(
            "bpm.message",
            Direction::Receive,
            1,
            2,
            fields(&["message.body", "message.private"]),
            9_000,
        )],
    );
    let agreed = grants_match(&local, &remote).unwrap();
    assert_eq!(
        agreed[0].fields,
        fields(&["message.body"]),
        "only the field both sides listed"
    );
    assert!(
        !agreed[0]
            .fields
            .includes(&FieldName::new("message.private").unwrap())
    );
}

#[test]
fn a_grant_wider_than_either_offer_is_refused_with_the_dimension() {
    let a = tenant(1);
    let b = tenant(2);
    let local_grant = grant_on(
        "bpm.message",
        Direction::Send,
        1,
        2,
        fields(&["message.body"]),
        9_000,
    );
    let local = offer(&a, vec![local_grant.clone()]);
    let remote = offer(
        &b,
        vec![grant_on(
            "bpm.message",
            Direction::Receive,
            1,
            2,
            fields(&["message.body"]),
            9_000,
        )],
    );
    // The agreed grant itself is allowed.
    assert!(grant_allowed(&local_grant, &local, &remote).is_ok());

    let wider = |change: fn(&mut Grant)| {
        let mut candidate = local_grant.clone();
        change(&mut candidate);
        grant_allowed(&candidate, &local, &remote)
    };
    assert!(matches!(
        wider(|grant| grant.direction = Direction::Both),
        Err(Error::GrantWider {
            dimension: "direction",
            ..
        })
    ));
    assert!(matches!(
        wider(|grant| grant.schema_range = SchemaRange::new(1, 3).unwrap()),
        Err(Error::GrantWider {
            dimension: "schema_range",
            ..
        })
    ));
    assert!(matches!(
        wider(|grant| grant.fields = fields(&["message.body", "message.extra"])),
        Err(Error::GrantWider {
            dimension: "fields",
            ..
        })
    ));
    assert!(matches!(
        wider(|grant| grant.expires = 9_001),
        Err(Error::GrantWider {
            dimension: "expiry",
            ..
        })
    ));
    assert!(matches!(
        wider(|grant| grant.notify = true),
        Err(Error::GrantWider {
            dimension: "notify",
            ..
        })
    ));
    assert!(matches!(
        wider(|grant| grant.topic = topic("bpm.other")),
        Err(Error::GrantWider {
            dimension: "topic",
            ..
        })
    ));
}

#[test]
fn both_sides_compute_one_agreement_digest() {
    let a = tenant(1);
    let b = tenant(2);
    let local_grant = grant_on(
        "bpm.message",
        Direction::Send,
        1,
        2,
        fields(&["message.body"]),
        9_000,
    );
    let local = offer(&a, vec![local_grant.clone()]);
    let remote = offer(
        &b,
        vec![grant_on(
            "bpm.message",
            Direction::Receive,
            1,
            2,
            fields(&["message.body"]),
            9_000,
        )],
    );
    let from_a = grants_match(&local, &remote).unwrap();
    let from_b = grants_match(&remote, &local).unwrap();
    let mut parties = [a.clone(), b.clone()];
    parties.sort();
    let digest_a = grants_digest(&from_a, &a, &parties);
    let digest_b = grants_digest(&from_b, &b, &parties);
    assert_eq!(digest_a, digest_b);
}
