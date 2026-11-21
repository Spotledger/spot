// ------------------------------------------------------------------------------------------
// the stream

async fn stream_response(
    f: &Fixture,
    query: &str,
    extra: &[(&str, &str)],
) -> axum::http::Response<Body> {
    send(
        f,
        "GET",
        &format!("/api/v1/events/stream?{query}"),
        Some(TOKEN),
        extra,
        None,
    )
    .await
}

async fn next_text(body: &mut Body) -> Option<String> {
    loop {
        let frame = body.frame().await?.unwrap();
        if let Ok(data) = frame.into_data() {
            return Some(String::from_utf8(data.to_vec()).unwrap());
        }
    }
}

fn frame_of(id: &str, topic: &str) -> FeedItem {
    FeedItem::Frame(format!("id: {id}\nevent: {topic}\ndata: {{}}\n\n").into_bytes())
}

#[tokio::test(start_paused = true)]
async fn the_stream_is_sse_with_the_lease_and_no_store() {
    let f = fixture();
    {
        let mut script = f.fake.lock();
        script.frames.push_back(frame_of(POSITION, "doc_posted"));
        script.end = true;
    }
    let response = stream_response(&f, "consumer=billing&partition=1", &[]).await;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE).unwrap(),
        "text/event-stream; charset=utf-8"
    );
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "no-store"
    );
    assert_eq!(
        response.headers().get("x-consumer-lease").unwrap(),
        "lease-s"
    );
    let mut body = response.into_body();
    let first = next_text(&mut body).await.unwrap();
    assert_eq!(
        first,
        format!("id: {POSITION}\nevent: doc_posted\ndata: {{}}\n\n")
    );
    assert!(
        next_text(&mut body).await.is_none(),
        "a closed feed ends the stream"
    );
    let (_, detail) = last_call(&f);
    assert!(
        detail.contains("consumer: \"billing\"") && detail.contains("partition: 1"),
        "{detail}"
    );
    assert!(detail.contains("after: None"), "{detail}");
}

#[tokio::test(start_paused = true)]
async fn the_stream_sends_a_keepalive_when_idle_and_ends_at_the_idle_limit() {
    let f = fixture();
    f.fake.lock().limits = Some(StreamLimits {
        heartbeat: Duration::from_secs(1),
        max_idle: Duration::from_secs(3),
    });
    let response = stream_response(&f, "consumer=billing&partition=0", &[]).await;
    let mut body = response.into_body();
    let mut keepalives = 0;
    while let Some(text) = next_text(&mut body).await {
        assert_eq!(text, ": keepalive\n\n");
        keepalives += 1;
        assert!(keepalives < 10, "the stream never ended");
    }
    assert!((2..=3).contains(&keepalives), "keepalives: {keepalives}");
}

#[tokio::test(start_paused = true)]
async fn an_event_resets_the_idle_clock() {
    let f = fixture();
    f.fake.lock().limits = Some(StreamLimits {
        heartbeat: Duration::from_secs(1),
        max_idle: Duration::from_secs(2),
    });
    f.fake
        .lock()
        .frames
        .push_back(frame_of(POSITION, "doc_posted"));
    let response = stream_response(&f, "consumer=billing&partition=0", &[]).await;
    let mut body = response.into_body();
    let started = tokio::time::Instant::now();
    let first = next_text(&mut body).await.unwrap();
    assert!(first.starts_with("id: "));
    let mut rest = 0;
    while next_text(&mut body).await.is_some() {
        rest += 1;
    }
    assert!(rest >= 1);
    assert!(started.elapsed() >= Duration::from_secs(2));
}

#[tokio::test(start_paused = true)]
async fn last_event_id_is_preferred_to_after_and_is_passed_through() {
    let f = fixture();
    f.fake.lock().end = true;
    let other = "v1.AAAAAAAAAAIAAAAAAAAAKg";
    let response = stream_response(
        &f,
        &format!("consumer=billing&partition=0&after={other}"),
        &[("last-event-id", POSITION)],
    )
    .await;
    assert_eq!(response.status().as_u16(), 200);
    assert!(
        last_call(&f)
            .1
            .contains(&format!("after: Some(\"{POSITION}\")"))
    );
    let response = stream_response(
        &f,
        &format!("consumer=billing&partition=0&after={other}"),
        &[],
    )
    .await;
    assert_eq!(response.status().as_u16(), 200);
    assert!(
        last_call(&f)
            .1
            .contains(&format!("after: Some(\"{other}\")"))
    );
}

#[tokio::test]
async fn the_stream_refuses_a_bad_query_and_never_reads_a_token_from_the_url() {
    let f = fixture();
    for (query, status) in [
        ("partition=0", 400),
        ("consumer=billing", 400),
        ("consumer=Billing&partition=0", 400),
        ("consumer=billing&partition=64", 400),
        ("consumer=billing&partition=x", 400),
        ("consumer=billing&partition=0&consumer=stock", 400),
        ("consumer=billing&partition=0&token=secret", 400),
        ("consumer=billing&partition=0&access_token=secret", 400),
        ("consumer=billing&partition=0&after=bad", 422),
    ] {
        let response = stream_response(&f, query, &[]).await;
        assert_eq!(response.status().as_u16(), status, "{query}");
    }
    let response = stream_response(
        &f,
        "consumer=billing&partition=0",
        &[("last-event-id", "bad")],
    )
    .await;
    assert_eq!(response.status().as_u16(), 422);
    assert!(
        f.fake.lock().calls.is_empty(),
        "a bad stream request reached the port"
    );
}

#[tokio::test]
async fn the_stream_reports_busy_and_the_connection_bound_before_any_body() {
    let f = fixture();
    fail_next(&f, ApiError::from_code(Code::EventstreamConsumerBusy));
    let response = stream_response(&f, "consumer=billing&partition=0", &[]).await;
    assert_eq!(response.status().as_u16(), 409);
    assert_eq!(
        response.headers().get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    fail_next(
        &f,
        ApiError::from_code(Code::ApiRateLimited).detail("retry_after", json!(5)),
    );
    let response = stream_response(&f, "consumer=billing&partition=0", &[]).await;
    assert_eq!(response.status().as_u16(), 429);
    assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "5");
    fail_next(&f, ApiError::from_code(Code::EventstreamConsumerNotFound));
    let response = stream_response(&f, "consumer=nobody&partition=0", &[]).await;
    assert_eq!(response.status().as_u16(), 404);
}
