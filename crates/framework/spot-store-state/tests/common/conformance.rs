//! The shared cases: one function, run against the memory double here and against PostgreSQL in
//! `spot-store-state-pg`, so the two are held to one contract. Every case uses names of its own,
//! so the cases can run in order on one store.
#![allow(
    dead_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic
)]

use spot_registry::{AppName, Digest, Version};
use spot_store_state::*;

pub fn app(s: &str) -> AppName {
    s.parse().unwrap()
}

pub fn version(s: &str) -> Version {
    s.parse().unwrap()
}

pub fn digest(n: u64) -> Digest {
    format!("{n:064x}").parse().unwrap()
}

pub fn hash(n: u8) -> KeyHash {
    KeyHash::from_bytes([n; 32])
}

pub fn assert_code<T: std::fmt::Debug>(result: Result<T>, code: &str) {
    match result {
        Err(e) => assert_eq!(e.code, code, "{e}"),
        Ok(v) => panic!("expected {code}, got success: {v:?}"),
    }
}

pub fn review(id: &str, app_name: &str, ver: &str, at: i64) -> ReviewRecord {
    ReviewRecord {
        review_id: id.to_owned(),
        publisher_id: "publisher1".to_owned(),
        app: app(app_name),
        version: version(ver),
        digest: digest(7),
        status: ReviewStatus::Queued,
        verdict: None,
        summary: None,
        error_code: None,
        created_at: at,
        updated_at: at,
    }
}

type Case = (&'static str, fn(&dyn StoreStateStore));

pub fn cases() -> Vec<Case> {
    vec![
        ("publisher keys", publisher_keys),
        ("reviews", reviews),
        ("the listing transition table", listing_table),
        ("the listing fails closed", listing_fails_closed),
        ("idempotency", idempotency),
        ("revoked keys and bundles", revocations),
        ("the generation and the issued list", generation_and_list),
        ("the audit log", audit),
    ]
}

pub fn run_all(store: &dyn StoreStateStore) {
    for (name, case) in cases() {
        println!("conformance: {name}");
        case(store);
    }
}

fn publisher_keys(s: &dyn StoreStateStore) {
    assert_eq!(s.lookup_publisher_key(&hash(1)).unwrap(), None);
    let first = s
        .issue_publisher_key("pk_pub", hash(1), "first", 10)
        .unwrap();
    let second = s
        .issue_publisher_key("pk_pub", hash(2), "rotated", 20)
        .unwrap();
    let other = s.issue_publisher_key("pk_other", hash(3), "x", 30).unwrap();
    assert!(first < second && second < other);
    assert_code(
        s.issue_publisher_key("pk_other", hash(1), "dup", 40),
        DUPLICATE_KEY_HASH,
    );
    let found = s.lookup_publisher_key(&hash(2)).unwrap().unwrap();
    assert_eq!(
        found,
        KeyRecord {
            publisher_id: "pk_pub".into(),
            key_id: second,
            key_hash: hash(2),
            revoked_at: None,
        }
    );
    assert_eq!(s.lookup_publisher_key(&hash(9)).unwrap(), None);
    s.revoke_publisher_key(first, 50).unwrap();
    s.revoke_publisher_key(first, 99).unwrap();
    assert_eq!(
        s.lookup_publisher_key(&hash(1))
            .unwrap()
            .unwrap()
            .revoked_at,
        Some(50),
        "a second revoke keeps the first time"
    );
    assert_code(s.revoke_publisher_key(first + 10_000, 1), KEY_UNKNOWN);
    let listed = s.list_publisher_keys("pk_pub").unwrap();
    assert_eq!(
        listed,
        vec![
            KeyInfo {
                key_id: first,
                label: "first".into(),
                issued_at: 10,
                revoked_at: Some(50)
            },
            KeyInfo {
                key_id: second,
                label: "rotated".into(),
                issued_at: 20,
                revoked_at: None
            },
        ]
    );
    assert!(s.list_publisher_keys("pk_nobody").unwrap().is_empty());
    assert_code(s.issue_publisher_key("", hash(4), "x", 1), TOO_LARGE);
    assert_code(s.issue_publisher_key("pk_pub", hash(4), "", 1), TOO_LARGE);
    assert_code(
        s.issue_publisher_key("pk_pub", hash(4), &"l".repeat(129), 1),
        TOO_LARGE,
    );
    assert_eq!(s.lookup_publisher_key(&hash(4)).unwrap(), None);
}

fn reviews(s: &dyn StoreStateStore) {
    let a = review("rv_a", "rv_app", "1.0.0", 100);
    assert!(s.create_review(&a).unwrap());
    let mut again = a.clone();
    again.status = ReviewStatus::Done;
    assert!(!s.create_review(&again).unwrap(), "create is idempotent");
    assert_eq!(s.get_review("rv_a").unwrap().unwrap(), a, "left untouched");
    assert_eq!(s.get_review("rv_none").unwrap(), None);

    let summary = ReviewSummary::new(r#"{"findings":[1, 2]}"#.to_owned()).unwrap();
    s.update_review(
        "rv_a",
        &ReviewUpdate {
            status: ReviewStatus::Done,
            verdict: Some("approve".into()),
            summary: Some(summary.clone()),
            error_code: None,
            now: 150,
        },
    )
    .unwrap();
    let got = s.get_review("rv_a").unwrap().unwrap();
    assert_eq!(got.status, ReviewStatus::Done);
    assert_eq!(got.verdict.as_deref(), Some("approve"));
    assert_eq!(
        got.summary.unwrap().as_str(),
        summary.as_str(),
        "byte for byte"
    );
    assert_eq!((got.created_at, got.updated_at), (100, 150));

    let update = ReviewUpdate {
        status: ReviewStatus::Failed,
        verdict: None,
        summary: None,
        error_code: Some("review.crashed".into()),
        now: 1,
    };
    assert_code(s.update_review("rv_none", &update), REVIEW_UNKNOWN);

    assert!(
        s.create_review(&review("rv_c", "rv_app", "1.0.0", 100))
            .unwrap()
    );
    assert!(
        s.create_review(&review("rv_b", "rv_app", "1.0.0", 100))
            .unwrap()
    );
    assert!(
        s.create_review(&review("rv_d", "rv_app", "1.0.0", 90))
            .unwrap()
    );
    assert!(
        s.create_review(&review("rv_e", "rv_app", "2.0.0", 50))
            .unwrap()
    );
    let ids: Vec<String> = s
        .reviews_for(&app("rv_app"), &version("1.0.0"))
        .unwrap()
        .into_iter()
        .map(|r| r.review_id)
        .collect();
    assert_eq!(
        ids,
        ["rv_d", "rv_a", "rv_b", "rv_c"],
        "oldest first, then by id"
    );
    assert!(
        s.reviews_for(&app("rv_app"), &version("3.0.0"))
            .unwrap()
            .is_empty()
    );

    assert_code(
        s.create_review(&review("", "rv_app", "1.0.0", 1)),
        TOO_LARGE,
    );
    let mut long = review("rv_f", "rv_app", "1.0.0", 1);
    long.verdict = Some("v".repeat(129));
    assert_code(s.create_review(&long), TOO_LARGE);
    assert_eq!(s.get_review("rv_f").unwrap(), None, "nothing was stored");

    assert_code(
        ReviewSummary::new(format!("\"{}\"", "x".repeat(MAX_SUMMARY))).map(|_| ()),
        TOO_LARGE,
    );
    assert!(ReviewSummary::new(format!("\"{}\"", "x".repeat(MAX_SUMMARY - 2))).is_ok());
    assert_code(
        ReviewSummary::new("{not json".into()).map(|_| ()),
        BAD_SUMMARY,
    );
}

const P: ListingState = ListingState::PendingReview;
const L: ListingState = ListingState::Listed;
const R: ListingState = ListingState::Rejected;
const W: ListingState = ListingState::Withdrawn;

fn listing_table(s: &dyn StoreStateStore) {
    let v = version("1.0.0");
    let mut n = 0;
    // From no row: only pending_review may create it.
    for to in [P, L, R, W] {
        n += 1;
        let name = app(&format!("lt_none_{n}"));
        let result = s.set_listing(&name, &v, to, "admin", 5);
        if to == P {
            result.unwrap();
            let row = s.listing(&name, &v).unwrap().unwrap();
            assert_eq!(
                row,
                ListingRecord {
                    state: P,
                    by: "admin".into(),
                    at: 5
                }
            );
        } else {
            assert_code(result, BAD_TRANSITION);
            assert_eq!(s.listing(&name, &v).unwrap(), None, "no row was made");
        }
    }
    // From each state to each state.
    let paths: [(ListingState, &[ListingState]); 4] =
        [(P, &[P]), (L, &[P, L]), (R, &[P, R]), (W, &[P, L, W])];
    for (from, path) in paths {
        for to in [P, L, R, W] {
            n += 1;
            let name = app(&format!("lt_{n}"));
            for step in path {
                s.set_listing(&name, &v, *step, "a", 1).unwrap();
            }
            let allowed = matches!((from, to), (P, L) | (P, R) | (L, W));
            let result = s.set_listing(&name, &v, to, "b", 2);
            let stored = s.listing(&name, &v).unwrap().unwrap();
            if allowed {
                result.unwrap();
                assert_eq!(
                    stored,
                    ListingRecord {
                        state: to,
                        by: "b".into(),
                        at: 2
                    }
                );
            } else {
                assert_code(result, BAD_TRANSITION);
                assert_eq!(stored.state, from, "a refused transition changes nothing");
                assert_eq!((stored.by.as_str(), stored.at), ("a", 1));
            }
        }
    }
    assert_code(s.set_listing(&app("lt_x"), &v, P, "", 1), TOO_LARGE);
}

fn listing_fails_closed(s: &dyn StoreStateStore) {
    let v = version("1.2.3");
    let apps = [
        "fc_none",
        "fc_pending",
        "fc_listed",
        "fc_rejected",
        "fc_withdrawn",
    ];
    s.set_listing(&app("fc_pending"), &v, P, "a", 1).unwrap();
    for name in &apps[2..] {
        s.set_listing(&app(name), &v, P, "a", 1).unwrap();
    }
    s.set_listing(&app("fc_listed"), &v, L, "a", 2).unwrap();
    s.set_listing(&app("fc_rejected"), &v, R, "a", 2).unwrap();
    s.set_listing(&app("fc_withdrawn"), &v, L, "a", 2).unwrap();
    s.set_listing(&app("fc_withdrawn"), &v, W, "a", 3).unwrap();
    // A second version of the listed app is not listed by its sibling.
    let other = version("1.2.4");
    let mut candidates: Vec<(AppName, Version)> =
        apps.iter().map(|a| (app(a), v.clone())).collect();
    candidates.push((app("fc_listed"), other));
    let listed = s.listed_among(&candidates).unwrap();
    assert_eq!(
        listed.into_iter().collect::<Vec<_>>(),
        vec![(app("fc_listed"), v.clone())],
        "only an explicit listed row lists"
    );
    assert!(s.listed_among(&[]).unwrap().is_empty());
    let many: Vec<(AppName, Version)> = (0..=MAX_CANDIDATES)
        .map(|i| (app(&format!("fc_many_{i}")), v.clone()))
        .collect();
    assert_code(s.listed_among(&many), TOO_LARGE);
    assert!(s.listed_among(&many[..MAX_CANDIDATES]).unwrap().is_empty());
}

fn idempotency(s: &dyn StoreStateStore) {
    let f1 = Fingerprint([1; 32]);
    let f2 = Fingerprint([2; 32]);
    assert_eq!(
        s.idempotency_begin("id_pub", "k1", &f1, 100).unwrap(),
        Begin::New
    );
    assert_eq!(
        s.idempotency_begin("id_pub", "k1", &f1, 101).unwrap(),
        Begin::Replay { review_id: None }
    );
    assert_eq!(
        s.idempotency_begin("id_pub", "k1", &f2, 102).unwrap(),
        Begin::Conflict
    );
    assert_eq!(
        s.idempotency_begin("id_other", "k1", &f2, 103).unwrap(),
        Begin::New,
        "the key is per publisher"
    );

    assert_code(
        s.idempotency_finish("id_pub", "k1", "id_rv"),
        REVIEW_UNKNOWN,
    );
    s.create_review(&review("id_rv", "id_app", "1.0.0", 1))
        .unwrap();
    s.create_review(&review("id_rv2", "id_app", "1.0.0", 1))
        .unwrap();
    s.idempotency_finish("id_pub", "k1", "id_rv").unwrap();
    s.idempotency_finish("id_pub", "k1", "id_rv").unwrap();
    assert_code(
        s.idempotency_finish("id_pub", "k1", "id_rv2"),
        IDEMPOTENCY_CONFLICT,
    );
    assert_code(
        s.idempotency_finish("id_pub", "nokey", "id_rv"),
        IDEMPOTENCY_CONFLICT,
    );
    assert_eq!(
        s.idempotency_begin("id_pub", "k1", &f1, 104).unwrap(),
        Begin::Replay {
            review_id: Some("id_rv".into())
        }
    );
    assert_eq!(
        s.idempotency_begin("id_pub", "k1", &f2, 105).unwrap(),
        Begin::Conflict
    );

    // Prune: rows created before the bound go, the rest stay.
    assert_eq!(
        s.idempotency_begin("id_pub", "k2", &f1, 200).unwrap(),
        Begin::New
    );
    assert_eq!(s.idempotency_prune(150).unwrap(), 2);
    assert_eq!(s.idempotency_prune(150).unwrap(), 0);
    assert_eq!(
        s.idempotency_begin("id_pub", "k1", &f2, 300).unwrap(),
        Begin::New
    );
    assert_eq!(
        s.idempotency_begin("id_pub", "k2", &f1, 301).unwrap(),
        Begin::Replay { review_id: None }
    );
    assert_code(s.idempotency_begin("id_pub", "", &f1, 1), TOO_LARGE);
    assert_code(
        s.idempotency_begin("id_pub", &"k".repeat(129), &f1, 1),
        TOO_LARGE,
    );
}

fn revocations(s: &dyn StoreStateStore) {
    let key = |id: &str, at: i64, reason: &str| KeyRevocation {
        key_id: id.into(),
        reason: reason.into(),
        at,
    };
    assert!(s.revoked_keys().unwrap().is_empty());
    assert!(s.revoke_key(&key("kb", 20, "second by time")).unwrap());
    assert!(s.revoke_key(&key("ka", 20, "same time, id order")).unwrap());
    assert!(s.revoke_key(&key("kc", 10, "first by time")).unwrap());
    assert!(
        !s.revoke_key(&key("kc", 99, "ignored")).unwrap(),
        "first record wins"
    );
    let ids: Vec<String> = s
        .revoked_keys()
        .unwrap()
        .into_iter()
        .map(|k| k.key_id)
        .collect();
    assert_eq!(ids, ["kc", "ka", "kb"]);
    assert_eq!(s.revoked_keys().unwrap()[0].reason, "first by time");
    assert_code(s.revoke_key(&key("", 1, "x")), TOO_LARGE);
    assert_code(s.revoke_key(&key("kz", 1, &"r".repeat(1025))), TOO_LARGE);
    assert_eq!(s.revoked_keys().unwrap().len(), 3);

    let bundle = |n: u64, at: i64| BundleRevocation {
        digest: digest(n),
        reason: "bad".into(),
        at,
    };
    assert!(s.revoke_bundle(&bundle(0xb2, 5)).unwrap());
    assert!(s.revoke_bundle(&bundle(0xb1, 5)).unwrap());
    assert!(!s.revoke_bundle(&bundle(0xb1, 9)).unwrap());
    let got = s.revoked_bundles().unwrap();
    assert_eq!(got, vec![bundle(0xb1, 5), bundle(0xb2, 5)]);
}

fn generation_and_list(s: &dyn StoreStateStore) {
    assert_eq!(s.last_issued_list().unwrap(), None);
    let g1 = s.next_generation(10).unwrap();
    let g2 = s.next_generation(11).unwrap();
    let g3 = s.next_generation(12).unwrap();
    assert!(
        g1 >= 1 && g2 == g1 + 1 && g3 == g2 + 1,
        "strictly increasing"
    );

    let body = "{\n  \"generation\": 1,   \"keys\":[]\n}".to_owned();
    let list = IssuedList {
        generation: g2,
        issued_at: 20,
        next_update: 30,
        body_json: body.clone(),
    };
    s.record_issued_list(&list).unwrap();
    assert_eq!(
        s.last_issued_list().unwrap().unwrap(),
        list,
        "byte for byte"
    );
    assert_code(s.record_issued_list(&list), BAD_TRANSITION);
    let older = IssuedList {
        generation: g1,
        ..list.clone()
    };
    assert_code(s.record_issued_list(&older), BAD_TRANSITION);
    let newer = IssuedList {
        generation: g3,
        body_json: "{}".into(),
        ..list.clone()
    };
    s.record_issued_list(&newer).unwrap();
    assert_eq!(s.last_issued_list().unwrap().unwrap().body_json, "{}");
    let huge = IssuedList {
        generation: g3 + 1,
        body_json: "x".repeat(MAX_LIST_BODY + 1),
        ..list
    };
    assert_code(s.record_issued_list(&huge), TOO_LARGE);
    assert_eq!(s.last_issued_list().unwrap().unwrap().generation, g3);
}

fn audit(s: &dyn StoreStateStore) {
    let entry = |kind, at| AuditEntry {
        kind,
        actor: "operator".into(),
        publisher_id: Some("au_pub".into()),
        app: Some(app("au_app")),
        version: Some(version("1.0.0-beta.1+b7")),
        digest: Some(digest(0xa0)),
        reason: "because".into(),
        at,
    };
    let start = s.read_audit(0, 1000).unwrap().len() as u64;
    let kinds = [
        AuditKind::Publish,
        AuditKind::Yank,
        AuditKind::Withdraw,
        AuditKind::RevokePublisher,
        AuditKind::RevokeKey,
        AuditKind::RevokeBundle,
        AuditKind::Approve,
        AuditKind::Reject,
        AuditKind::KeyIssue,
        AuditKind::KeyRevoke,
    ];
    let mut ids = Vec::new();
    for (i, kind) in kinds.into_iter().enumerate() {
        ids.push(s.append_audit(&entry(kind, i as i64)).unwrap());
    }
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
    let bare = AuditEntry {
        kind: AuditKind::Publish,
        actor: "a".into(),
        publisher_id: None,
        app: None,
        version: None,
        digest: None,
        reason: String::new(),
        at: 99,
    };
    let bare_id = s.append_audit(&bare).unwrap();
    let all = s.read_audit(ids[0] - 1, 1000).unwrap();
    assert_eq!(all.len(), 11);
    assert_eq!(all[0].entry, entry(AuditKind::Publish, 0));
    assert_eq!(
        all[10],
        AuditRecord {
            id: bare_id,
            entry: bare
        }
    );
    let page = s.read_audit(ids[2], 3).unwrap();
    assert_eq!(page.iter().map(|r| r.id).collect::<Vec<_>>(), ids[3..6]);
    assert!(s.read_audit(bare_id, 10).unwrap().is_empty());
    assert_eq!(s.read_audit(0, 5000).unwrap().len() as u64, start + 11);

    let mut long = entry(AuditKind::Yank, 1);
    long.reason = "r".repeat(1025);
    assert_code(s.append_audit(&long), TOO_LARGE);
    long.reason = String::new();
    long.actor = String::new();
    assert_code(s.append_audit(&long), TOO_LARGE);
    assert_eq!(
        s.read_audit(0, 5000).unwrap().len() as u64,
        start + 11,
        "refused, not stored"
    );
}
