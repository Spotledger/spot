//! The rule graph hosted over a real `spot serve` on real PostgreSQL (skipped, with no output,
//! when `SPOT_TEST_PG_URL` is unset): a site with no rule sets, with a good one and with a bad
//! one starts and serves; readiness names a set that did not compile (by name, never its text)
//! and stays in rotation. The port itself is in-process only (no route reaches it), so the
//! derivation, the principal filter, the limits and the resolver are unit-tested in
//! `services/rule_graph.rs`.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod support;
use std::net::SocketAddr;
use std::time::Duration;

use spot_cli::Options;
use spot_login::LoginConfig;
use spot_storage_contract::RecordId;
use support::Env;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const GOOD: &str = "open(T, P) :- task.project(T, P).\n";
/// `Z` is not bound by a positive atom: not range-restricted.
const UNSAFE: &str = "broken(T, Z) :- task.project(T, P).\n";
/// A predicate the metadata does not have: the text names it, the readiness must not.
const UNKNOWN: &str = "nonesuch(T) :- no_such_doctype.no_such_link(T, _).\n";

async fn get(addr: SocketAddr, host: &str, path: &str) -> (u16, String) {
    let head = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let mut bytes = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut bytes))
        .await
        .unwrap()
        .unwrap();
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let (headers, payload) = raw.split_once("\r\n\r\n").unwrap();
    (
        headers.split_whitespace().nth(1).unwrap().parse().unwrap(),
        payload.to_owned(),
    )
}

/// Runs a server over `sites` sites whose sample app ships the rule files `files`
/// (`(file stem, text)`), and returns each site's readiness (status, body). `None` when skipped.
async fn readiness(files: &[(&str, &str)], sites: usize) -> Option<Vec<(u16, String)>> {
    let env = Env::new().await?;
    if !files.is_empty() {
        let dir = env.root.join("sample/rules");
        std::fs::create_dir_all(&dir).unwrap();
        for (stem, text) in files {
            std::fs::write(dir.join(format!("{stem}.rules")), text).unwrap();
        }
    }
    let mut names = vec![env.site.clone()];
    env.ok(&["new-site", &env.site]);
    for _ in 1..sites {
        let name = format!(
            "spot-test-{}",
            RecordId::new_v7().to_string().replace('-', "")
        );
        env.ok(&["new-site", &name]);
        names.push(name);
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
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let mut out = Vec::new();
    for name in &names {
        out.push(get(addr, name, "/api/health/ready").await);
    }
    let _ = stop.send(());
    let _ = server.join().await;
    Some(out)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_site_with_no_rule_sets_starts_and_serves() {
    let Some(ready) = readiness(&[], 1).await else {
        return;
    };
    assert_eq!(ready[0].0, 200, "{}", ready[0].1);
    assert!(!ready[0].1.contains("did not compile"), "{}", ready[0].1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_that_compiles_leaves_the_site_healthy() {
    let Some(ready) = readiness(&[("open_tasks", GOOD)], 1).await else {
        return;
    };
    assert_eq!(ready[0].0, 200, "{}", ready[0].1);
    assert!(!ready[0].1.contains("did not compile"), "{}", ready[0].1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_set_that_does_not_compile_degrades_readiness_by_name_and_the_site_stays_in_rotation() {
    let Some(ready) = readiness(
        &[
            ("open_tasks", GOOD),
            ("broken", UNSAFE),
            ("nonesuch", UNKNOWN),
        ],
        1,
    )
    .await
    else {
        return;
    };
    let (status, body) = &ready[0];
    assert_eq!(*status, 200, "a degraded site stays in rotation: {body}");
    assert!(body.contains("did not compile"), "{body}");
    assert!(
        body.contains("broken") && body.contains("nonesuch"),
        "{body}"
    );
    // The good set is not named; the rule text and the predicates it reads never are.
    assert!(!body.contains("open_tasks"), "{body}");
    assert!(!body.contains("no_such_doctype"), "{body}");
    assert!(!body.contains("task.project"), "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_site_of_a_server_reports_its_own_rule_sets() {
    let Some(ready) = readiness(&[("broken", UNSAFE)], 2).await else {
        return;
    };
    assert_eq!(ready.len(), 2);
    for (status, body) in &ready {
        assert_eq!(*status, 200, "{body}");
        assert!(body.contains("broken"), "{body}");
    }
}
