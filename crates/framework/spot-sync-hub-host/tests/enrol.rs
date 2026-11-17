//! The hub's enrolment endpoint (WP-RTB-02.T13): the shape the node's `HttpEnroller` posts, one
//! answer for every wrong code, no secret in any answer, the throttle and the body cap.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use spot_core::{Clock, SystemClock};
use spot_sync_hub_host::enrol::{
    CODE_INVALID, ENROL_PATH, EnrolSettings, MAX_BODY_BYTES, Redeemer, Refusal, ThreadedRedeemer,
    enrol_router,
};
use tower::ServiceExt;

const GOOD_CODE: &str = "code1.00112233445566778899aabbccddeeff";

#[derive(Default)]
struct Fake {
    #[allow(clippy::type_complexity)]
    seen: Mutex<Vec<(String, String, Vec<u8>, String)>>,
}

impl Redeemer for Fake {
    fn redeem(
        &self,
        code: &str,
        node: &str,
        certificate: &[u8],
        source: &str,
    ) -> Result<(), Refusal> {
        self.seen.lock().unwrap().push((
            code.to_owned(),
            node.to_owned(),
            certificate.to_vec(),
            source.to_owned(),
        ));
        if code == GOOD_CODE {
            Ok(())
        } else {
            Err(Refusal {
                code: CODE_INVALID.to_owned(),
                message: "the enrolment code is not valid".to_owned(),
                status: 403,
            })
        }
    }
}

fn clock() -> Arc<SystemClock> {
    Arc::new(SystemClock::new())
}

fn router(fake: Arc<Fake>, per_second: u32, clock: Arc<SystemClock>) -> axum::Router {
    enrol_router(
        fake,
        EnrolSettings {
            per_second,
            queued: 4,
        },
        clock as Arc<dyn Clock>,
    )
    .unwrap()
}

fn post(body: impl Into<Body>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(ENROL_PATH)
        .header(header::CONTENT_TYPE, "application/json")
        .body(body.into())
        .unwrap()
}

fn enrol_body(code: &str, certificate_hex: &str) -> String {
    serde_json::json!({
        "node": "edge-khi#branch_01",
        "code": code,
        "certificate": certificate_hex,
    })
    .to_string()
}

async fn text(response: axum::response::Response) -> (StatusCode, String) {
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn a_good_code_is_redeemed_with_the_decoded_certificate() {
    let fake = Arc::new(Fake::default());
    let app = router(fake.clone(), 100, clock());
    let (status, _) = text(
        app.oneshot(post(enrol_body(GOOD_CODE, "00ff10ab")))
            .await
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let seen = fake.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].1, "edge-khi#branch_01");
    assert_eq!(seen[0].2, vec![0x00, 0xff, 0x10, 0xab]);
    assert_eq!(seen[0].3, "node:edge-khi#branch_01");
}

#[tokio::test]
async fn every_wrong_code_gets_the_same_answer_and_it_never_echoes_the_code() {
    let fake = Arc::new(Fake::default());
    let mut answers = Vec::new();
    for wrong in [
        "code1.deadbeef",
        "garbage",
        "code2.00112233445566778899aabbccddeeff",
    ] {
        let app = router(fake.clone(), 100, clock());
        let (status, body) =
            text(app.oneshot(post(enrol_body(wrong, "00ff"))).await.unwrap()).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(body.contains(CODE_INVALID));
        assert!(!body.contains(wrong), "the code is never echoed");
        answers.push(body);
    }
    assert!(answers.windows(2).all(|pair| pair[0] == pair[1]));
}

#[tokio::test]
async fn a_malformed_request_is_refused_without_reaching_the_redeemer() {
    let fake = Arc::new(Fake::default());
    let cases: Vec<(String, StatusCode)> = vec![
        ("not json".to_owned(), StatusCode::BAD_REQUEST),
        (
            r#"{"node":"a#b","code":"c","certificate":"00","extra":1}"#.to_owned(),
            StatusCode::BAD_REQUEST,
        ),
        (enrol_body(GOOD_CODE, "xyz"), StatusCode::BAD_REQUEST),
        (enrol_body(GOOD_CODE, "abc"), StatusCode::BAD_REQUEST),
        (enrol_body(GOOD_CODE, ""), StatusCode::BAD_REQUEST),
        (
            "x".repeat(MAX_BODY_BYTES + 1),
            StatusCode::PAYLOAD_TOO_LARGE,
        ),
    ];
    for (body, expected) in cases {
        let app = router(fake.clone(), 100, clock());
        let (status, answer) = text(app.oneshot(post(body)).await.unwrap()).await;
        assert_eq!(status, expected);
        assert!(answer.contains("sync_http.invalid_request"));
    }
    assert!(fake.seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_endpoint_throttles_before_the_redeemer_is_reached() {
    let fake = Arc::new(Fake::default());
    // One token a second, and the two requests follow each other at once.
    let app = router(fake.clone(), 1, clock());
    let first = app
        .clone()
        .oneshot(post(enrol_body(GOOD_CODE, "00")))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let second = app
        .oneshot(post(enrol_body(GOOD_CODE, "00")))
        .await
        .unwrap();
    assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        fake.seen.lock().unwrap().len(),
        1,
        "the throttled one never reached the redeemer"
    );
}

#[tokio::test]
async fn a_closed_redeemer_answers_unavailable() {
    let redeemer = ThreadedRedeemer::spawn(Arc::new(Fake::default()), 2).unwrap();
    assert!(
        redeemer
            .redeem_async(GOOD_CODE.into(), "a#b".into(), vec![1], "s".into())
            .await
            .is_some()
    );
    redeemer.close();
    assert!(
        redeemer
            .redeem_async(GOOD_CODE.into(), "a#b".into(), vec![1], "s".into())
            .await
            .is_none()
    );
    assert_eq!(format!("{redeemer:?}"), "ThreadedRedeemer { .. }");
}

#[tokio::test]
async fn only_post_on_the_one_path_is_served() {
    let app = router(Arc::new(Fake::default()), 100, clock());
    let get = Request::builder()
        .method("GET")
        .uri(ENROL_PATH)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(get).await.unwrap().status(),
        StatusCode::METHOD_NOT_ALLOWED
    );
    let other = Request::builder()
        .method("POST")
        .uri("/sync/v1/other")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.oneshot(other).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}
