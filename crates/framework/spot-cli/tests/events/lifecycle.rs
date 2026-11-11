#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consumers_pull_ack_restart_and_survive_a_restart() {
    let Some(mut s) = Stack::start(&[]).await else {
        return;
    };
    let path = "/api/v1/events/consumers/ledger";
    // A consumer registers, is told apart from an identical re-registration, and is listed.
    let body = spec(ROLES_ONLY, json!({"kind": "tail"}));
    let created = s.admin("PUT", path, Some(body.clone())).await;
    assert_eq!(created.status, 201, "{:?}", created.body);
    assert_eq!(created.body["data"]["name"], "ledger");
    assert_eq!(created.body["data"]["partitions_state"][0]["parked"], false);
    let again = s.admin("PUT", path, Some(body)).await;
    assert_eq!(again.status, 200, "{:?}", again.body);
    let other = s
        .admin(
            "PUT",
            path,
            Some(
                spec(ROLES_ONLY, json!({"kind": "tail"}))
                    .as_object()
                    .cloned()
                    .map(|mut m| {
                        m.insert("partitions".into(), json!(2));
                        Json::Object(m)
                    })
                    .unwrap(),
            ),
        )
        .await;
    assert_eq!(
        (other.status, other.code()),
        (422, "eventstream.spec_invalid")
    );
    let listed = s.admin("GET", "/api/v1/events/consumers", None).await;
    assert_eq!(listed.status, 200);
    assert!(
        listed.body["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "ledger")
    );
    assert_eq!(s.admin("GET", path, None).await.status, 200);

    // Events written after the consumer started arrive in order; earlier ones do not (tail).
    let made: Vec<String> = vec![
        s.make_role("Alpha").await,
        s.make_role("Bravo").await,
        s.make_role("Charlie").await,
    ];
    let pull_path = format!("{path}/pull");
    let first = s.admin("POST", &pull_path, Some(pull_body())).await;
    assert_eq!(first.status, 200, "{:?}", first.body);
    let lease = first.header("x-consumer-lease").expect("a lease");
    assert_eq!(subjects(&first), made, "events in order, tail only");
    assert_eq!(first.body["data"]["window_full"], false);
    // A second pull without an ack returns the same events with the same ids.
    let second = s
        .call(
            &s.admin,
            "POST",
            &pull_path,
            &[("x-consumer-lease", &lease)],
            Some(pull_body()),
        )
        .await;
    assert_eq!(second.status, 200, "{:?}", second.body);
    assert_eq!(event_ids(&second), event_ids(&first));
    assert_eq!(
        second.body["data"]["next_position"],
        first.body["data"]["next_position"]
    );
    // Without the lease, while it is live, the consumer is busy.
    let busy = s.admin("POST", &pull_path, Some(pull_body())).await;
    assert_eq!(
        (busy.status, busy.code()),
        (409, "eventstream.consumer_busy")
    );

    // Only an ack moves the checkpoint; a stale, a forged and a foreign position are refused.
    let ack_path = format!("{path}/ack");
    let head = first.body["data"]["next_position"]
        .as_str()
        .unwrap()
        .to_owned();
    let lease_header = [("x-consumer-lease", lease.as_str())];
    let forged = s
        .call(
            &s.admin,
            "POST",
            &ack_path,
            &lease_header,
            Some(json!({"partition": 0, "position": "v1.bm90LWEtcG9zaXRpb24"})),
        )
        .await;
    assert_eq!(
        (forged.status, forged.code()),
        (422, "eventstream.position_invalid")
    );
    // A well-formed position of this consumer that was never delivered cannot be acknowledged:
    // positions are opaque but not secret, so only what was delivered moves a checkpoint.
    for generation in [0, 1, 2] {
        let future = s
            .call(
                &s.admin,
                "POST",
                &ack_path,
                &lease_header,
                Some(json!({
                    "partition": 0,
                    "position": position("ledger", generation, 999_999_999)
                })),
            )
            .await;
        assert_eq!(
            (future.status, future.code()),
            (409, "eventstream.ack_refused"),
            "generation {generation}: {:?}",
            future.body
        );
    }
    let no_lease = s
        .admin(
            "POST",
            &ack_path,
            Some(json!({"partition": 0, "position": head})),
        )
        .await;
    assert_eq!(
        (no_lease.status, no_lease.code()),
        (409, "eventstream.ack_refused")
    );
    let acked = s
        .call(
            &s.admin,
            "POST",
            &ack_path,
            &lease_header,
            Some(json!({"partition": 0, "position": head})),
        )
        .await;
    assert_eq!(acked.status, 200, "{:?}", acked.body);
    assert_eq!(acked.body["data"]["position"], head);
    let repeated = s
        .call(
            &s.admin,
            "POST",
            &ack_path,
            &lease_header,
            Some(json!({"partition": 0, "position": head})),
        )
        .await;
    assert_eq!(
        repeated.status, 200,
        "repeating an ack of the checkpoint changes nothing"
    );
    let after = s
        .call(
            &s.admin,
            "POST",
            &pull_path,
            &lease_header,
            Some(pull_body()),
        )
        .await;
    assert!(
        event_ids(&after).is_empty(),
        "acknowledged events are not delivered again"
    );

    // A restart from the beginning replays; a restart without the lease is refused. The
    // checkpoint it answers is a position that is behind the next ack.
    let restart_path = format!("{path}/restart");
    let refused = s
        .admin(
            "POST",
            &restart_path,
            Some(json!({"partition": 0, "from": {"kind": "from_beginning"}})),
        )
        .await;
    assert_eq!(
        (refused.status, refused.code()),
        (409, "eventstream.consumer_busy")
    );
    let restarted = s
        .call(
            &s.admin,
            "POST",
            &restart_path,
            &lease_header,
            Some(json!({"partition": 0, "from": {"kind": "from_beginning"}})),
        )
        .await;
    assert_eq!(restarted.status, 200, "{:?}", restarted.body);
    let start_position = restarted.body["data"]["position"]
        .as_str()
        .unwrap()
        .to_owned();
    let replay = s
        .call(
            &s.admin,
            "POST",
            &pull_path,
            &lease_header,
            Some(pull_body()),
        )
        .await;
    for id in &made {
        assert!(subjects(&replay).contains(id), "the replay holds {id}");
    }
    let behind = s
        .call(
            &s.admin,
            "POST",
            &ack_path,
            &lease_header,
            Some(json!({"partition": 0, "position": start_position})),
        )
        .await;
    assert_eq!(
        behind.status, 200,
        "an ack of the checkpoint itself is accepted"
    );

    // Kill the server between a pull and its ack: the checkpoint is durable and the same
    // events are delivered again, so nothing is lost.
    let pending = replay.body["data"]["next_position"]
        .as_str()
        .unwrap()
        .to_owned();
    let _ = pending;
    s.restart_host().await;
    let resumed = s.admin("POST", &pull_path, Some(pull_body())).await;
    assert_eq!(resumed.status, 200, "{:?}", resumed.body);
    let new_lease = resumed.header("x-consumer-lease").expect("a lease");
    for id in &made {
        assert!(subjects(&resumed).contains(id), "{id} survived the restart");
    }
    let next = resumed.body["data"]["next_position"]
        .as_str()
        .unwrap()
        .to_owned();
    let done = s
        .call(
            &s.admin,
            "POST",
            &ack_path,
            &[("x-consumer-lease", &new_lease)],
            Some(json!({"partition": 0, "position": next})),
        )
        .await;
    assert_eq!(done.status, 200, "{:?}", done.body);

    // Deleting removes the consumer for good.
    assert_eq!(s.admin("DELETE", path, None).await.status, 204);
    let gone = s.admin("GET", path, None).await;
    assert_eq!(
        (gone.status, gone.code()),
        (404, "eventstream.consumer_not_found")
    );
    s.finish().await;
}
