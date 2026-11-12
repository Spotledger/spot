//! Publish, ownership, size caps, authentication and the security refusals, over the router with
//! memory doubles and a real filesystem blob store.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use axum::http::StatusCode;
use common::{Harness, Part, b64, bundle, cert, signature_of, standard_parts, tampered_signature};
use serde_json::json;
use spot_store_server::{ct_compare_count, hash_key};
use spot_store_state::ReviewStatus;

#[test]
fn publish_answers_202_with_a_review_id_and_the_version_is_not_public() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let bytes = bundle("invoice_reminders", "1.0.0", "Invoice reminders");
    let reply = harness.publish(&token, "invoice_reminders", &bytes, 1);
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{:?}", reply.json());
    assert!(reply.header("x-request-id").is_some());
    let review_id = reply.data()["review_id"].as_str().unwrap().to_owned();
    assert!(!review_id.is_empty());

    // Fail closed: under review, the version is exactly as unknown as one that never existed.
    let anonymous = harness.get("/store/v1/apps/invoice_reminders/versions/1.0.0");
    assert_eq!(anonymous.status, StatusCode::NOT_FOUND);
    assert_eq!(anonymous.code(), "store.version_not_found");
    let unknown = harness.get("/store/v1/apps/invoice_reminders/versions/9.9.9");
    assert_eq!(unknown.code(), anonymous.code());
    assert_eq!(
        harness.get("/store/v1/apps/invoice_reminders").code(),
        "store.app_not_found"
    );
    let search = harness.get("/store/v1/apps");
    assert_eq!(search.status, StatusCode::OK);
    assert_eq!(search.data().as_array().unwrap().len(), 0);

    // The owner sees it, and nobody else.
    let owner = harness.get_with(
        "/store/v1/apps/invoice_reminders/versions/1.0.0",
        Some(&token),
        &[],
    );
    assert_eq!(owner.status, StatusCode::OK, "{:?}", owner.json());
    let other = harness.issue_key(2);
    let stranger = harness.get_with(
        "/store/v1/apps/invoice_reminders/versions/1.0.0",
        Some(&other),
        &[],
    );
    assert_eq!(stranger.code(), "store.version_not_found");
}

#[test]
fn a_review_runs_and_is_visible_to_its_publisher_only() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let review_id = harness.publish_ok(&token, "reviewed_app", "1.0.0", 1);
    let record = harness.wait_review(&review_id);
    assert!(matches!(
        record.status,
        ReviewStatus::Done | ReviewStatus::Failed
    ));

    let path = format!("/store/v1/reviews/{review_id}");
    let mine = harness.get_with(&path, Some(&token), &[]);
    assert_eq!(mine.status, StatusCode::OK, "{:?}", mine.json());
    let data = mine.data();
    assert_eq!(data["id"], review_id.as_str());
    assert_eq!(data["app"], "reviewed_app");
    let verdict = data["verdict"].as_str().unwrap();
    assert!(["rejected", "needs_human_review", "looks_clean"].contains(&verdict));
    assert!(data["findings"]["info"].is_u64());
    assert!(data["findings"]["warning"].is_u64());
    assert!(data["findings"]["blocker"].is_u64());
    // The review never says an app is safe, approved, trusted or verified.
    let text = String::from_utf8(mine.body.clone()).unwrap().to_lowercase();
    for word in ["\"safe\"", "approved", "trusted", "verified"] {
        assert!(!text.contains(word), "{word} in {text}");
    }

    let other = harness.issue_key(2);
    let theirs = harness.get_with(&path, Some(&other), &[]);
    assert_eq!(theirs.status, StatusCode::NOT_FOUND);
    assert_eq!(theirs.code(), "store.review_not_found");
    let unknown = harness.get_with("/store/v1/reviews/nope", Some(&token), &[]);
    assert_eq!(unknown.code(), "store.review_not_found");
    let anonymous = harness.get(&path);
    assert_eq!(anonymous.status, StatusCode::UNAUTHORIZED);
}

#[test]
fn an_unsigned_or_wrongly_signed_upload_is_refused_and_leaves_no_blob() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let bytes = bundle("signed_app", "1.0.0", "Signed");

    // A signature that parses and does not verify.
    let parts = standard_parts(&bytes, &tampered_signature(&bytes, 1), 1);
    let reply = harness.send(harness.publish_request(Some(&token), "signed_app", &parts, &[]));
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.code(), "store.signature_invalid");

    // A signature over another version of the same app.
    let other_version = bundle("signed_app", "2.0.0", "Signed");
    let parts = standard_parts(&bytes, &signature_of(&other_version, 1), 1);
    let reply = harness.send(harness.publish_request(Some(&token), "signed_app", &parts, &[]));
    assert_eq!(reply.code(), "store.signature_invalid");

    // No signature part at all.
    let parts: Vec<Part> = standard_parts(&bytes, &signature_of(&bytes, 1), 1)
        .into_iter()
        .filter(|part| part.name != "signature")
        .collect();
    let reply = harness.send(harness.publish_request(Some(&token), "signed_app", &parts, &[]));
    assert_eq!(reply.status, StatusCode::BAD_REQUEST);
    assert_eq!(reply.code(), "store.invalid_request");

    // A signature that is not base64.
    let mut parts = standard_parts(&bytes, &signature_of(&bytes, 1), 1);
    parts[1].data = b"!!! not base64 !!!".to_vec();
    let reply = harness.send(harness.publish_request(Some(&token), "signed_app", &parts, &[]));
    assert_eq!(reply.code(), "store.invalid_request");

    assert!(
        Harness::names_in(&harness.blob_dir()).is_empty(),
        "a refused upload leaves no blob: {:?}",
        Harness::names_in(&harness.blob_dir())
    );
    assert!(
        Harness::names_in(&harness.temp_dir()).is_empty(),
        "and no temporary file"
    );
    let audit = harness.audit();
    assert!(
        audit
            .iter()
            .any(|(kind, reason)| kind == "publish" && reason.starts_with("refused: store."))
    );
}

#[test]
fn a_name_owned_by_another_publisher_is_name_taken() {
    let harness = Harness::new();
    let first = harness.issue_key(1);
    let second = harness.issue_key(2);
    harness.publish_ok(&first, "shared_name", "1.0.0", 1);
    let bytes = bundle("shared_name", "5.0.0", "Mine now");
    let reply = harness.publish(&second, "shared_name", &bytes, 2);
    assert_eq!(reply.status, StatusCode::CONFLICT, "{:?}", reply.json());
    assert_eq!(reply.code(), "store.name_taken");
}

#[test]
fn a_lower_or_equal_version_is_not_monotonic() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    harness.publish_ok(&token, "versioned", "2.0.0", 1);
    for version in ["1.0.0", "2.0.0-rc.1"] {
        let bytes = bundle("versioned", version, "Versioned");
        let reply = harness.publish(&token, "versioned", &bytes, 1);
        assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(reply.code(), "store.version_not_monotonic", "{version}");
    }
    // Same version, different bytes: also refused.
    let changed = bundle("versioned", "2.0.0", "A different title");
    let reply = harness.publish(&token, "versioned", &changed, 1);
    assert_eq!(reply.code(), "store.version_not_monotonic");
}

#[test]
fn a_key_cannot_publish_for_another_publisher() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let bytes = bundle("stolen", "1.0.0", "Stolen");
    // Publisher 1's key, publisher 2's own signature and certificate.
    let parts = standard_parts(&bytes, &signature_of(&bytes, 2), 2);
    let reply = harness.send(harness.publish_request(Some(&token), "stolen", &parts, &[]));
    assert_eq!(reply.status, StatusCode::FORBIDDEN);
    assert_eq!(reply.code(), "store.forbidden");
    assert!(Harness::names_in(&harness.blob_dir()).is_empty());
}

#[test]
fn the_name_in_the_path_must_match_the_manifest() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let bytes = bundle("real_name", "1.0.0", "Real");
    let reply = harness.publish(&token, "other_name", &bytes, 1);
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.code(), "store.invalid_manifest");
    assert!(Harness::names_in(&harness.blob_dir()).is_empty());
}

#[test]
fn a_garbage_archive_is_a_bundle_error() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let junk = b"this is not a zip archive".to_vec();
    let parts = standard_parts(
        &junk,
        &tampered_signature(&bundle("junk_app", "1.0.0", "J"), 1),
        1,
    );
    let reply = harness.send(harness.publish_request(Some(&token), "junk_app", &parts, &[]));
    assert_eq!(reply.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(reply.code(), "store.bundle_invalid");
    assert_eq!(
        reply.json()["error"]["details"]["bundle_code"],
        "bundle.bad_archive"
    );
}

#[test]
fn path_like_and_malformed_names_are_refused_before_anything_is_stored() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let bytes = bundle("fine_name", "1.0.0", "Fine");
    let parts = standard_parts(&bytes, &signature_of(&bytes, 1), 1);
    for name in ["..%2Fx", "Upper", "a%2Fb", "9starts_with_digit", "has-dash"] {
        let reply = harness.send(harness.publish_request(Some(&token), name, &parts, &[]));
        assert_eq!(reply.status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(reply.code(), "store.invalid_request", "{name}");
        let read = harness.get(&format!("/store/v1/apps/{name}"));
        assert_eq!(read.status, StatusCode::BAD_REQUEST, "{name}");
    }
    let too_long = "a".repeat(64);
    let reply = harness.send(harness.publish_request(Some(&token), &too_long, &parts, &[]));
    assert_eq!(reply.code(), "store.invalid_request");
    assert!(Harness::names_in(&harness.blob_dir()).is_empty());
    assert!(Harness::names_in(&harness.temp_dir()).is_empty());
}

#[test]
fn an_oversized_bundle_is_cut_off_and_leaves_nothing() {
    let harness = Harness::with(|config| config.limits.max_bundle_bytes = 4096);
    let token = harness.issue_key(1);
    let bytes = bundle("big_app", "1.0.0", "Big");
    let mut parts = standard_parts(&bytes, &signature_of(&bytes, 1), 1);
    parts[0].data = vec![7_u8; 100 * 1024];
    let reply = harness.send(harness.publish_request(Some(&token), "big_app", &parts, &[]));
    assert_eq!(
        reply.status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "{:?}",
        reply.body
    );
    assert_eq!(reply.code(), "store.too_large");
    assert!(Harness::names_in(&harness.temp_dir()).is_empty());
    assert!(Harness::names_in(&harness.blob_dir()).is_empty());

    // A declared length far above the cap is refused before the body is read.
    let request = harness.publish_request(
        Some(&token),
        "big_app",
        &standard_parts(&bytes, &signature_of(&bytes, 1), 1),
        &[("content-length", "50000000")],
    );
    let reply = harness.send(request);
    assert_eq!(reply.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(reply.code(), "store.too_large");
}

#[test]
fn parts_the_contract_does_not_allow_are_refused() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let bytes = bundle("parts_app", "1.0.0", "Parts");
    let good = || standard_parts(&bytes, &signature_of(&bytes, 1), 1);
    let send = |parts: Vec<Part>| {
        harness.send(harness.publish_request(Some(&token), "parts_app", &parts, &[]))
    };

    // Two bundle parts.
    let mut parts = good();
    parts.push(Part {
        name: "bundle",
        content_type: None,
        data: bytes.clone(),
    });
    assert_eq!(send(parts).code(), "store.invalid_request");
    // A part the contract does not define.
    let mut parts = good();
    parts.push(Part {
        name: "extra",
        content_type: None,
        data: b"x".to_vec(),
    });
    assert_eq!(send(parts).code(), "store.invalid_request");
    // A certificate that is not text/plain.
    let mut parts = good();
    parts[2].content_type = Some("application/json");
    assert_eq!(send(parts).code(), "store.invalid_request");
    // More than eight certificates.
    let mut parts = good();
    for _ in 0..8 {
        parts.push(Part {
            name: "certificate_chain",
            content_type: Some("text/plain"),
            data: b64(cert(1).to_json().as_bytes()).into_bytes(),
        });
    }
    assert_eq!(send(parts).code(), "store.invalid_request");
    // Two certificates are inside the contract's count but this store takes a chain of one.
    let mut parts = good();
    parts.push(Part {
        name: "certificate_chain",
        content_type: Some("text/plain"),
        data: b64(cert(1).to_json().as_bytes()).into_bytes(),
    });
    assert_eq!(send(parts).code(), "store.invalid_request");
    // No bundle.
    let parts: Vec<Part> = good().into_iter().filter(|p| p.name != "bundle").collect();
    assert_eq!(send(parts).code(), "store.invalid_request");
    // An empty bundle part.
    let mut parts = good();
    parts[0].data = Vec::new();
    assert_eq!(send(parts).code(), "store.invalid_request");
    // Not multipart at all.
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/store/v1/apps/parts_app/versions")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .body(axum::body::Body::from("{}"))
        .unwrap();
    assert_eq!(harness.send(request).code(), "store.invalid_request");
    assert!(Harness::names_in(&harness.blob_dir()).is_empty());
    assert!(Harness::names_in(&harness.temp_dir()).is_empty());
    // And a correct upload still works afterwards.
    assert_eq!(send(good()).status, StatusCode::ACCEPTED);
}

#[test]
fn credentials_are_checked_in_constant_time_and_all_failures_look_alike() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let bytes = bundle("auth_app", "1.0.0", "Auth");
    let parts = standard_parts(&bytes, &signature_of(&bytes, 1), 1);
    let before = ct_compare_count();

    let unknown = format!("spk_{}", "0".repeat(64));
    let missing = harness.send(harness.publish_request(None, "auth_app", &parts, &[]));
    let wrong = harness.send(harness.publish_request(Some(&unknown), "auth_app", &parts, &[]));
    let malformed =
        harness.send(harness.publish_request(Some("not-a-key"), "auth_app", &parts, &[]));
    // Revoke the only key of publisher 1, then use it.
    let keys = harness.state.list_publisher_keys("publisher1").unwrap();
    harness.admin_post(
        &format!("/admin/v1/publisher-keys/{}/revoke", keys[0].key_id),
        None,
    );
    let revoked = harness.send(harness.publish_request(Some(&token), "auth_app", &parts, &[]));
    for reply in [&missing, &wrong, &malformed, &revoked] {
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
        assert_eq!(reply.code(), "store.unauthorized");
        assert_eq!(
            reply.json()["error"]["message"],
            missing.json()["error"]["message"]
        );
    }
    // The unknown key and the revoked key both went through the constant-time comparison.
    assert!(ct_compare_count() >= before + 2);
    // Only the hash is stored.
    assert!(
        harness
            .state
            .lookup_publisher_key(&hash_key(&token))
            .unwrap()
            .is_some()
    );
}

#[test]
fn an_idempotency_key_replays_the_same_review_and_refuses_a_different_upload() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let bytes = bundle("idem_app", "1.0.0", "Idem");
    let parts = standard_parts(&bytes, &signature_of(&bytes, 1), 1);
    let send = |parts: &[Part], key: &str| {
        harness.send(harness.publish_request(
            Some(&token),
            "idem_app",
            parts,
            &[("idempotency-key", key)],
        ))
    };
    let first = send(&parts, "abc-123");
    assert_eq!(first.status, StatusCode::ACCEPTED, "{:?}", first.json());
    assert!(first.header("idempotency-replayed").is_none());
    let again = send(&parts, "abc-123");
    assert_eq!(again.status, StatusCode::ACCEPTED);
    assert_eq!(
        again.header("idempotency-replayed").as_deref(),
        Some("true")
    );
    assert_eq!(again.data(), first.data());

    let other = bundle("idem_app", "1.1.0", "Idem two");
    let other_parts = standard_parts(&other, &signature_of(&other, 1), 1);
    let conflict = send(&other_parts, "abc-123");
    assert_eq!(conflict.status, StatusCode::BAD_REQUEST);
    assert_eq!(conflict.code(), "store.invalid_request");

    // A key with a space is not a key.
    let bad = send(&parts, "has space");
    assert_eq!(bad.code(), "store.invalid_request");

    // The same bytes without a key are accepted again as the same review (no second review).
    let repeat = harness.publish(&token, "idem_app", &bytes, 1);
    assert_eq!(repeat.status, StatusCode::ACCEPTED);
    assert_eq!(repeat.data(), first.data());
}

#[test]
fn concurrent_publishes_of_the_same_version_have_exactly_one_winner() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let requests: Vec<_> = (0..6)
        .map(|n| {
            let bytes = bundle("racing_app", "1.0.0", &format!("variant {n}"));
            let parts = standard_parts(&bytes, &signature_of(&bytes, 1), 1);
            harness.publish_request(Some(&token), "racing_app", &parts, &[])
        })
        .collect();
    let replies: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = requests
            .into_iter()
            .map(|request| scope.spawn(|| harness.send(request)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let accepted = replies
        .iter()
        .filter(|reply| reply.status == StatusCode::ACCEPTED)
        .count();
    assert_eq!(accepted, 1, "exactly one wins");
    for reply in replies.iter().filter(|r| r.status != StatusCode::ACCEPTED) {
        assert_eq!(reply.code(), "store.version_not_monotonic");
    }
    // Exactly one release was committed. (Losers that passed the registry's preflight before the
    // winner committed may leave an orphan blob, which the registry documents and repairs.)
    assert_eq!(harness.index.digests().unwrap().len(), 1);
}

#[test]
fn publish_and_authentication_failures_are_rate_limited() {
    let harness = Harness::with(|config| {
        config.rates.publish_per_key.max = 2;
        config.rates.auth_failures_per_address.max = 3;
    });
    let token = harness.issue_key(1);
    let bytes = bundle("limited_app", "1.0.0", "Limited");
    let parts = standard_parts(&bytes, &signature_of(&bytes, 1), 1);
    let statuses: Vec<StatusCode> = (0..3)
        .map(|_| {
            harness
                .send(harness.publish_request(Some(&token), "limited_app", &parts, &[]))
                .status
        })
        .collect();
    assert_eq!(statuses[0], StatusCode::ACCEPTED);
    assert_eq!(statuses[2], StatusCode::TOO_MANY_REQUESTS);
    let limited = harness.send(harness.publish_request(Some(&token), "limited_app", &parts, &[]));
    assert_eq!(limited.code(), "store.rate_limited");
    assert!(limited.header("retry-after").is_some());

    // Guessing keys is stopped per source address.
    let guess = format!("spk_{}", "1".repeat(64));
    for _ in 0..3 {
        let reply = harness.send(harness.publish_request(Some(&guess), "limited_app", &parts, &[]));
        assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
    }
    let blocked = harness.send(harness.publish_request(Some(&guess), "limited_app", &parts, &[]));
    assert_eq!(blocked.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(blocked.code(), "store.rate_limited");
}

#[test]
fn admin_routes_need_the_operator_credential() {
    let harness = Harness::new();
    let token = harness.issue_key(1);
    let path = "/admin/v1/publishers/publisher1/keys";
    let body = Some(json!({ "label": "x" }));
    let none = harness.post_json(path, None, body.clone(), &[]);
    assert_eq!(none.status, StatusCode::UNAUTHORIZED);
    assert_eq!(none.code(), "store_server.admin_unauthorized");
    let publisher_key_as_admin = harness.post_json(path, Some(&token), body.clone(), &[]);
    assert_eq!(
        publisher_key_as_admin.code(),
        "store_server.admin_unauthorized"
    );
    let wrong = harness.post_json(
        path,
        Some("operator-secret-wrong-wrong-wrong-wrong-1"),
        body,
        &[],
    );
    assert_eq!(wrong.code(), "store_server.admin_unauthorized");
    assert_eq!(harness.get("/metrics").status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        harness.get("/admin/v1/audit").status,
        StatusCode::UNAUTHORIZED
    );
    // The key a publisher holds is never an operator credential, and the audit never prints one.
    let audit = serde_json::to_string(&harness.admin_get("/admin/v1/audit").json()).unwrap();
    assert!(!audit.contains(&token));
}
