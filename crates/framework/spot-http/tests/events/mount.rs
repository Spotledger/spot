// ------------------------------------------------------------------------------------------
// the group and the contract

#[tokio::test]
async fn the_group_is_not_mounted_without_its_port() {
    let (_, report) = router_with_ports_missing();
    assert!(report.mounted.is_empty());
    assert_eq!(report.skipped.len(), 1);
}

fn router_with_ports_missing() -> (axum::Router, spot_http::api::MountReport) {
    let mounted =
        spot_http::api::router::routes::mount(&[events::registration()], &Ports::new(), &[]);
    (mounted.authenticated, mounted.report)
}

#[test]
fn the_group_serves_exactly_the_events_operations_of_the_contract() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../contracts/native-api/openapi.yaml"
    ))
    .unwrap();
    let root: Json = serde_norway::from_str(&text).unwrap();
    let mut contract: Vec<String> = Vec::new();
    for (path, item) in root["paths"].as_object().unwrap() {
        if !path.starts_with("/api/v1/events/") {
            continue;
        }
        for (method, op) in item.as_object().unwrap() {
            if ["get", "put", "post", "delete"].contains(&method.as_str()) {
                contract.push(op["operationId"].as_str().unwrap().to_owned());
            }
        }
    }
    contract.sort();
    let mut served: Vec<String> = events::SERVES
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    served.sort();
    assert_eq!(served, contract);
    // Every code the routes can answer with is in the contract's error table, with the status
    // the registry gives it.
    let codes = root["x-spot-error-codes"].as_object().unwrap();
    for (code, status) in [
        (Code::EventstreamConsumerNotFound, 404),
        (Code::EventstreamConsumerBusy, 409),
        (Code::EventstreamResyncRequired, 410),
        (Code::EventstreamWindowFull, 429),
        (Code::EventstreamPositionInvalid, 422),
        (Code::EventstreamAckRefused, 409),
        (Code::EventstreamSpecInvalid, 422),
        (Code::PermissionDenied, 403),
        (Code::ApiRateLimited, 429),
    ] {
        assert_eq!(codes[code.as_str()][0], status, "{}", code.as_str());
    }
    assert_eq!(
        root["info"]["version"].as_str().unwrap().split('.').next(),
        Some("0")
    );
}

#[test]
fn the_registered_set_includes_every_events_operation() {
    // The group is registered under the registry's standard names, so the conformance walk
    // covers its operations; the real host skips it (port absent) and mounts it itself.
    let registered = spot_http::api::router::routes::registry_serves();
    for id in events::SERVES {
        assert!(registered.contains(id), "{id} is not registered");
    }
    assert_eq!(events::SERVES.len(), 8);
}
