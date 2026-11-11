//! `is_grant_shaped`: both reserved shapes, and the ordinary ids that must stay ordinary.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{ACTOR, Fixture};
use spot_bpm_governance::RequestAuthority;
use spot_bpm_request_authority::{GrantError, is_grant_shaped, refuse_grant_shaped};

#[test]
fn every_rg1_grant_is_shaped() {
    for id in [
        "rg1.",
        "rg1.x",
        "rg1.payload.tag",
        "rg1.payload-with_more.tag",
        "rg1. leading space is still the prefix",
    ] {
        assert!(is_grant_shaped(id), "{id}");
    }
}

#[test]
fn the_old_plain_request_node_attempt_shape_is_shaped() {
    for id in [
        "req-7f3a:approve:2",
        "0a0b0c:node:1",
        "request:node:0",
        "a:b:4294967295",
    ] {
        assert!(is_grant_shaped(id), "{id}");
    }
}

#[test]
fn ordinary_idempotency_keys_are_not_shaped() {
    for id in [
        "019a0000-0000-7000-8000-000000000007",
        "import-2026-10-21-0042",
        "order-4711-retry",
        "8f14e45f-ea0f-4b1e-9d2a-1a1a1a1a1a1a",
        "",
        "rg1",
        "rg10.abc.def",
        "rg2.abc.def",
        "request:node",
        "request:node:",
        "request:node:next",
        ":node:1",
        "request::1",
        "request:no de:1",
        "request:node:1:2",
        "request:node:-1",
        "request.node.1",
    ] {
        assert!(!is_grant_shaped(id), "{id}");
    }
}

#[test]
fn a_real_grant_is_shaped() {
    let fixture = Fixture::new();
    let grant = fixture.issue(1);
    assert!(is_grant_shaped(&grant), "{grant}");
    assert!(fixture.authority.authenticate(&grant, ACTOR).is_some());
}

#[test]
fn a_reserved_id_is_refused_and_an_ordinary_one_is_not() {
    let reserved = refuse_grant_shaped("rg1.payload.tag").unwrap_err();
    assert_eq!(reserved, GrantError::CommandIdReserved);
    assert_eq!(reserved.code(), "bpm_request_authority.command_id_reserved");
    assert_eq!(
        reserved.to_app_error().code,
        "bpm_request_authority.command_id_reserved"
    );
    assert_eq!(reserved.to_app_error().message, reserved.to_string());

    let legacy = refuse_grant_shaped("req-7f3a:approve:2").unwrap_err();
    assert_eq!(legacy, GrantError::CommandIdReserved);

    assert_eq!(refuse_grant_shaped("import-2026-10-21-0042"), Ok(()));
    assert_eq!(
        refuse_grant_shaped("019a0000-0000-7000-8000-000000000007"),
        Ok(())
    );
}
