#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rights_ownership_and_what_a_consumer_may_read() {
    let Some(s) = Stack::start(&[
        ("plain@events.test", ""),
        ("reader1@events.test", "Event Reader"),
        ("reader2@events.test", "Event Reader"),
    ])
    .await
    else {
        return;
    };
    // A user with no right is denied: a refusal, never an empty success.
    let plain = s.session("plain@events.test").await;
    for (method, path, body) in [
        ("GET", "/api/v1/events/consumers", None),
        (
            "PUT",
            "/api/v1/events/consumers/mine",
            Some(spec(ROLES_ONLY, json!({"kind": "tail"}))),
        ),
        ("GET", "/api/v1/events/consumers/mine", None),
        (
            "POST",
            "/api/v1/events/consumers/mine/pull",
            Some(pull_body()),
        ),
    ] {
        let reply = s.call(&plain, method, path, &[], body).await;
        assert_eq!(
            (reply.status, reply.code()),
            (403, "permission.denied"),
            "{method} {path}"
        );
    }
    let denied = http(
        s.addr,
        &s.host,
        Some(&plain),
        "GET",
        "/api/v1/events/stream?consumer=mine&partition=0",
        &[],
        None,
    )
    .await;
    assert_eq!((denied.status, denied.code()), (403, "permission.denied"));

    // A reader owns its consumers, and nobody else sees them (404, not 403).
    let r1 = s.session("reader1@events.test").await;
    let r2 = s.session("reader2@events.test").await;
    let body = spec(ROLES_ONLY, json!({"kind": "tail"}));
    // The reserved platform topics need the explicit stream right.
    let reserved = s
        .call(
            &r1,
            "PUT",
            "/api/v1/events/consumers/mine",
            &[],
            Some(body.clone()),
        )
        .await;
    assert_eq!(
        (reserved.status, reserved.code()),
        (403, "permission.denied"),
        "{:?}",
        reserved.body
    );
    // A prefix shorter than a reserved class selects reserved topics too (`d.*` selects `doc_`).
    for short in ["d.*", "do.*", "s.*", "sy.*", "i.*", "identity.*"] {
        let sneaky = json!({"topics": [short], "partitions": 1, "payload_mode": "ids_only"});
        let refused = s
            .call(
                &r1,
                "PUT",
                "/api/v1/events/consumers/sneaky",
                &[],
                Some(sneaky),
            )
            .await;
        assert_eq!(
            (refused.status, refused.code()),
            (403, "permission.denied"),
            "{short}: {:?}",
            refused.body
        );
    }
    let app_topic =
        json!({"topics": ["billing_invoice_sent"], "partitions": 1, "payload_mode": "ids_only"});
    let made = s
        .call(
            &r1,
            "PUT",
            "/api/v1/events/consumers/mine",
            &[],
            Some(app_topic.clone()),
        )
        .await;
    assert_eq!(made.status, 201, "{:?}", made.body);
    assert_eq!(made.body["data"]["owner"].as_str().unwrap().len(), 36);
    for (method, path, body) in [
        ("GET", "/api/v1/events/consumers/mine", None),
        ("DELETE", "/api/v1/events/consumers/mine", None),
        (
            "POST",
            "/api/v1/events/consumers/mine/pull",
            Some(pull_body()),
        ),
        (
            "POST",
            "/api/v1/events/consumers/mine/ack",
            Some(json!({"partition": 0, "position": "v1.bm9wZQ"})),
        ),
        (
            "POST",
            "/api/v1/events/consumers/mine/restart",
            Some(json!({"partition": 0, "from": {"kind": "tail"}})),
        ),
    ] {
        let reply = s.call(&r2, method, path, &[], body).await;
        assert_eq!(
            (reply.status, reply.code()),
            (404, "eventstream.consumer_not_found"),
            "{method} {path}"
        );
    }
    let stream = http(
        s.addr,
        &s.host,
        Some(&r2),
        "GET",
        "/api/v1/events/stream?consumer=mine&partition=0",
        &[],
        None,
    )
    .await;
    assert_eq!(stream.status, 404);
    let theirs = s
        .call(&r2, "GET", "/api/v1/events/consumers", &[], None)
        .await;
    assert!(
        theirs.body["data"].as_array().unwrap().is_empty(),
        "{:?}",
        theirs.body
    );
    // The administrator sees every consumer.
    let all = s.admin("GET", "/api/v1/events/consumers", None).await;
    assert!(
        all.body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "mine")
    );

    // A reader with the stream right registers a reserved-topic consumer, and still sees none
    // of the events about documents it may not read: they are skipped, never delivered.
    let streamer_role = "Event Streamer";
    s.assign("reader2@events.test", streamer_role).await;
    let r2 = s.session("reader2@events.test").await;
    let mine = s
        .call(
            &r2,
            "PUT",
            "/api/v1/events/consumers/blind",
            &[],
            Some(body),
        )
        .await;
    assert_eq!(mine.status, 201, "{:?}", mine.body);
    for name in ["Echo", "Foxtrot"] {
        s.make_role(name).await;
    }
    let pull = s
        .call(
            &r2,
            "POST",
            "/api/v1/events/consumers/blind/pull",
            &[],
            Some(pull_body()),
        )
        .await;
    assert_eq!(pull.status, 200, "{:?}", pull.body);
    assert!(
        event_ids(&pull).is_empty(),
        "an unreadable document's event was delivered"
    );
    assert!(
        pull.body["data"]["skipped"].as_u64().unwrap() >= 2,
        "{:?}",
        pull.body
    );
    // The administrator's own consumer over the same events does see them.
    s.admin(
        "PUT",
        "/api/v1/events/consumers/sees",
        Some(spec(ROLES_ONLY, json!({"kind": "from_beginning"}))),
    )
    .await;
    let sees = s
        .admin(
            "POST",
            "/api/v1/events/consumers/sees/pull",
            Some(pull_body()),
        )
        .await;
    assert!(event_ids(&sees).len() >= 2, "{:?}", sees.body);
    s.finish().await;
}
