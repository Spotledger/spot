//! The read side and the operator side: search, metadata, download (`ETag`, ranges), yank, the
//! signed revocation list, revocations taking effect, the listing states, paid apps, health.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use axum::http::StatusCode;
use base64::Engine as _;
use common::{Harness, NOW, bundle, open, publisher_key, root_key};
use serde_json::json;
use spot_bundle_signing::{
    BundleFacts, BundleSignature, CertificateChain, FreshRevocationList, RevocationBody,
    SignedCertificate, TrustAnchor, TrustedRoot, VerifyMode, VerifyPolicy, issue_revocation_list,
    verify, verify_fresh_revocation_list,
};
use spot_license::Kind;
use spot_store_commerce::{AppLicence, LicenceIssuer};
use spot_store_server::StoreKey;

fn published_and_listed(harness: &Harness, token: &str, name: &str, version: &str) {
    harness.publish_ok(token, name, version, 1);
    let reply = harness.approve(name, version);
    assert_eq!(reply.status, StatusCode::OK, "{:?}", reply.json());
}

fn decode(header: &str) -> Vec<u8> {
    base64::engine::general_purpose::STANDARD
        .decode(header)
        .unwrap()
}

fn store_anchor() -> TrustAnchor {
    TrustAnchor::new(vec![TrustedRoot::new(
        StoreKey::from_seed(&[3; 32]).public(),
        0,
        None,
    )])
}

/// The list inside `{"data": ...}`, parsed by the crate's own strict wire codec.
fn parse_list(reply: &common::Reply) -> FreshRevocationList {
    assert!(reply.body.starts_with(b"{\"data\":"));
    assert!(reply.body.ends_with(b"}"));
    FreshRevocationList::from_wire_json(&reply.body[8..reply.body.len() - 1]).unwrap()
}

#[test]
fn publish_to_install_round_trip_verifies_with_the_site_side_check() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    harness.publish_ok(&token, "round_trip", "1.2.3", 1);
    let approved = harness.approve("round_trip", "1.2.3");
    assert_eq!(approved.data()["state"], "listed");

    let search = harness.get("/store/v1/apps");
    assert_eq!(search.status, StatusCode::OK);
    let listing = search.data()[0].clone();
    assert_eq!(listing["name"], "round_trip");
    assert_eq!(listing["title"], "round_trip");
    assert_eq!(listing["publisher"], "publisher1");
    assert_eq!(listing["latest_version"], "1.2.3");
    assert_eq!(listing["published_at"], "1970-01-12T13:46:40Z");
    assert!(search.json()["next_cursor"].is_null());
    let app = harness.get("/store/v1/apps/round_trip");
    assert_eq!(app.status, StatusCode::OK);
    assert_eq!(app.data(), listing);

    let version = harness.get("/store/v1/apps/round_trip/versions/1.2.3");
    assert_eq!(version.status, StatusCode::OK);
    let detail = version.data();
    assert_eq!(detail["yanked"], false);
    assert!(detail["yanked_reason"].is_null());
    assert!(detail["signature_id"].as_str().unwrap().len() >= 16);

    let download = harness.get("/store/v1/apps/round_trip/versions/1.2.3/bundle");
    assert_eq!(download.status, StatusCode::OK);
    let digest = detail["digest"].as_str().unwrap();
    assert_eq!(download.header("etag").unwrap(), format!("\"{digest}\""));
    assert_eq!(download.header("accept-ranges").as_deref(), Some("bytes"));
    assert_eq!(
        download.header("content-type").as_deref(),
        Some("application/octet-stream")
    );
    assert_eq!(detail["size"].as_u64().unwrap(), download.body.len() as u64);

    // The site never trusts the store: it verifies the signature and the digest itself.
    let opened = open(&download.body);
    assert_eq!(opened.digest().to_hex(), digest);
    let signature =
        BundleSignature::from_json(&decode(&download.header("x-spot-signature").unwrap())).unwrap();
    let certificate =
        SignedCertificate::from_json(&decode(&download.header("x-spot-certificate").unwrap()))
            .unwrap();
    let anchor = TrustAnchor::new(vec![TrustedRoot::new(root_key().public(), 0, None)]);
    let list = issue_revocation_list(
        RevocationBody {
            generation: 1,
            issued_at: NOW,
            issued_by: root_key().public().key_id(),
            keys: vec![],
            publishers: vec![],
            bundles: vec![],
        },
        &root_key(),
    );
    let verified = verify(
        &BundleFacts {
            digest: &opened.digest(),
            app_id: "round_trip",
            version: "1.2.3",
        },
        &signature,
        &CertificateChain::new(certificate),
        &anchor,
        &list,
        NOW,
        &VerifyPolicy {
            mode: VerifyMode::Install,
            allow_expired_certificate_at_startup: false,
            held_revocation_generation: 0,
        },
    )
    .unwrap();
    assert_eq!(verified.publisher_id, "publisher1");
    // Neither header claims the download is verified.
    let headers = format!("{:?}", download.headers).to_lowercase();
    assert!(!headers.contains("verified") && !headers.contains("safe"));
}

#[test]
fn downloads_support_etag_and_single_ranges() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    published_and_listed(&harness, &token, "ranged_app", "1.0.0");
    let path = "/store/v1/apps/ranged_app/versions/1.0.0/bundle";
    let full = harness.get(path);
    let size = full.body.len();
    let etag = full.header("etag").unwrap();

    let head = harness.get_with(path, None, &[("range", "bytes=0-9")]);
    assert_eq!(head.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(head.body, full.body[..10]);
    assert_eq!(
        head.header("content-range").unwrap(),
        format!("bytes 0-9/{size}")
    );
    assert_eq!(head.header("etag").unwrap(), etag);
    let tail = harness.get_with(path, None, &[("range", "bytes=-5")]);
    assert_eq!(tail.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(tail.body, full.body[size - 5..]);
    let from_end = format!("bytes={}-", size - 3);
    let open_ended = harness.get_with(path, None, &[("range", from_end.as_str())]);
    assert_eq!(open_ended.body, full.body[size - 3..]);

    for bad in [
        format!("bytes={size}-"),
        "bytes=5-2".to_owned(),
        "bytes=0-1,3-4".to_owned(),
    ] {
        let reply = harness.get_with(path, None, &[("range", bad.as_str())]);
        assert_eq!(reply.status, StatusCode::RANGE_NOT_SATISFIABLE, "{bad}");
        assert_eq!(reply.code(), "store.invalid_request");
        assert_eq!(
            reply.header("content-range").unwrap(),
            format!("bytes */{size}")
        );
    }
    // A unit the store does not know is ignored.
    assert_eq!(
        harness
            .get_with(path, None, &[("range", "items=1-2")])
            .status,
        StatusCode::OK
    );

    let same = harness.get_with(path, None, &[("if-none-match", etag.as_str())]);
    assert_eq!(same.status, StatusCode::NOT_MODIFIED);
    assert!(same.body.is_empty());
    assert_eq!(same.header("etag").unwrap(), etag);
    let other = harness.get_with(path, None, &[("if-none-match", "\"deadbeef\"")]);
    assert_eq!(other.status, StatusCode::OK);
}

#[test]
fn yank_hides_a_version_from_search_but_not_from_its_digest_download() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let other = harness.issue_key(2);
    published_and_listed(&harness, &token, "yank_app", "1.0.0");
    let path = "/store/v1/apps/yank_app/versions/1.0.0/yank";
    let reason = json!({ "reason": "A credential leaked in a log line." });

    assert_eq!(
        harness
            .post_json(path, None, Some(reason.clone()), &[])
            .status,
        StatusCode::UNAUTHORIZED
    );
    // Another publisher's key: the same 404 as an unknown version.
    let stranger = harness.post_json(path, Some(&other), Some(reason.clone()), &[]);
    assert_eq!(stranger.status, StatusCode::NOT_FOUND);
    assert_eq!(stranger.code(), "store.version_not_found");
    let unknown = harness.post_json(
        "/store/v1/apps/yank_app/versions/9.9.9/yank",
        Some(&token),
        Some(reason.clone()),
        &[],
    );
    assert_eq!(unknown.code(), "store.version_not_found");
    for body in [
        json!({ "reason": "" }),
        json!({ "reason": "x", "extra": 1 }),
    ] {
        let reply = harness.post_json(path, Some(&token), Some(body), &[]);
        assert_eq!(reply.code(), "store.invalid_request");
    }

    let yanked = harness.post_json(path, Some(&token), Some(reason.clone()), &[]);
    assert_eq!(yanked.status, StatusCode::OK, "{:?}", yanked.json());
    assert_eq!(yanked.data()["yanked"], true);
    assert_eq!(
        yanked.data()["yanked_reason"],
        "A credential leaked in a log line."
    );
    let again = harness.post_json(path, Some(&token), Some(reason), &[]);
    assert_eq!(again.status, StatusCode::CONFLICT);
    assert_eq!(again.code(), "store.already_yanked");

    // Not offered by default, reported as yanked, still downloadable by its digest.
    assert_eq!(
        harness
            .get("/store/v1/apps")
            .data()
            .as_array()
            .unwrap()
            .len(),
        0
    );
    let version = harness.get("/store/v1/apps/yank_app/versions/1.0.0");
    assert_eq!(version.data()["yanked"], true);
    let download = harness.get("/store/v1/apps/yank_app/versions/1.0.0/bundle");
    assert_eq!(download.status, StatusCode::OK);
    assert!(
        harness
            .audit()
            .iter()
            .any(|(kind, reason)| kind == "yank" && reason.starts_with("A credential"))
    );
}

#[test]
fn the_revocation_list_is_signed_cacheable_and_the_same_for_every_caller() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let first = harness.get("/store/v1/revocations");
    assert_eq!(first.status, StatusCode::OK);
    assert_eq!(first.header("etag").unwrap(), "\"1\"");
    assert_eq!(first.header("cache-control").unwrap(), "max-age=300");
    let list = parse_list(&first);
    verify_fresh_revocation_list(&list, &store_anchor(), NOW, None).unwrap();
    assert_eq!(list.body.generation, 1);
    assert_eq!(list.body.next_update, NOW + 86_400, "a 24 hour lifetime");
    assert_eq!(list.body.issued_by, *StoreKey::from_seed(&[3; 32]).id());

    // Served identically with or without a credential.
    let with_key = harness.get_with("/store/v1/revocations", Some(&token), &[]);
    assert_eq!(with_key.body, first.body);
    // Conditional request.
    let cached = harness.get_with("/store/v1/revocations", None, &[("if-none-match", "\"1\"")]);
    assert_eq!(cached.status, StatusCode::NOT_MODIFIED);
    assert!(cached.body.is_empty());

    // An entry changes: the list is reissued before the route answers, and the ETag moves.
    let digest = "ab".repeat(32);
    let revoked = harness.admin_post(
        &format!("/admin/v1/bundles/{digest}/revoke"),
        Some(json!({ "reason": "malware" })),
    );
    assert_eq!(revoked.status, StatusCode::OK, "{:?}", revoked.json());
    assert_eq!(revoked.data()["generation"], 2);
    let second = harness.get("/store/v1/revocations");
    assert_eq!(second.header("etag").unwrap(), "\"2\"");
    let list = parse_list(&second);
    verify_fresh_revocation_list(&list, &store_anchor(), NOW, Some(1)).unwrap();
    assert_eq!(list.body.bundles.len(), 1);
    assert_eq!(list.body.bundles[0].digest_hex, digest);
    assert_eq!(list.body.bundles[0].reason, "malware");
    // The old ETag no longer matches.
    let stale = harness.get_with("/store/v1/revocations", None, &[("if-none-match", "\"1\"")]);
    assert_eq!(stale.status, StatusCode::OK);

    // Bad operator input is refused and revokes nothing.
    let bad = harness.admin_post(
        "/admin/v1/bundles/not-a-digest/revoke",
        Some(json!({ "reason": "x" })),
    );
    assert_eq!(bad.status, StatusCode::BAD_REQUEST);
    assert_eq!(harness.service.revocation_generation(), 2);
}

#[test]
fn a_restart_serves_the_same_list_bytes_and_etag() {
    let first = Harness::new();
    let before = first.get("/store/v1/revocations");
    let second = Harness::on(
        std::sync::Arc::clone(&first.index),
        std::sync::Arc::clone(&first.state),
        |_| {},
    );
    let after = second.get("/store/v1/revocations");
    assert_eq!(after.body, before.body);
    assert_eq!(after.header("etag"), before.header("etag"));
}

#[test]
fn a_revoked_publisher_cannot_publish_and_is_on_the_next_list() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let healthy = harness.issue_key(2);
    published_and_listed(&harness, &token, "revoked_app", "1.0.0");
    let before = parse_list(&harness.get("/store/v1/revocations"))
        .body
        .generation;

    let revoke = harness.admin_post(
        "/admin/v1/publishers/publisher1/revoke",
        Some(json!({ "reason": "abuse" })),
    );
    assert_eq!(revoke.status, StatusCode::OK, "{:?}", revoke.json());
    let list = parse_list(&harness.get("/store/v1/revocations"));
    assert!(list.body.generation > before, "within one request");
    assert_eq!(list.body.publishers.len(), 1);
    assert_eq!(list.body.publishers[0].publisher_id, "publisher1");
    assert_eq!(list.body.publishers[0].reason, "abuse");
    assert_eq!(list.body.publishers[0].effective_from, NOW);

    // The key died with the publisher: the next request is refused.
    let bytes = bundle("revoked_app", "2.0.0", "Two");
    assert_eq!(
        harness.publish(&token, "revoked_app", &bytes, 1).status,
        StatusCode::UNAUTHORIZED
    );
    // A new key for the same publisher id still cannot publish or yank.
    let fresh = harness.issue_key(1);
    let refused = harness.publish(&fresh, "revoked_app", &bytes, 1);
    assert_eq!(refused.status, StatusCode::FORBIDDEN);
    assert_eq!(refused.code(), "store.publisher_revoked");
    let yank = harness.post_json(
        "/store/v1/apps/revoked_app/versions/1.0.0/yank",
        Some(&fresh),
        Some(json!({ "reason": "x" })),
        &[],
    );
    assert_eq!(yank.code(), "store.publisher_revoked");
    // Other publishers are unaffected.
    let other = bundle("healthy_app", "1.0.0", "Healthy");
    assert_eq!(
        harness.publish(&healthy, "healthy_app", &other, 2).status,
        StatusCode::ACCEPTED
    );
    assert!(
        harness
            .audit()
            .iter()
            .any(|(kind, reason)| kind == "revoke_publisher" && reason == "abuse")
    );
}

#[test]
fn a_publisher_who_never_published_can_be_revoked_too() {
    let harness = Harness::new();
    let revoke = harness.admin_post(
        "/admin/v1/publishers/publisher9/revoke",
        Some(json!({ "reason": "takedown" })),
    );
    assert_eq!(revoke.status, StatusCode::OK, "{:?}", revoke.json());
    let token = harness.issue_key(9);
    let bytes = bundle("never_app", "1.0.0", "Never");
    let reply = harness.publish(&token, "never_app", &bytes, 9);
    // The registry's own trust refuses the certificate's publisher.
    assert_eq!(reply.status, StatusCode::FORBIDDEN, "{:?}", reply.json());
    assert_eq!(reply.code(), "store.publisher_revoked");
}

#[test]
fn a_revoked_signing_key_or_bundle_is_refused_and_listed() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let first = bundle("keyed_app", "1.0.0", "keyed_app");
    assert_eq!(
        harness.publish(&token, "keyed_app", &first, 1).status,
        StatusCode::ACCEPTED
    );

    // The bundle is revoked: the same bytes are not accepted again.
    let digest = open(&first).digest().to_hex();
    let revoked = harness.admin_post(
        &format!("/admin/v1/bundles/{digest}/revoke"),
        Some(json!({ "reason": "malware" })),
    );
    assert_eq!(revoked.status, StatusCode::OK);
    let again = harness.publish(&token, "keyed_app", &first, 1);
    assert_eq!(again.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(again.code(), "store.signature_invalid");

    // The signing key is revoked: a new version signed with it is refused.
    let key_id = publisher_key(1).public().key_id();
    let revoked = harness.admin_post(
        &format!("/admin/v1/signing-keys/{}/revoke", key_id.as_str()),
        Some(json!({ "reason": "leaked" })),
    );
    assert_eq!(revoked.status, StatusCode::OK, "{:?}", revoked.json());
    let next = bundle("keyed_app", "2.0.0", "keyed_app");
    let reply = harness.publish(&token, "keyed_app", &next, 1);
    assert_eq!(reply.code(), "store.signature_invalid");
    let list = parse_list(&harness.get("/store/v1/revocations"));
    assert_eq!(list.body.keys.len(), 1);
    assert_eq!(list.body.keys[0].key_id, key_id);
    assert!(list.body.keys[0].compromised);
    assert_eq!(list.body.bundles.len(), 1);
}

#[test]
fn a_paid_apps_bundle_needs_an_entitled_licence() {
    let harness = Harness::with(|config| config.paid_apps = vec!["paid_app".to_owned()]);
    let token = harness.issue_key(1);
    published_and_listed(&harness, &token, "paid_app", "1.0.0");
    published_and_listed(&harness, &token, "free_app", "1.0.0");
    let paid = "/store/v1/apps/paid_app/versions/1.0.0/bundle";
    let free = "/store/v1/apps/free_app/versions/1.0.0/bundle";

    let none = harness.get(paid);
    assert_eq!(none.status, StatusCode::FORBIDDEN);
    assert_eq!(none.code(), "store.not_entitled");
    let garbage = harness.get_with(paid, Some("not-a-licence"), &[]);
    assert_eq!(garbage.code(), "store.not_entitled");
    // A missing version is 404 either way: the licence check reveals nothing.
    let missing = harness.get("/store/v1/apps/paid_app/versions/9.9.9/bundle");
    assert_eq!(missing.code(), "store.version_not_found");

    let key = StoreKey::from_seed(&[3; 32]);
    let issuer = LicenceIssuer::new(&key);
    let mint = |app: &str| {
        let licence = AppLicence::new(
            "lic-1",
            app,
            "acme",
            "pro_once",
            Kind::Perpetual,
            None,
            0,
            1,
            NOW - 100,
        )
        .unwrap();
        issuer.issue(&licence).unwrap().to_text().unwrap()
    };
    let good = harness.get_with(paid, Some(&mint("paid_app")), &[]);
    assert_eq!(
        good.status,
        StatusCode::OK,
        "{:?}",
        String::from_utf8_lossy(&good.body)
    );
    let wrong_app = harness.get_with(paid, Some(&mint("other_app")), &[]);
    assert_eq!(wrong_app.code(), "store.not_entitled");

    // A free app ignores any licence, an invalid one included.
    assert_eq!(harness.get(free).status, StatusCode::OK);
    assert_eq!(
        harness.get_with(free, Some("not-a-licence"), &[]).status,
        StatusCode::OK
    );
    // The owner fetches their own bundle with their publisher key.
    assert_eq!(
        harness.get_with(paid, Some(&token), &[]).status,
        StatusCode::OK
    );
}

#[test]
fn search_keeps_only_listed_versions_and_refills_across_registry_pages() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    for name in ["aaa_app", "bbb_app", "ccc_app", "ddd_app"] {
        harness.publish_ok(&token, name, "1.0.0", 1);
    }
    for name in ["aaa_app", "ccc_app", "ddd_app"] {
        assert_eq!(harness.approve(name, "1.0.0").status, StatusCode::OK);
    }
    let names = |reply: &common::Reply| -> Vec<String> {
        reply
            .data()
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["name"].as_str().unwrap().to_owned())
            .collect()
    };

    let all = harness.get("/store/v1/apps?limit=10");
    assert_eq!(names(&all), ["aaa_app", "ccc_app", "ddd_app"]);
    assert!(all.json()["next_cursor"].is_null());

    // A page of two spans the unlisted bbb_app: it is refilled from the next registry page.
    let first = harness.get("/store/v1/apps?limit=2");
    assert_eq!(names(&first), ["aaa_app", "ccc_app"]);
    let cursor = first.json()["next_cursor"].as_str().unwrap().to_owned();
    let second = harness.get(&format!("/store/v1/apps?limit=2&cursor={cursor}"));
    assert_eq!(second.status, StatusCode::OK, "{:?}", second.json());
    assert_eq!(names(&second), ["ddd_app"]);
    assert!(second.json()["next_cursor"].is_null());

    // A cursor belongs to its query.
    let other_query = harness.get(&format!("/store/v1/apps?limit=3&cursor={cursor}"));
    assert_eq!(other_query.status, StatusCode::BAD_REQUEST);
    assert_eq!(other_query.code(), "store.invalid_request");
    assert_eq!(
        harness.get("/store/v1/apps?cursor=nonsense").status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        harness.get("/store/v1/apps?limit=0").status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        harness.get("/store/v1/apps?limit=101").status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        harness.get("/store/v1/apps?capability=teleport").status,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        harness
            .get("/store/v1/apps?capability=doctypes.read")
            .status,
        StatusCode::OK
    );

    let filtered = harness.get("/store/v1/apps?query=ccc");
    assert_eq!(names(&filtered), ["ccc_app"]);
}

#[test]
fn approval_rejection_and_withdrawal_are_operator_acts_and_audited() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    harness.publish_ok(&token, "state_app", "1.0.0", 1);
    harness.publish_ok(&token, "state_app", "2.0.0", 1);

    assert_eq!(harness.approve("state_app", "1.0.0").status, StatusCode::OK);
    let twice = harness.approve("state_app", "1.0.0");
    assert_eq!(twice.status, StatusCode::CONFLICT);
    assert_eq!(twice.code(), "store_server.bad_state");
    let unknown = harness.approve("state_app", "9.9.9");
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.code(), "store_server.not_found");

    let rejected = harness.admin_post(
        "/admin/v1/apps/state_app/versions/2.0.0/reject",
        Some(json!({ "reason": "asks for too much" })),
    );
    assert_eq!(rejected.data()["state"], "rejected");
    assert_eq!(
        harness.approve("state_app", "2.0.0").code(),
        "store_server.bad_state"
    );
    assert_eq!(
        harness
            .get("/store/v1/apps/state_app/versions/2.0.0")
            .code(),
        "store.version_not_found"
    );

    let listed = harness.get("/store/v1/apps/state_app");
    assert_eq!(
        listed.data()["latest_version"],
        "1.0.0",
        "the listed version stays"
    );
    let withdrawn = harness.admin_post("/admin/v1/apps/state_app/versions/1.0.0/withdraw", None);
    assert_eq!(withdrawn.data()["state"], "withdrawn");
    assert_eq!(
        harness
            .get("/store/v1/apps")
            .data()
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        harness.get("/store/v1/apps/state_app").code(),
        "store.app_not_found"
    );
    assert_eq!(
        harness
            .get("/store/v1/apps/state_app/versions/1.0.0/bundle")
            .code(),
        "store.version_not_found"
    );

    let audit = harness.audit();
    let kinds: Vec<&str> = audit.iter().map(|(kind, _)| kind.as_str()).collect();
    assert_eq!(kinds.iter().filter(|kind| **kind == "publish").count(), 2);
    for kind in ["approve", "reject", "withdraw", "key_issue"] {
        assert!(kinds.contains(&kind), "{kind} in {kinds:?}");
    }
    assert!(
        audit
            .iter()
            .any(|(kind, reason)| kind == "approve" && reason == "refused: store_server.bad_state")
    );
    assert!(
        audit
            .iter()
            .any(|(kind, reason)| kind == "reject" && reason == "asks for too much")
    );
}

#[test]
fn health_turns_unhealthy_when_the_revocation_list_goes_stale_and_recovers() {
    let harness = Harness::new();
    let health = harness.get("/health");
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(health.json()["status"], "ok");

    // No refresh ran for a whole lifetime: the served list is stale, and the service says so.
    harness.clock.advance(86_400 + 1);
    let stale = harness.get("/health");
    assert_eq!(stale.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(stale.json()["revocation_list_fresh"], false);
    // The maintenance round reissues it with a higher generation.
    harness.service.maintain_once();
    assert_eq!(harness.get("/health").status, StatusCode::OK);
    assert!(harness.service.revocation_generation() >= 2);

    let metrics = harness.admin_get("/metrics");
    assert_eq!(metrics.status, StatusCode::OK);
    let text = String::from_utf8(metrics.body).unwrap();
    assert!(text.contains("spot_store_requests_2xx_total"));
    assert!(text.contains("spot_store_revocation_generation"));
}

#[test]
fn unknown_routes_and_methods_answer_the_error_envelope_with_a_request_id() {
    let harness = Harness::new();
    let missing = harness.get("/nowhere");
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.code(), "store_server.not_found");
    let id = missing.header("x-request-id").unwrap();
    assert_eq!(missing.json()["error"]["request_id"], id.as_str());
    assert_eq!(missing.json()["error"]["retryable"], false);

    let wrong = harness.post_json("/health", None, None, &[]);
    assert_eq!(wrong.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(wrong.code(), "store_server.method_not_allowed");
    // Every response carries a request id, successes included, and each one is new.
    let ok = harness.get("/health");
    assert_ne!(ok.header("x-request-id").unwrap(), id);
}
