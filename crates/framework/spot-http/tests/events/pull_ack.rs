// ------------------------------------------------------------------------------------------
// pull

#[tokio::test]
async fn pull_returns_the_batch_and_the_lease_and_passes_the_defaults() {
    let f = fixture();
    let r = call(
        &f,
        "POST",
        "/api/v1/events/consumers/billing/pull",
        &[],
        Some(json!({"partition": 1})),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(r.body["data"]["events"].as_array().unwrap().len(), 1);
    assert_eq!(r.body["data"]["next_position"], POSITION);
    assert_eq!(r.body["data"]["skipped"], 3);
    assert_eq!(r.body["data"]["window_full"], false);
    assert!(r.body.get("messages").is_none());
    assert_eq!(r.headers.get("x-consumer-lease").unwrap(), "lease-1");
    let (_, detail) = last_call(&f);
    assert!(
        detail.contains("partition: 1, limit: 100, wait_ms: 0"),
        "{detail}"
    );
    assert!(detail.contains("None"), "no lease was sent: {detail}");
}

#[tokio::test]
async fn pull_sends_the_lease_back_and_carries_a_window_full_message() {
    let f = fixture();
    f.fake.lock().window_full = true;
    let r = call(
        &f,
        "POST",
        "/api/v1/events/consumers/billing/pull",
        &[("x-consumer-lease", "lease-9")],
        Some(json!({"partition": 0, "limit": 1000, "wait_ms": 30000})),
    )
    .await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["data"]["window_full"], true);
    assert_eq!(r.body["data"]["events"], json!([]));
    assert_eq!(r.body["messages"][0]["code"], "eventstream.window_full");
    assert_eq!(r.body["messages"][0]["level"], "info");
    assert_eq!(r.headers.get("x-consumer-lease").unwrap(), "lease-9");
    let (_, detail) = last_call(&f);
    assert!(detail.contains("Some(\"lease-9\")"), "{detail}");
    assert!(detail.contains("limit: 1000, wait_ms: 30000"), "{detail}");
}

#[tokio::test]
async fn pull_refuses_a_bad_request_before_the_port() {
    let f = fixture();
    let path = "/api/v1/events/consumers/billing/pull";
    for body in [
        json!({}),
        json!({"partition": 64}),
        json!({"partition": -1}),
        json!({"partition": "0"}),
        json!({"partition": 0, "limit": 0}),
        json!({"partition": 0, "limit": 1001}),
        json!({"partition": 0, "wait_ms": 30001}),
        json!({"partition": 0, "extra": true}),
    ] {
        let r = call(&f, "POST", path, &[], Some(body.clone())).await;
        assert_eq!((r.status, code(&r)), (400, "api.invalid_request"), "{body}");
    }
    for lease in ["", "has space", "tab\there"] {
        let r = call(
            &f,
            "POST",
            path,
            &[("x-consumer-lease", lease)],
            Some(json!({"partition": 0})),
        )
        .await;
        assert_eq!(r.status, 400, "{lease:?}");
    }
    let long = "l".repeat(513);
    let r = call(
        &f,
        "POST",
        path,
        &[("x-consumer-lease", &long)],
        Some(json!({"partition": 0})),
    )
    .await;
    assert_eq!(r.status, 400);
    assert!(f.fake.lock().calls.is_empty());
}

#[tokio::test]
async fn pull_reports_busy_resync_and_unknown_with_their_statuses() {
    let f = fixture();
    let path = "/api/v1/events/consumers/billing/pull";
    let busy = ApiError::from_code(Code::EventstreamConsumerBusy).detail("retry_after", json!(7));
    fail_next(&f, busy);
    let r = call(&f, "POST", path, &[], Some(json!({"partition": 0}))).await;
    assert_eq!((r.status, code(&r)), (409, "eventstream.consumer_busy"));
    assert_eq!(r.headers.get(header::RETRY_AFTER).unwrap(), "7");
    let resync = ApiError::from_code(Code::EventstreamResyncRequired)
        .detail("retained_from", json!(POSITION));
    fail_next(&f, resync);
    let r = call(&f, "POST", path, &[], Some(json!({"partition": 0}))).await;
    assert_eq!((r.status, code(&r)), (410, "eventstream.resync_required"));
    assert_eq!(r.body["error"]["details"]["retained_from"], POSITION);
    fail_next(&f, ApiError::from_code(Code::EventstreamConsumerNotFound));
    let r = call(&f, "POST", path, &[], Some(json!({"partition": 0}))).await;
    assert_eq!(r.status, 404);
}

// ------------------------------------------------------------------------------------------
// ack and restart

#[tokio::test]
async fn ack_answers_the_checkpoint_and_passes_the_lease() {
    let f = fixture();
    let r = call(
        &f,
        "POST",
        "/api/v1/events/consumers/billing/ack",
        &[("x-consumer-lease", "lease-1")],
        Some(json!({"partition": 1, "position": POSITION})),
    )
    .await;
    assert_eq!(r.status, 200, "{}", r.body);
    assert_eq!(
        r.body["data"],
        json!({"partition": 1, "position": POSITION})
    );
    let (op, detail) = last_call(&f);
    assert_eq!(op, "ack");
    assert!(
        detail.contains("Some(\"lease-1\")") && detail.contains(POSITION),
        "{detail}"
    );
}

#[tokio::test]
async fn ack_refuses_a_malformed_position_and_reports_the_ports_refusals() {
    let f = fixture();
    let path = "/api/v1/events/consumers/billing/ack";
    for position in ["", "v2.AAAA", "v1.", "v1.has space", "AAAA", "v1.a.b"] {
        let r = call(
            &f,
            "POST",
            path,
            &[],
            Some(json!({"partition": 0, "position": position})),
        )
        .await;
        assert_eq!(
            (r.status, code(&r)),
            (422, "eventstream.position_invalid"),
            "{position:?}"
        );
    }
    for body in [
        json!({"partition": 0}),
        json!({"position": POSITION}),
        json!({"partition": 0, "position": 7}),
    ] {
        let r = call(&f, "POST", path, &[], Some(body.clone())).await;
        assert!(matches!(r.status, 400 | 422), "{body}: {}", r.status);
    }
    assert!(f.fake.lock().calls.is_empty());
    for (error, status, expected) in [
        (Code::EventstreamAckRefused, 409, "eventstream.ack_refused"),
        (
            Code::EventstreamPositionInvalid,
            422,
            "eventstream.position_invalid",
        ),
        (
            Code::EventstreamConsumerBusy,
            409,
            "eventstream.consumer_busy",
        ),
        (
            Code::EventstreamConsumerNotFound,
            404,
            "eventstream.consumer_not_found",
        ),
    ] {
        fail_next(&f, ApiError::from_code(error));
        let r = call(
            &f,
            "POST",
            path,
            &[],
            Some(json!({"partition": 0, "position": POSITION})),
        )
        .await;
        assert_eq!((r.status, code(&r)), (status, expected));
    }
}

#[tokio::test]
async fn restart_takes_each_kind_of_start_and_answers_the_checkpoint() {
    let f = fixture();
    let path = "/api/v1/events/consumers/billing/restart";
    for (from, needle) in [
        (json!({"kind": "tail"}), "Tail"),
        (json!({"kind": "from_beginning"}), "FromBeginning"),
        (
            json!({"kind": "from_position", "position": POSITION}),
            "FromPosition",
        ),
    ] {
        let r = call(
            &f,
            "POST",
            path,
            &[],
            Some(json!({"partition": 0, "from": from})),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.body);
        assert_eq!(r.body["data"]["partition"], 0);
        assert!(last_call(&f).1.contains(needle));
    }
    for body in [
        json!({"partition": 0}),
        json!({"from": {"kind": "tail"}}),
        json!({"partition": 0, "from": {"kind": "from_position"}}),
        json!({"partition": 0, "from": "tail"}),
    ] {
        let r = call(&f, "POST", path, &[], Some(body.clone())).await;
        assert_eq!(r.status, 400, "{body}");
    }
    let r = call(
        &f,
        "POST",
        path,
        &[],
        Some(json!({"partition": 0, "from": {"kind": "from_position", "position": "bad"}})),
    )
    .await;
    assert_eq!((r.status, code(&r)), (422, "eventstream.position_invalid"));
}
