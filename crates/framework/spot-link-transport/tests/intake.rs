//! The host intake routes (WP-CTL-10.T7) through the real router with the loopback transport: the
//! gate, the second-tier limiter, the ownership checks and the mapping of the port's refusals.
//! The store-backed semantics (nonces, pinned signers) are proved in `spot-tenant-link-pg`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![allow(missing_docs)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::net::code_of;
use common::*;
use spot_link_transport::{
    CarrierTable, IntakeRefusal, IntakeSettings, LinkIntake, NonceSweep, RateConfig, codes,
};
use spot_sync_http::{HttpRequest, LoopbackTransport, Transport};
use spot_tenant_link::{
    ConsentPolicy, HandshakeMessage, Party, RevokeMessage, RevokeReason, SeedSigner,
};

/// A port that really verifies each signature (and counts the verifications), refuses a nonce it
/// has seen, and records what it was asked to prune.
#[derive(Default)]
struct Counting {
    verifications: AtomicUsize,
    nonces: Mutex<BTreeSet<[u8; 16]>>,
    pruned: Mutex<Vec<u64>>,
    store_down: std::sync::atomic::AtomicBool,
}

impl LinkIntake for Counting {
    fn handshake(
        &self,
        _: &Party,
        message: &HandshakeMessage,
        _: u64,
    ) -> Result<(), IntakeRefusal> {
        self.verifications.fetch_add(1, Ordering::SeqCst);
        if self.store_down.load(Ordering::SeqCst) {
            return Err(IntakeRefusal::Unavailable);
        }
        message.verify().map_err(|_| IntakeRefusal::Forged)?;
        if !self
            .nonces
            .lock()
            .unwrap()
            .insert(*message.nonce.as_bytes())
        {
            return Err(IntakeRefusal::Replayed);
        }
        Ok(())
    }

    fn revoke(&self, _: &Party, message: &RevokeMessage, _: u64) -> Result<(), IntakeRefusal> {
        self.verifications.fetch_add(1, Ordering::SeqCst);
        message.verify().map_err(|_| IntakeRefusal::Forged)?;
        if !self
            .nonces
            .lock()
            .unwrap()
            .insert(*message.nonce.as_bytes())
        {
            return Err(IntakeRefusal::Replayed);
        }
        Ok(())
    }

    fn prune_nonces(&self, now: u64, _: ConsentPolicy) -> Result<u64, IntakeRefusal> {
        self.pruned.lock().unwrap().push(now);
        Ok(3)
    }
}

fn settings(burst: u32, max_keys: usize) -> IntakeSettings {
    IntakeSettings {
        rate: RateConfig {
            burst,
            per_second: 1,
            max_keys,
        },
        ..IntakeSettings::default()
    }
}

fn post(path: &str, body: &impl serde::Serialize) -> HttpRequest {
    HttpRequest::post(path.to_owned(), serde_json::to_vec(body).unwrap())
        .with_content_type("application/json")
}

struct Rig {
    w: World,
    port: Arc<Counting>,
    server: spot_link_transport::LinkServer,
    directory: Arc<spot_link_transport::PinnedCertificates>,
}

fn rig(settings: IntakeSettings) -> Rig {
    let w = world();
    let (server, directory) = server_for(
        &w.b,
        &w.a.party,
        &certificate(1),
        Arc::new(CarrierTable::new()),
    );
    let port = Arc::new(Counting::default());
    let server = server.with_intake(port.clone(), settings);
    Rig {
        w,
        port,
        server,
        directory,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_genuine_proposal_is_taken_once_and_a_replay_is_refused_409() {
    let r = rig(IntakeSettings::default());
    let t = LoopbackTransport::new(r.server.router(), certificate(1));
    let (message, _) = proposal(11, &r.w.a.party, &r.w.b.party, 1);
    let first = t
        .request(post("/link/v1/handshake", &message))
        .await
        .unwrap();
    assert_eq!(first.status, 200);
    let again = t
        .request(post("/link/v1/handshake", &message))
        .await
        .unwrap();
    assert_eq!(again.status, 409);
    let body = String::from_utf8_lossy(&again.body);
    assert!(body.contains("link_transport.replayed"), "{body}");
    assert!(!body.contains("invalid_request"), "{body}");
    assert!(!body.contains("nonce"));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forged_proposal_is_refused_403_and_claims_nothing() {
    let r = rig(IntakeSettings::default());
    let t = LoopbackTransport::new(r.server.router(), certificate(1));
    let (mut message, _) = proposal(11, &r.w.a.party, &r.w.b.party, 1);
    message.signature = spot_tenant_link::SignatureBytes::from_bytes([7u8; 64]);
    let refused = t
        .request(post("/link/v1/handshake", &message))
        .await
        .unwrap();
    assert_eq!(
        (refused.status, code_of(&refused).as_str()),
        (403, codes::FORBIDDEN)
    );
    assert!(r.port.nonces.lock().unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_proposal_from_someone_else_than_the_caller_never_reaches_the_port() {
    let r = rig(IntakeSettings::default());
    let t = LoopbackTransport::new(r.server.router(), certificate(1));
    let stranger = tenant(5);
    let (message, _) = proposal(12, &stranger, &r.w.b.party, 2);
    let refused = t
        .request(post("/link/v1/handshake", &message))
        .await
        .unwrap();
    assert_eq!(refused.status, 403);
    assert_eq!(r.port.verifications.load(Ordering::SeqCst), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_later_message_must_belong_to_a_link_the_caller_is_the_peer_of() {
    let r = rig(IntakeSettings::default());
    // C is enrolled but is not the peer of the link a `reject` names.
    r.directory.pin(certificate(3).fingerprint(), tenant(3));
    let t = LoopbackTransport::new(r.server.router(), certificate(3));
    let signer = SeedSigner::from_seed([13u8; 32], tenant(3));
    let message = reject_for(&signer, r.w.b.link);
    let refused = t
        .request(post("/link/v1/handshake", &message))
        .await
        .unwrap();
    assert_eq!(refused.status, 403);
    assert_eq!(r.port.verifications.load(Ordering::SeqCst), 0);
}

fn reject_for(signer: &SeedSigner, link: spot_tenant_link::LinkId) -> HandshakeMessage {
    // A hand-built message: only its shape matters, the port is never reached.
    HandshakeMessage {
        revision: 1,
        previous: None,
        nonce: spot_tenant_link::Nonce::from_bytes([9u8; 16]),
        signer: spot_tenant_link::IdentitySigner::public_key(signer),
        body: spot_tenant_link::HandshakeBody::Reject(spot_tenant_link::RejectBody {
            link,
            reason: spot_tenant_link::RejectReason::TermsDeclined,
        }),
        signature: spot_tenant_link::SignatureBytes::from_bytes([0u8; 64]),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoke_by_the_peer_is_applied_once_and_a_replay_is_refused() {
    let r = rig(IntakeSettings::default());
    let t = LoopbackTransport::new(r.server.router(), certificate(1));
    let message = RevokeMessage::sign(
        r.w.a.signer.as_ref(),
        r.w.b.link,
        RevokeReason::TermsEnded,
        spot_tenant_link::Nonce::from_bytes([21u8; 16]),
    );
    let path = format!("/link/v1/{}/revoke", r.w.b.link);
    assert_eq!(t.request(post(&path, &message)).await.unwrap().status, 200);
    let again = t.request(post(&path, &message)).await.unwrap();
    assert_eq!(again.status, 409);
    assert!(String::from_utf8_lossy(&again.body).contains("link_transport.replayed"));
    // Signed by a stranger: forged.
    let stranger = SeedSigner::from_seed([99u8; 32], r.w.a.party.clone());
    let forged = RevokeMessage::sign(
        &stranger,
        r.w.b.link,
        RevokeReason::TermsEnded,
        spot_tenant_link::Nonce::from_bytes([22u8; 16]),
    );
    let mut forged = forged;
    forged.reason = RevokeReason::SecurityConcern;
    assert_eq!(t.request(post(&path, &forged)).await.unwrap().status, 403);
    // A path that names another link than the message does.
    let other = "/link/v1/0192f1c0-0000-7000-8000-000000000000/revoke";
    assert_eq!(t.request(post(other, &message)).await.unwrap().status, 403);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_store_failure_fails_closed_with_503() {
    let r = rig(IntakeSettings::default());
    r.port.store_down.store(true, Ordering::SeqCst);
    let t = LoopbackTransport::new(r.server.router(), certificate(1));
    let (message, _) = proposal(11, &r.w.a.party, &r.w.b.party, 1);
    let down = t
        .request(post("/link/v1/handshake", &message))
        .await
        .unwrap();
    assert_eq!(
        (down.status, code_of(&down).as_str()),
        (503, codes::UNAVAILABLE)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_flood_from_one_peer_is_cut_off_before_any_signature_check() {
    let r = rig(settings(3, 100));
    r.directory.pin(certificate(2).fingerprint(), tenant(3));
    let router = r.server.router();
    let a = LoopbackTransport::new(router.clone(), certificate(1));
    let mut statuses = Vec::new();
    for n in 0..10u8 {
        let (message, _) = proposal(11, &r.w.a.party, &r.w.b.party, n + 1);
        let response = a
            .request(post("/link/v1/handshake", &message))
            .await
            .unwrap();
        statuses.push(response.status);
    }
    assert_eq!(statuses.iter().filter(|s| **s == 200).count(), 3);
    assert_eq!(statuses.iter().filter(|s| **s == 429).count(), 7);
    assert_eq!(
        r.port.verifications.load(Ordering::SeqCst),
        3,
        "a limited request is never verified"
    );
    assert_eq!(r.server.intake_counters().limited(), 7);

    // Another peer has its own bucket.
    let b = LoopbackTransport::new(router, certificate(2));
    let (message, _) = proposal(14, &tenant(3), &r.w.b.party, 77);
    let other = b
        .request(post("/link/v1/handshake", &message))
        .await
        .unwrap();
    assert_eq!(other.status, 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_limiter_table_stays_bounded_and_outsiders_create_no_keys() {
    let r = rig(settings(1, 2));
    let router = r.server.router();
    // Fifty unpinned certificates: refused by the directory, no key, no verification.
    for seed in 100..150u8 {
        let t = LoopbackTransport::new(router.clone(), certificate(seed));
        let (message, _) = proposal(11, &r.w.a.party, &r.w.b.party, 1);
        let refused = t
            .request(post("/link/v1/handshake", &message))
            .await
            .unwrap();
        assert_eq!(refused.status, 403);
    }
    assert_eq!(r.port.verifications.load(Ordering::SeqCst), 0);
    assert_eq!(r.server.intake_counters().limited(), 0);
    // The table of two keys is still empty: two enrolled peers get in, a third is refused
    // rather than growing the table.
    for n in 1..=3u8 {
        r.directory
            .pin(certificate(n + 20).fingerprint(), tenant(n + 20));
    }
    let mut statuses = Vec::new();
    for n in 1..=3u8 {
        let t = LoopbackTransport::new(router.clone(), certificate(n + 20));
        let party = tenant(n + 20);
        let (message, _) = proposal(30 + n, &party, &r.w.b.party, n + 40);
        statuses.push(
            t.request(post("/link/v1/handshake", &message))
                .await
                .unwrap()
                .status,
        );
    }
    assert_eq!(statuses, vec![200, 200, 429]);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_port_there_are_no_intake_routes() {
    let w = world();
    let (server, _) = server_for(
        &w.b,
        &w.a.party,
        &certificate(1),
        Arc::new(CarrierTable::new()),
    );
    let t = LoopbackTransport::new(server.router(), certificate(1));
    let (message, _) = proposal(11, &w.a.party, &w.b.party, 1);
    assert_eq!(
        t.request(post("/link/v1/handshake", &message))
            .await
            .unwrap()
            .status,
        404
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_missing_certificate_is_401_and_oversize_bodies_413() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let r = rig(IntakeSettings {
        max_handshake_bytes: 64,
        ..IntakeSettings::default()
    });
    let response = r
        .server
        .router()
        .oneshot(
            Request::post("/link/v1/handshake")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    let t = LoopbackTransport::new(r.server.router(), certificate(1));
    let big = HttpRequest::post("/link/v1/handshake".to_owned(), vec![b' '; 65]);
    assert_eq!(t.request(big).await.unwrap().status, 413);
}

#[test]
fn the_sweep_prunes_on_its_interval_with_time_as_an_argument() {
    let port = Counting::default();
    let sweep = NonceSweep::new(1_000);
    let policy = ConsentPolicy::default();
    assert_eq!(sweep.run_if_due(&port, 5_000, policy).unwrap(), Some(3));
    assert_eq!(sweep.run_if_due(&port, 5_999, policy).unwrap(), None);
    assert_eq!(sweep.run_if_due(&port, 6_000, policy).unwrap(), Some(3));
    // A clock that went backwards prunes again rather than stalling.
    assert_eq!(sweep.run_if_due(&port, 100, policy).unwrap(), Some(3));
    assert_eq!(*port.pruned.lock().unwrap(), vec![5_000, 6_000, 100]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_limited_request_carries_retry_after() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;
    let r = rig(settings(1, 10));
    let router = r.server.router();
    let send = || {
        let mut request = Request::post("/link/v1/handshake")
            .body(Body::from("{}"))
            .unwrap();
        request.extensions_mut().insert(certificate(1));
        router.clone().oneshot(request)
    };
    assert_eq!(send().await.unwrap().status(), 400);
    let limited = send().await.unwrap();
    assert_eq!(limited.status(), 429);
    assert!(limited.headers().contains_key("retry-after"));
}
