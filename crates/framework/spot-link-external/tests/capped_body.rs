//! The inbound body is read under the peer's cap, and only there: an unauthenticated
//! sender must not be able to make the edge buffer more than the peer's cap before the receiver
//! ever runs.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, missing_docs)]

mod common;

use common::*;
use spot_link_external::{InboundRequest, codes};

const CAP: usize = 64;

#[test]
fn a_body_over_the_cap_stops_one_byte_past_it() {
    // One-byte chunks, ten thousand of them: the reader must stop at cap + 1 bytes and leave
    // the rest of the stream unread, so neither the buffer nor the sender's stream decides how
    // much memory this request costs.
    let mut body = Bytes::new(10_000, 1);
    let error =
        InboundRequest::from_stream("/links/acme/inbound", Vec::new(), &mut body, CAP).unwrap_err();
    assert_eq!(error.code(), codes::BODY_TOO_LARGE);
    assert_eq!(
        body.pulled,
        CAP + 1,
        "the reader reads exactly one byte past the cap, never the whole body"
    );
    assert!(
        body.next().is_some(),
        "the stream still had bytes left: the reader stopped, it did not drain"
    );
}

#[test]
fn a_body_over_the_cap_in_large_chunks_is_refused_without_reading_further() {
    // Chunks of 1 000 bytes against a cap of 64: the first chunk already crosses the cap, so
    // the read stops there. The reader copies at most `cap` bytes of it (the unit test on the
    // reservation proves the buffer never grows past the cap) and never asks for the rest of
    // the stream.
    let mut body = Bytes::new(10_000, 1_000);
    let error =
        InboundRequest::from_stream("/links/acme/inbound", Vec::new(), &mut body, CAP).unwrap_err();
    assert_eq!(error.code(), codes::BODY_TOO_LARGE);
    assert_eq!(
        body.pulled, 1_000,
        "one chunk is taken, then the read stops"
    );
}

#[test]
fn a_declared_length_over_the_cap_is_refused_before_one_byte_is_read() {
    // The header is a claim, and this one is over the cap: zero chunks are pulled, so a sender
    // cannot buy memory with a `Content-Length` either.
    let mut body = Bytes::new(1_000, 16);
    let headers = vec![("content-length".to_owned(), (CAP * 4).to_string())];
    let error =
        InboundRequest::from_stream("/links/acme/inbound", headers, &mut body, CAP).unwrap_err();
    assert_eq!(error.code(), codes::BODY_TOO_LARGE);
    assert_eq!(
        body.pulled, 0,
        "nothing is read from a declared over-cap body"
    );
}

#[test]
fn a_declared_length_that_is_not_a_number_is_refused_before_one_byte_is_read() {
    // Fail closed: a length the reader cannot trust is not treated as "no length".
    let mut body = Bytes::new(1_000, 16);
    let headers = vec![("Content-Length".to_owned(), "sixty-four".to_owned())];
    let error =
        InboundRequest::from_stream("/links/acme/inbound", headers, &mut body, CAP).unwrap_err();
    assert_eq!(error.code(), codes::INVALID_BODY);
    assert_eq!(body.pulled, 0);
}

#[test]
fn a_body_at_exactly_the_cap_passes() {
    let body = Bytes::new(CAP, 7);
    let request = InboundRequest::from_stream("/links/acme/inbound", Vec::new(), body, CAP)
        .expect("a body at the cap is accepted");
    assert_eq!(request.path, "/links/acme/inbound");
    assert_eq!(request.body().len(), CAP);
    // And one byte more does not.
    let over = Bytes::new(CAP + 1, 7);
    assert_eq!(
        InboundRequest::from_stream("/links/acme/inbound", Vec::new(), over, CAP)
            .unwrap_err()
            .code(),
        codes::BODY_TOO_LARGE
    );
}

#[test]
fn the_route_reads_under_the_peers_cap_and_never_drains_a_stream() {
    let mut world = World::new();
    world.peer.max_body_bytes = u64::try_from(CAP).unwrap();
    let path = world.peer.inbound.path.clone();
    let mut body = Bytes::new(10_000, 1);
    let error = world.read(&path, Vec::new(), &mut body).unwrap_err();
    assert_eq!(error.code(), codes::BODY_TOO_LARGE);
    assert_eq!(body.pulled, CAP + 1);
    // The peer's cap, not a caller's, is what the route reads under.
    assert!(world.read(&path, Vec::new(), Bytes::new(CAP, 1)).is_ok());
}

#[test]
fn a_peer_without_a_cap_is_refused_rather_than_read_without_one() {
    let mut world = World::new();
    world.peer.max_body_bytes = 0;
    let path = world.peer.inbound.path.clone();
    let mut body = Bytes::new(1_000, 1);
    let error = world.read(&path, Vec::new(), &mut body).unwrap_err();
    assert_eq!(error.code(), codes::INVALID_PEER);
    assert_eq!(body.pulled, 0, "a missing cap is never an unlimited read");
}
