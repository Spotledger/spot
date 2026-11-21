//! Debugging and tracing for `spot serve` in development (WP-DX-03.T3): the parts that need no
//! database. The PostgreSQL scenarios of the brief (one trace across HTTP, a script step and
//! storage of a real request; a DAP session with a source-mapped breakpoint; a second request
//! served while the first is paused) need the real host and a script engine and are listed in
//! the report as not yet written.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use spot_cli::services::SiteCtx;
use spot_cli::services::devtools::server::{TOKEN_HEADER, serve_traces};
use spot_cli::services::devtools::{Settings, register, resolve, status_file};
use spot_core::{ManualClock, SiteId};
use spot_devtools::{DebugPolicy, DevApi, DevToken, Layer, TraceCollector};

const TOKEN: &str = "dev-token-0123456789";

fn ctx_with(settings: BTreeMap<String, toml::Value>) -> SiteCtx<'static> {
    let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
    SiteCtx::new("acme", Duration::from_secs(1), clock, settings, true)
}

#[test]
fn a_site_that_does_not_ask_for_devtools_registers_nothing() {
    for settings in [
        BTreeMap::new(),
        BTreeMap::from([(
            "devtools".to_owned(),
            toml::Value::Table(toml::from_str("enabled = false").unwrap()),
        )]),
    ] {
        let mut ctx = ctx_with(settings);
        register(&mut ctx).expect("a disabled devtools module registers cleanly");
        let assembled = ctx.finish(axum::Router::new());
        let services = assembled.assembly.expect("an assembling build");
        assert!(services.services().is_empty());
    }
}

#[test]
fn an_unknown_setting_is_refused() {
    let settings = BTreeMap::from([(
        "devtools".to_owned(),
        toml::Value::Table(toml::from_str("enabld = true").unwrap()),
    )]);
    let mut ctx = ctx_with(settings);
    assert!(register(&mut ctx).is_err());
}

#[test]
fn production_and_non_loopback_and_missing_token_are_all_refused() {
    let site = SiteId::new("acme").unwrap();
    let local = ["127.0.0.1:8080".parse().unwrap()];
    let production = Settings {
        production: true,
        ..Settings::default()
    };
    assert!(resolve(&production, &site, &local, Some(TOKEN)).is_err());
    let public = ["0.0.0.0:8080".parse().unwrap()];
    assert!(resolve(&Settings::default(), &site, &public, Some(TOKEN)).is_err());
    assert!(resolve(&Settings::default(), &site, &local, None).is_err());
    assert!(resolve(&Settings::default(), &site, &local, Some(TOKEN)).is_ok());
}

#[test]
fn the_status_file_lives_under_the_site_directory() {
    let path = status_file(std::path::Path::new("/srv/spot"), "acme");
    assert!(path.ends_with("sites/acme/devtools.json"));
}

fn request(address: std::net::SocketAddr, target: &str, token: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    let header = token.map_or_else(String::new, |t| format!("{TOKEN_HEADER}: {t}\r\n"));
    write!(stream, "GET {target} HTTP/1.1\r\nHost: x\r\n{header}\r\n").unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let status = raw.split_whitespace().nth(1).unwrap().parse().unwrap();
    let body = raw
        .split_once("\r\n\r\n")
        .map_or(String::new(), |(_, b)| b.to_owned());
    (status, body)
}

#[test]
fn the_viewer_needs_the_token_hides_a_production_site_and_serves_a_trace() {
    let collector = TraceCollector::new(SiteId::new("acme").unwrap(), 10);
    let root = collector.root(
        Layer::Http,
        "GET /api/v1/x",
        [("request_id".to_owned(), "r-7".to_owned())],
        Duration::from_millis(1),
    );
    root.end(Duration::from_millis(3));
    let api = DevApi::new(collector);
    let policy = DebugPolicy::new(DevToken::new(TOKEN).unwrap());

    for production in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (api, policy, stop) = (api.clone(), policy.clone(), Arc::clone(&stop));
            std::thread::spawn(move || serve_traces(&listener, &api, &policy, production, &stop))
        };
        let (status, _) = request(address, "/dev/traces", None);
        let (wrong, _) = request(address, "/dev/traces", Some("not-the-token-at-all"));
        let (ok, body) = request(address, "/dev/traces?request_id=r-7", Some(TOKEN));
        let (unknown, _) = request(address, "/elsewhere", Some(TOKEN));
        stop.store(true, Ordering::Relaxed);
        thread.join().unwrap();
        if production {
            // A production site has no such route, token or not.
            assert_eq!((status, wrong, ok, unknown), (404, 404, 404, 404));
        } else {
            assert_eq!((status, wrong, ok, unknown), (401, 401, 200, 404));
            assert!(body.contains("r-7"), "{body}");
        }
    }
}
