// ------------------------------------------------------------------------------------------
// list, get, delete

#[tokio::test]
async fn list_answers_the_callers_consumers_in_the_contract_shape() {
    let f = fixture();
    let r = call(&f, "GET", "/api/v1/events/consumers", &[], None).await;
    assert_eq!(r.status, 200, "{}", r.body);
    let data = r.body["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["name"], "billing");
    assert_eq!(data[0]["filter"], Json::Null);
    assert_eq!(data[0]["payload_mode"], "ids_only");
    assert_eq!(data[0]["partitions_state"][1]["parked"], true);
    assert_eq!(data[0]["partitions_state"][0]["checkpoint"], POSITION);
    assert_eq!(
        r.headers.get(header::CACHE_CONTROL).unwrap(),
        "private, no-store"
    );
    assert!(r.headers.get("x-request-id").is_some());
}

#[tokio::test]
async fn every_operation_needs_a_bearer_and_never_reaches_the_port_without_one() {
    let f = fixture();
    for (method, path) in [
        ("GET", "/api/v1/events/consumers"),
        ("GET", "/api/v1/events/consumers/billing"),
        ("DELETE", "/api/v1/events/consumers/billing"),
        ("GET", "/api/v1/events/stream?consumer=billing&partition=0"),
    ] {
        let response = send(&f, method, path, None, &[], None).await;
        assert_eq!(response.status().as_u16(), 401, "{method} {path}");
    }
    for (path, body) in [
        (
            "/api/v1/events/consumers/billing/pull",
            json!({"partition": 0}),
        ),
        (
            "/api/v1/events/consumers/billing/ack",
            json!({"partition": 0, "position": POSITION}),
        ),
        (
            "/api/v1/events/consumers/billing/restart",
            json!({"partition": 0, "from": {"kind": "tail"}}),
        ),
    ] {
        let response = send(&f, "POST", path, None, &[], Some(body)).await;
        assert_eq!(response.status().as_u16(), 401, "{path}");
    }
    let response = send(
        &f,
        "PUT",
        "/api/v1/events/consumers/billing",
        None,
        &[],
        Some(put_body()),
    )
    .await;
    assert_eq!(response.status().as_u16(), 401);
    assert!(
        f.fake.lock().calls.is_empty(),
        "the port was reached without a credential"
    );
}

#[tokio::test]
async fn get_and_delete_pass_only_the_name_and_the_credential() {
    let f = fixture();
    let r = call(&f, "GET", "/api/v1/events/consumers/billing", &[], None).await;
    assert_eq!(r.status, 200);
    assert_eq!(r.body["data"]["name"], "billing");
    assert_eq!(last_call(&f), ("get", "billing".to_owned()));
    let response = send(
        &f,
        "DELETE",
        "/api/v1/events/consumers/billing",
        Some(TOKEN),
        &[],
        None,
    )
    .await;
    assert_eq!(response.status().as_u16(), 204);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.is_empty());
    assert_eq!(f.fake.lock().tokens, vec![TOKEN.to_owned(); 2]);
}

#[tokio::test]
async fn an_unknown_consumer_is_404_and_a_refusal_is_403_with_the_port_error() {
    let f = fixture();
    fail_next(&f, ApiError::from_code(Code::EventstreamConsumerNotFound));
    let r = call(&f, "GET", "/api/v1/events/consumers/nobody", &[], None).await;
    assert_eq!(r.status, 404);
    assert_eq!(code(&r), "eventstream.consumer_not_found");
    fail_next(&f, ApiError::from_code(Code::PermissionDenied));
    let r = call(&f, "DELETE", "/api/v1/events/consumers/billing", &[], None).await;
    assert_eq!(r.status, 403);
    assert_eq!(code(&r), "permission.denied");
    fail_next(&f, ApiError::from_code(Code::PermissionDenied));
    let r = call(&f, "GET", "/api/v1/events/consumers", &[], None).await;
    assert_eq!(r.status, 403, "a denial is a denial, not an empty list");
}

#[tokio::test]
async fn a_name_that_is_not_an_identifier_never_reaches_the_port() {
    let f = fixture();
    for name in ["Billing", "9lives", "a-b", "x.y"] {
        let r = call(
            &f,
            "GET",
            &format!("/api/v1/events/consumers/{name}"),
            &[],
            None,
        )
        .await;
        assert_eq!(r.status, 400, "{name}");
        assert_eq!(code(&r), "api.invalid_request");
    }
    assert!(f.fake.lock().calls.is_empty());
}

// ------------------------------------------------------------------------------------------
// put

#[tokio::test]
async fn put_is_201_when_created_and_200_when_it_existed() {
    let f = fixture();
    f.fake.lock().created = true;
    let r = call(
        &f,
        "PUT",
        "/api/v1/events/consumers/billing",
        &[],
        Some(put_body()),
    )
    .await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert_eq!(r.body["data"]["name"], "billing");
    let (_, detail) = last_call(&f);
    assert!(detail.contains("billing.*"), "{detail}");
    assert!(detail.contains("Tail"), "{detail}");
    f.fake.lock().created = false;
    let r = call(
        &f,
        "PUT",
        "/api/v1/events/consumers/billing",
        &[],
        Some(put_body()),
    )
    .await;
    assert_eq!(r.status, 200);
}

#[tokio::test]
async fn put_refuses_a_malformed_definition_before_the_port() {
    let f = fixture();
    let mut cases: Vec<(Json, u16, &str)> = Vec::new();
    cases.push((
        json!({"topics": [], "partitions": 1, "payload_mode": "ids_only"}),
        422,
        "eventstream.spec_invalid",
    ));
    cases.push((
        json!({"topics": ["Bad Topic"], "partitions": 1, "payload_mode": "ids_only"}),
        422,
        "eventstream.spec_invalid",
    ));
    cases.push((
        json!({"topics": ["a"], "partitions": 0, "payload_mode": "ids_only"}),
        422,
        "eventstream.spec_invalid",
    ));
    cases.push((
        json!({"topics": ["a"], "partitions": 65, "payload_mode": "ids_only"}),
        422,
        "eventstream.spec_invalid",
    ));
    cases.push((json!({"topics": ["a"], "partitions": 1, "payload_mode": "ids_only", "filter": "x".repeat(2001)}), 422, "eventstream.spec_invalid"));
    cases.push((
        json!({"topics": ["*"], "partitions": 1, "payload_mode": "ids_only"}),
        422,
        "eventstream.spec_invalid",
    ));
    cases.push((
        json!({"topics": ["a"], "partitions": 1}),
        400,
        "api.invalid_request",
    ));
    cases.push((
        json!({"topics": ["a"], "partitions": 1, "payload_mode": "everything"}),
        400,
        "api.invalid_request",
    ));
    cases.push((
        json!({"topics": ["a"], "partitions": 1, "payload_mode": "ids_only", "owner": "x"}),
        400,
        "api.invalid_request",
    ));
    cases.push((
        json!({"topics": "a", "partitions": 1, "payload_mode": "ids_only"}),
        400,
        "api.invalid_request",
    ));
    cases.push((
        json!({"topics": ["a"], "partitions": 1, "payload_mode": "ids_only", "filter": 7}),
        400,
        "api.invalid_request",
    ));
    cases.push((json!({"topics": ["a"], "partitions": 1, "payload_mode": "ids_only", "start": {"kind": "middle"}}), 400, "api.invalid_request"));
    cases.push((json!({"topics": ["a"], "partitions": 1, "payload_mode": "ids_only", "start": {"kind": "tail", "position": POSITION}}), 400, "api.invalid_request"));
    cases.push((json!({"topics": ["a"], "partitions": 1, "payload_mode": "ids_only", "start": {"kind": "from_position", "position": "not-a-position"}}), 422, "eventstream.position_invalid"));
    cases.push((json!([1]), 400, "api.invalid_request"));
    for (body, status, expected) in cases {
        let r = call(
            &f,
            "PUT",
            "/api/v1/events/consumers/billing",
            &[],
            Some(body.clone()),
        )
        .await;
        assert_eq!((r.status, code(&r)), (status, expected), "{body}");
    }
    assert!(
        f.fake.lock().calls.is_empty(),
        "a malformed definition reached the port"
    );
}

#[tokio::test]
async fn put_passes_the_start_and_defaults_it_to_none() {
    let f = fixture();
    let mut body = put_body();
    body.as_object_mut().unwrap().remove("start");
    call(
        &f,
        "PUT",
        "/api/v1/events/consumers/billing",
        &[],
        Some(body.clone()),
    )
    .await;
    assert!(last_call(&f).1.contains("start: None"));
    body["start"] = json!({"kind": "from_position", "position": POSITION});
    call(
        &f,
        "PUT",
        "/api/v1/events/consumers/billing",
        &[],
        Some(body.clone()),
    )
    .await;
    assert!(last_call(&f).1.contains("FromPosition"));
    body["start"] = json!({"kind": "from_beginning"});
    call(
        &f,
        "PUT",
        "/api/v1/events/consumers/billing",
        &[],
        Some(body),
    )
    .await;
    assert!(last_call(&f).1.contains("FromBeginning"));
}

#[tokio::test]
async fn put_needs_a_json_body() {
    let f = fixture();
    let response = send(
        &f,
        "PUT",
        "/api/v1/events/consumers/billing",
        Some(TOKEN),
        &[(header::CONTENT_TYPE.as_str(), "text/plain")],
        None,
    )
    .await;
    assert_eq!(response.status().as_u16(), 415);
    let r = call(&f, "PUT", "/api/v1/events/consumers/billing", &[], None).await;
    assert_eq!(r.status, 400);
    assert_eq!(code(&r), "api.invalid_json");
}

#[tokio::test]
async fn put_reports_a_port_refusal_with_the_registry_status() {
    let f = fixture();
    for (error, status, expected) in [
        (Code::PermissionDenied, 403, "permission.denied"),
        (
            Code::EventstreamSpecInvalid,
            422,
            "eventstream.spec_invalid",
        ),
        (Code::ServerUnavailable, 503, "server.unavailable"),
    ] {
        fail_next(&f, ApiError::from_code(error));
        let r = call(
            &f,
            "PUT",
            "/api/v1/events/consumers/billing",
            &[],
            Some(put_body()),
        )
        .await;
        assert_eq!((r.status, code(&r)), (status, expected));
    }
}
