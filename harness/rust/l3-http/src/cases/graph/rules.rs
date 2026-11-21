//! Rule sets of an app: a good set loads, a set that does not compile arrives by a reload and is
//! named in readiness (never its text) while the site stays in rotation, and a fixed set clears
//! the report. The derivation itself has no route (see the blocked case in `graph.rs`).

use std::time::{Duration, Instant};

use super::host::{FAST, GraphHost, Start, write_rules};
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

const GOOD: &str = "approvers_of(R, O) :- chain_receipt.source(R, O).\n";
/// `Z` is bound by no positive atom: not range-restricted.
const BAD: &str = "broken(R, Z) :- chain_receipt.source(R, O).\n";

pub async fn a_bad_rule_set_keeps_the_old_one_and_names_itself() -> CaseResult {
    let mut host = GraphHost::start(Start {
        graph: FAST,
        rules: &[("approvers", GOOD)],
        ..Start::default()
    })
    .await?;
    let result = run(&host).await;
    host.finish().await;
    result
}

async fn readiness(host: &GraphHost) -> Result<(u16, String), Failure> {
    let reply = host.anonymous(host.site()).health("ready").await?;
    Ok((reply.status, reply.text()))
}

/// Polls readiness until `want` holds of its body (30 s); the site is in rotation (200) at every
/// look.
async fn until(
    host: &GraphHost,
    what: &str,
    want: impl Fn(&str) -> bool,
) -> Result<String, Failure> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let (status, body) = readiness(host).await?;
        ensure!(
            status == 200,
            "a degraded rule set took the site out of rotation (HTTP {status}): {body}"
        );
        if want(&body) {
            return Ok(body);
        }
        ensure!(
            Instant::now() < deadline,
            "readiness never showed {what}: {body}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn run(host: &GraphHost) -> CaseResult {
    let (status, body) = readiness(host).await?;
    ensure!(
        status == 200 && !body.contains("did not compile"),
        "a good rule set degrades readiness: HTTP {status}: {body}"
    );

    // A bad set arrives with a reload: it is named, the good one is not, and no rule text shows.
    write_rules(host.fixture.root(), "broken", BAD)?;
    tokio::task::block_in_place(|| spot_cli::request_reload_and_wait(host.fixture.root(), host.site()))
        .map_err(|e| failure!("cannot request a reload: {e}"))?;
    let body = until(host, "the set that does not compile", |b| {
        b.contains("did not compile")
    })
    .await?;
    ensure!(body.contains("broken"), "the bad set is not named: {body}");
    ensure!(
        !body.contains("approvers") && !body.contains("chain_receipt"),
        "readiness names the good set or leaks a predicate: {body}"
    );

    // The set is fixed and the report clears.
    write_rules(host.fixture.root(), "broken", GOOD)?;
    tokio::task::block_in_place(|| spot_cli::request_reload_and_wait(host.fixture.root(), host.site()))
        .map_err(|e| failure!("cannot request a reload: {e}"))?;
    until(host, "no failing rule set", |b| {
        !b.contains("did not compile")
    })
    .await?;
    Ok(())
}
