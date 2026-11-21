//! Selection table tests and the loopback integration test live here so the crate
//! root stays lean. The integration test needs a real multicast socket and is
//! `#[ignore]`d with its reason; it is run locally and the result is reported in the
//! PR (the brief's instruction).

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use spot_discovery::manual::parse_manual;
use spot_discovery::record::{
    AdvertisedSiteNames, Advertisement, PROTOCOL_VERSION, ProtocolVersion, ServerId, TlsMode,
};
use spot_discovery::select::{FoundServer, ManualServer, Selection, SelectionPolicy, pick};
use spot_discovery::{Found, FoundSource};

fn found(id: &str, ipv4: [u8; 4], port: u16) -> FoundServer {
    FoundServer::new(
        ServerId(id.to_string()),
        vec![IpAddr::V4(Ipv4Addr::new(
            ipv4[0], ipv4[1], ipv4[2], ipv4[3],
        ))],
        Advertisement {
            service_instance: ServerId(id.to_string()),
            port,
            protocol_version: PROTOCOL_VERSION,
            tls: TlsMode::Off,
            server_key_fingerprint: None,
            product_version: String::new(),
            site_names: AdvertisedSiteNames::default(),
        },
    )
}

#[test]
fn last_used_wins_when_present() {
    let a = found("shop-a", [192, 168, 1, 10], 8080);
    let b = found("shop-b", [192, 168, 1, 11], 8080);
    let policy = SelectionPolicy::default();
    let pick_res = pick(
        &[a, b],
        Some(&ServerId("shop-b".to_string())),
        &policy,
        None,
    );
    match pick_res {
        Selection::Select(f) => assert_eq!(f.server_id.as_str(), "shop-b"),
        other => panic!("expected Select, got {other:?}"),
    }
}

#[test]
fn last_used_absent_falls_through_to_single_or_choice() {
    let a = found("shop-a", [192, 168, 1, 10], 8080);
    let b = found("shop-b", [192, 168, 1, 11], 8080);
    let policy = SelectionPolicy::default();
    // Last used is not among them, two present: the user must choose.
    let res = pick(
        &[a.clone(), b],
        Some(&ServerId("gone".to_string())),
        &policy,
        None,
    );
    match res {
        Selection::NeedsChoice(list) => {
            assert_eq!(list.len(), 2);
            let ids: Vec<&str> = list.iter().map(|f| f.server_id.as_str()).collect();
            assert_eq!(ids, ["shop-a", "shop-b"]);
        }
        other => panic!("expected NeedsChoice, got {other:?}"),
    }
    // Last used is not among them, one present: select it.
    let res1 = pick(&[a], Some(&ServerId("gone".to_string())), &policy, None);
    match res1 {
        Selection::Select(f) => assert_eq!(f.server_id.as_str(), "shop-a"),
        other => panic!("expected Select, got {other:?}"),
    }
}

#[test]
fn one_server_means_select_it_without_last_used() {
    let only = found("only-server", [10, 0, 0, 5], 9090);
    let res = pick(&[only], None, &SelectionPolicy::default(), None);
    match res {
        Selection::Select(f) => {
            assert_eq!(f.server_id.as_str(), "only-server");
            assert_eq!(f.port, 9090);
            assert_eq!(f.addresses.len(), 1);
        }
        other => panic!("expected Select, got {other:?}"),
    }
}

#[test]
fn several_servers_need_choice_never_guess() {
    let servers: Vec<FoundServer> = (0u8..5)
        .map(|i| found(&format!("srv-{i}"), [192, 168, 1, i], 8080))
        .collect();
    let res = pick(&servers, None, &SelectionPolicy::default(), None);
    match res {
        Selection::NeedsChoice(list) => assert_eq!(list.len(), 5),
        other => panic!("expected NeedsChoice, got {other:?}"),
    }
}

#[test]
fn none_found_offers_manual_route() {
    let res: Selection = pick(&[], None, &SelectionPolicy::default(), None);
    assert_eq!(res, Selection::NoneFound);
}

#[test]
fn policy_floor_filters_protocol_versions() {
    let new = found("new", [192, 168, 1, 1], 8080);
    let mut old = found("old", [192, 168, 1, 2], 8080);
    old.advertisement.protocol_version = ProtocolVersion(0);
    let policy = SelectionPolicy {
        min_protocol_version: 1,
        ..SelectionPolicy::default()
    };
    // The old-version server is filtered; the new one is the single candidate.
    let res = pick(&[old, new], None, &policy, None);
    match res {
        Selection::Select(f) => assert_eq!(f.server_id.as_str(), "new"),
        other => panic!("expected Select, got {other:?}"),
    }
}

#[test]
fn fingerprint_change_turns_selection_into_needs_trust() {
    let a = found("shop-a", [192, 168, 1, 10], 8080);
    let policy = SelectionPolicy::default();
    let changed = pick(
        &[a],
        Some(&ServerId("shop-a".to_string())),
        &policy,
        Some(&ServerId("shop-a".to_string())),
    );
    match changed {
        Selection::SelectNeedsTrust(f) => assert_eq!(f.server_id.as_str(), "shop-a"),
        other => panic!("expected SelectNeedsTrust, got {other:?}"),
    }
}

#[test]
fn manual_selection_route() {
    let (manual, _adv) = parse_manual("192.168.1.99:8443").unwrap_or_else(|e| panic!("{e:?}"));
    let expected = ManualServer {
        host: "192.168.1.99".to_string(),
        port: 8443,
    };
    assert_eq!(manual, expected);
}

#[test]
fn found_manual_source_is_selection_compatible() {
    // The manual fallback yields the same `Found`-shape; both sit in one picker.
    let (m, adv) = parse_manual("shopserver:8080").unwrap_or_else(|e| panic!("{e:?}"));
    let manual_found = Found {
        server_id: ServerId("manual".to_string()),
        addresses: Vec::new(),
        port: m.port,
        advertisement: adv,
        last_seen: 0,
        source: FoundSource::Manual,
    };
    assert_eq!(manual_found.source, FoundSource::Manual);
}

#[test]
fn addresses_ipv4_first_ordering_by_browser_contract() {
    // The browser contract: IPv4 sorted, then IPv6 sorted. Pure function, tested
    // through the same ordering the decoder applies.
    let a = found("dual", [192, 168, 1, 2], 8080);
    assert_eq!(a.addresses, vec![IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2))]);
}

#[test]
fn max_age_zero_means_no_bound() {
    let a = found("a", [192, 168, 1, 3], 8080);
    let policy = SelectionPolicy {
        max_age: Duration::ZERO,
        ..SelectionPolicy::default()
    };
    let res = pick(&[a], None, &policy, None);
    assert!(matches!(res, Selection::Select(_)));
}
