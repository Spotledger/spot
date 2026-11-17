//! The graph projection over a real `spot serve` on real PostgreSQL (skipped, with no output,
//! when `SPOT_TEST_PG_URL` is unset): off by default it creates nothing; enabled with an endpoint
//! the service refuses (not https, not on the allow-list, no secret) it stays off while the site
//! serves, and a second site of the same server is unaffected. The byte content of a pushed batch
//! and the settings are unit-tested in `services/graph_projection.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use std::net::SocketAddr;
use std::time::Duration;

use serde_json::{Value as Json, json};
use spot_cli::Options;
use spot_login::LoginConfig;
use spot_storage_contract::RecordId;
use support::Env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn post(addr: SocketAddr, host: &str, path: &str, body: Json) -> (u16, Json) {
    let body = body.to_string();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let raw = String::from_utf8(bytes).unwrap();
    let (headers, payload) = raw.split_once("\r\n\r\n").unwrap();
    (
        headers.split_whitespace().nth(1).unwrap().parse().unwrap(),
        serde_json::from_str(payload).unwrap_or(Json::Null),
    )
}

fn secret_of(output: &str) -> String {
    output.trim().rsplit(' ').next().unwrap().to_owned()
}

fn cursor_of(env: &Env, site: &str) -> std::path::PathBuf {
    env.root
        .join("sites")
        .join(site)
        .join("graph_projection.cursor")
}

/// Starts a server over `sites` sites with `table` appended to `common.toml` (the text of a
/// `[services.graph_projection]` table, or empty), waits, and checks that every site serves
/// (the Administrator can sign in) and none holds a cursor. Returns `None` when skipped.
async fn run_case(table: &str, extra_sites: usize) -> Option<()> {
    let env = Env::new().await?;
    if !table.is_empty() {
        let common = env.root.join("common.toml");
        let mut text = std::fs::read_to_string(&common).unwrap();
        text.push_str(table);
        std::fs::write(&common, text).unwrap();
    }
    let mut sites = vec![(
        env.site.clone(),
        secret_of(&env.ok(&["new-site", &env.site])),
    )];
    for _ in 0..extra_sites {
        let name = format!(
            "spot-test-{}",
            RecordId::new_v7().to_string().replace('-', "")
        );
        let key = secret_of(&env.ok(&["new-site", &name]));
        sites.push((name, key));
    }
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let server = spot_cli::start_with(
        &env.root,
        false,
        Options {
            clock: None,
            login: LoginConfig::for_tests(),
        },
        async {
            let _ = stopped.await;
        },
    )
    .await
    .unwrap();
    let addr = server.local_addrs()[0];
    tokio::time::sleep(Duration::from_millis(2_500)).await;
    for (site, key) in &sites {
        let (status, answer) = post(
            addr,
            site,
            "/api/v1/auth/login",
            json!({"user": "administrator", "password": key}),
        )
        .await;
        assert_eq!(status, 200, "{site} must serve: {answer}");
        assert!(
            !cursor_of(&env, site).exists(),
            "{site}: a projection that is off or refused writes no cursor"
        );
    }
    let _ = stop.send(());
    let _ = server.join().await;
    Some(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_by_default_creates_nothing() {
    let _ = run_case("", 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_explicit_off_creates_nothing_even_with_an_endpoint() {
    let table = "[services.graph_projection]\nenabled = false\nendpoint = \"https://graph.example.com/in\"\nallowed_hosts = [\"graph.example.com\"]\ndatabase = \"g\"\nsecret = \"engine\"\n";
    let _ = run_case(table, 0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_endpoint_the_service_refuses_leaves_it_off_and_the_site_serving() {
    let cases = [
        // Not https.
        "enabled = true\nendpoint = \"http://graph.example.com/in\"\nallowed_hosts = [\"graph.example.com\"]\ndatabase = \"g\"\nsecret = \"engine\"\n",
        // https, but the host is not on the allow-list.
        "enabled = true\nendpoint = \"https://graph.example.com/in\"\nallowed_hosts = [\"other.example.com\"]\ndatabase = \"g\"\nsecret = \"engine\"\n",
        // An empty allow-list.
        "enabled = true\nendpoint = \"https://graph.example.com/in\"\ndatabase = \"g\"\nsecret = \"engine\"\n",
        // No endpoint at all.
        "enabled = true\nallowed_hosts = [\"graph.example.com\"]\ndatabase = \"g\"\nsecret = \"engine\"\n",
        // An allow-listed endpoint but no secret reference.
        "enabled = true\nendpoint = \"https://graph.example.com/in\"\nallowed_hosts = [\"graph.example.com\"]\ndatabase = \"g\"\n",
    ];
    for body in cases {
        let table = format!("[services.graph_projection]\n{body}");
        if run_case(&table, 0).await.is_none() {
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusing_configuration_does_not_touch_a_second_site() {
    let table = "[services.graph_projection]\nenabled = true\nendpoint = \"http://graph.example.com/in\"\nallowed_hosts = [\"graph.example.com\"]\ndatabase = \"g\"\nsecret = \"engine\"\n";
    let _ = run_case(table, 1).await;
}
