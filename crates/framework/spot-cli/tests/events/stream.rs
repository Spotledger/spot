#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_stream_is_sse_with_one_connection_per_consumer_and_a_bound() {
    let Some(s) = Stack::start(&[]).await else {
        return;
    };
    for name in ["live", "second"] {
        let made = s
            .admin(
                "PUT",
                &format!("/api/v1/events/consumers/{name}"),
                Some(spec(ROLES_ONLY, json!({"kind": "tail"}))),
            )
            .await;
        assert_eq!(made.status, 201, "{:?}", made.body);
    }
    let mut stream = Sse::open(s.addr, &s.host, &s.admin, "consumer=live&partition=0", &[]).await;
    assert!(
        stream
            .until("text/event-stream", Duration::from_secs(10))
            .await,
        "{}",
        stream.seen
    );
    assert!(stream.seen.starts_with("HTTP/1.1 200"), "{}", stream.seen);
    let lease = stream.lease();
    assert!(!lease.is_empty(), "the lease is answered in a header");
    assert!(
        stream
            .seen
            .to_ascii_lowercase()
            .contains("cache-control: no-store")
    );

    // One connection per consumer: a second stream and a pull without the lease are refused.
    let second = http(
        s.addr,
        &s.host,
        Some(&s.admin),
        "GET",
        "/api/v1/events/stream?consumer=live&partition=0",
        &[],
        None,
    )
    .await;
    assert_eq!(
        (second.status, second.code()),
        (409, "eventstream.consumer_busy")
    );
    let pull = s
        .admin(
            "POST",
            "/api/v1/events/consumers/live/pull",
            Some(pull_body()),
        )
        .await;
    assert_eq!(
        (pull.status, pull.code()),
        (409, "eventstream.consumer_busy")
    );

    // The connection bound: two streams per caller here (`max_streams_per_user`), the third is
    // refused with a wait, and a closed stream makes room again.
    let third = s
        .admin(
            "PUT",
            "/api/v1/events/consumers/third",
            Some(spec(ROLES_ONLY, json!({"kind": "tail"}))),
        )
        .await;
    assert_eq!(third.status, 201, "{:?}", third.body);
    let mut second_stream = Sse::open(
        s.addr,
        &s.host,
        &s.admin,
        "consumer=second&partition=0",
        &[],
    )
    .await;
    assert!(
        second_stream
            .until("HTTP/1.1 200", Duration::from_secs(10))
            .await
    );
    let over = http(
        s.addr,
        &s.host,
        Some(&s.admin),
        "GET",
        "/api/v1/events/stream?consumer=third&partition=0",
        &[],
        None,
    )
    .await;
    assert_eq!((over.status, over.code()), (429, "api.rate_limited"));
    assert!(over.header("retry-after").is_some());
    drop(second_stream);

    // An event written now arrives as a frame with an opaque id, and keepalives flow when idle.
    let first = s.make_role("Delta").await;
    let made = s.make_role("Echo").await;
    assert!(
        stream
            .until("event: doc_created", Duration::from_secs(10))
            .await,
        "{}",
        stream.seen
    );
    assert!(stream.until(&first, Duration::from_secs(5)).await);
    assert!(stream.until(&made, Duration::from_secs(5)).await);
    assert!(
        stream.until(": keepalive", Duration::from_secs(5)).await,
        "{}",
        stream.seen
    );
    let ids = stream.ids();
    assert_eq!(ids.len(), 2, "{}", stream.seen);
    assert!(ids[0].starts_with("v1."));
    // The id is the position to acknowledge, and an earlier event of the stream is
    // acknowledged on its own: the lease from the stream's header authorises it, and the
    // checkpoint stops there, not at the last event sent.
    let early = s
        .call(
            &s.admin,
            "POST",
            "/api/v1/events/consumers/live/ack",
            &[("x-consumer-lease", &lease)],
            Some(json!({"partition": 0, "position": ids[0]})),
        )
        .await;
    assert_eq!(early.status, 200, "{:?}", early.body);
    let state = s.admin("GET", "/api/v1/events/consumers/live", None).await;
    assert_eq!(
        state.body["data"]["partitions_state"][0]["checkpoint"], ids[0],
        "{:?}",
        state.body
    );
    let acked = s
        .call(
            &s.admin,
            "POST",
            "/api/v1/events/consumers/live/ack",
            &[("x-consumer-lease", &lease)],
            Some(json!({"partition": 0, "position": ids[1]})),
        )
        .await;
    assert_eq!(acked.status, 200, "{:?}", acked.body);
    // A position issued for another consumer is not accepted here.
    let other = s
        .call(
            &s.admin,
            "POST",
            "/api/v1/events/consumers/second/ack",
            &[],
            Some(json!({"partition": 0, "position": ids[0]})),
        )
        .await;
    assert_eq!(
        (other.status, other.code()),
        (422, "eventstream.position_invalid"),
        "{:?}",
        other.body
    );
    drop(stream);

    // The lease is given back when the connection ends: the consumer can stream again, from its
    // checkpoint, and the acknowledged event is not repeated.
    let mut resumed = None;
    for _ in 0..50 {
        let mut probe = Sse::open(
            s.addr,
            &s.host,
            &s.admin,
            "consumer=live&partition=0",
            &[("Last-Event-ID", &ids[1])],
        )
        .await;
        assert!(probe.until("HTTP/1.1 ", Duration::from_secs(10)).await);
        if probe.seen.starts_with("HTTP/1.1 200") {
            resumed = Some(probe);
            break;
        }
        assert!(
            probe.seen.starts_with("HTTP/1.1 409") || probe.seen.starts_with("HTTP/1.1 429"),
            "{}",
            probe.seen
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut resumed = resumed.expect("the lease was not given back when the stream ended");
    assert!(
        !resumed.until(&made, Duration::from_millis(600)).await,
        "the acknowledged event came back"
    );

    // A bad query is refused with a registry error before a stream opens.
    let bad = http(
        s.addr,
        &s.host,
        Some(&s.admin),
        "GET",
        "/api/v1/events/stream?consumer=live&partition=0&token=x",
        &[],
        None,
    )
    .await;
    assert_eq!(bad.status, 400, "{:?}", bad.body);
    let missing = http(
        s.addr,
        &s.host,
        None,
        "GET",
        "/api/v1/events/stream?consumer=live&partition=0",
        &[],
        None,
    )
    .await;
    assert_eq!(missing.status, 401);
    s.finish().await;
}
