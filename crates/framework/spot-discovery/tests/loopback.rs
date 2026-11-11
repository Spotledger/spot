//! The loopback integration test: an `Advertiser` and a `Browser` on the same machine
//! find each other.
//!
//! `#[ignore]` with reason: CI runners frequently have no multicast ability (and some
//! forbid binding UDP 5353), and a red build from missing infrastructure would be a
//! false signal. The command below runs it locally where multicast exists; the
//! result is reported in the PR per the task brief.
//!
//! Locally: `cargo test -p spot-discovery --test loopback -- --ignored`

use std::time::{Duration, Instant};

use spot_discovery::advertiser::Advertiser;
use spot_discovery::browser::Browser;
use spot_discovery::record::{
    AdvertisedSiteNames, Advertisement, Fingerprint, PROTOCOL_VERSION, ServerId, TlsMode,
};

fn advertisement() -> Advertisement {
    Advertisement {
        service_instance: ServerId("loopback-test".to_string()),
        port: 45_678,
        protocol_version: PROTOCOL_VERSION,
        tls: TlsMode::SelfSigned,
        server_key_fingerprint: Some(Fingerprint("ab".repeat(16))),
        product_version: "0.1.0".to_string(),
        site_names: AdvertisedSiteNames::default(),
    }
}

#[test]
#[ignore = "needs a machine with working mDNS multicast (CI runners often lack it); \
            run locally with --ignored and report the result in the PR"]
fn advertiser_and_browser_find_each_other_within_5_seconds() {
    let handle = Advertiser::start(spot_discovery::record::AdvertiserConfig::default_for_test(
        advertisement(),
    ))
    .unwrap_or_else(|e| panic!("advertiser failed to start: {e:?}"));

    let deadline = Instant::now() + Duration::from_secs(4);
    let mut seen = None;
    while Instant::now() < deadline && seen.is_none() {
        let found = Browser::scan(Duration::from_millis(600))
            .unwrap_or_else(|e| panic!("scan failed: {e:?}"));
        if let Some(f) = found
            .into_iter()
            .find(|f| f.server_id.as_str() == "loopback-test")
        {
            seen = Some(f);
        }
    }

    let found = seen.unwrap_or_else(|| panic!("the advertiser was not found within 5 seconds"));
    assert_eq!(found.port, 45_678);
    assert_eq!(found.advertisement.tls, TlsMode::SelfSigned);
    assert_eq!(
        found
            .advertisement
            .server_key_fingerprint
            .as_ref()
            .map(|f| f.0.clone()),
        Some("ab".repeat(16))
    );
    assert!(found.last_seen <= 5_000, "last_seen must be a fresh stamp");

    // Goodbye: after stop + a short browse, the server must disappear.
    handle.stop();
    std::thread::sleep(Duration::from_millis(400));
    let after =
        Browser::scan(Duration::from_millis(800)).unwrap_or_else(|e| panic!("scan failed: {e:?}"));
    assert!(
        !after
            .iter()
            .any(|f| f.server_id.as_str() == "loopback-test"),
        "the server must disappear after the goodbye"
    );
}

#[test]
#[ignore = "needs a machine with working mDNS multicast; run locally with --ignored"]
fn watch_stream_reports_appeared_then_disappeared() {
    let handle = Advertiser::start(spot_discovery::record::AdvertiserConfig::default_for_test(
        advertisement(),
    ))
    .unwrap_or_else(|e| panic!("advertiser failed to start: {e:?}"));

    let (mut watch, rx) = Browser::watch().unwrap_or_else(|e| panic!("watch failed: {e:?}"));
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut appeared = false;
    while Instant::now() < deadline && !appeared {
        if let Ok(ev) = rx.try_recv()
            && matches!(ev, spot_discovery::FoundEvent::Appeared(_))
        {
            appeared = true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(appeared, "no Appeared event within 5 seconds");

    handle.stop();
    let mut gone = false;
    let until = Instant::now() + Duration::from_secs(3);
    while Instant::now() < until && !gone {
        if let Ok(ev) = rx.try_recv()
            && matches!(ev, spot_discovery::FoundEvent::Disappeared { .. })
        {
            gone = true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    watch.stop();
    assert!(gone, "no Disappeared event after the goodbye");
}
