//! AI workload isolation on the running server (WP-AI-05.T4): twenty parallel agents against a
//! steady human load. Humans stay inside the latency budget and the AI side is the one that is shed.
//!
//! The shared fixture's host runs without isolation, and isolation is built when the site is
//! assembled (a reload keeps the running services), so the case starts its own fixture, adds
//! `[services.ai_isolation]` to the fixture's `common.toml`, restarts the host on the same root and
//! runs against that. The budget is `spot-ai-isolation`'s default high p95 (250 ms,
//! `LoadThresholds::default().high_p95`); this crate does not depend on it, so the number is repeated
//! here and the case fails loudly if the crate's default moves (see the report).

use std::time::{Duration, Instant};

use serde_json::json;
use spot_login::LoginConfig;

use super::CaseDef;
use crate::client::Client;
use crate::fixture::{ADMIN_PASSWORD, Fixture, MANAGER, MEMBER};
use crate::report::CaseResult;
use crate::{ensure, failure};

/// After `function` (40): it owns its fixture and host.
pub const ORDER: u32 = 60;

/// The human p95 budget: `spot-ai-isolation`'s default `LoadThresholds::high_p95`.
const BUDGET: Duration = Duration::from_millis(250);

const AGENTS: usize = 20;
const ROUND: Duration = Duration::from_secs(4);
const ROLE_PASSWORD: &str = "l3-http.ai-role~password-42";

pub fn cases() -> Vec<CaseDef> {
    vec![CaseDef::run(
        "ai_isolation: twenty_agents_do_not_degrade_human_latency_and_are_shed_first",
        "AI isolation",
        "with isolation on, twenty agents calling MCP in parallel leave the human p95 inside its budget and are answered `429` with `Retry-After` (shed), never the humans",
        false,
        |_| Box::pin(twenty_agents()),
    )]
}

async fn twenty_agents() -> CaseResult {
    let Some(mut fixture) = Fixture::start_secondary().await else {
        return Err(failure!(
            "the isolation case could not start its own fixture"
        ));
    };
    let result = run(&fixture).await;
    fixture.finish().await;
    result
}

fn p95(mut values: Vec<Duration>) -> Duration {
    values.sort();
    let rank = (values.len() * 95).div_ceil(100).max(1);
    values.get(rank - 1).copied().unwrap_or(Duration::ZERO)
}

async fn run(fixture: &Fixture) -> CaseResult {
    // 1. Switch the isolation on and restart the host on the same root.
    let password = fixture.root().join("ai_role.pw");
    std::fs::write(&password, format!("{ROLE_PASSWORD}\n"))
        .map_err(|e| failure!("cannot write the password file: {e}"))?;
    let common = fixture.root().join("common.toml");
    let mut text =
        std::fs::read_to_string(&common).map_err(|e| failure!("cannot read common.toml: {e}"))?;
    text.push_str(&format!(
        "[services.ai_isolation]\nenabled = true\npool_size = 4\npassword_file = {:?}\n",
        password.display().to_string()
    ));
    std::fs::write(&common, text).map_err(|e| failure!("cannot write common.toml: {e}"))?;
    fixture.begin_shutdown();
    fixture.join_host().await;
    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let running = spot_cli::start_with(
        fixture.root(),
        false,
        spot_cli::Options {
            clock: None,
            login: LoginConfig::for_tests(),
        },
        async {
            let _ = stopped.await;
        },
    )
    .await
    .map_err(|e| failure!("the host did not restart with isolation: {e}"))?;
    let addr = *running
        .local_addrs()
        .first()
        .ok_or_else(|| failure!("the restarted host bound no address"))?;
    let outcome = measure(fixture, Client::new(addr, fixture.site().to_owned())).await;
    let _ = stop.send(());
    let _ = tokio::time::timeout(Duration::from_secs(15), running.join()).await;
    outcome
}

async fn measure(fixture: &Fixture, anonymous: Client) -> CaseResult {
    // Ready, then the actors.
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(reply) = anonymous.health("ready").await
            && reply.status == 200
        {
            break;
        }
        ensure!(
            Instant::now() < deadline,
            "the restarted host never became ready"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let admin = anonymous.sign_in("administrator", ADMIN_PASSWORD).await?;
    let manager_user = fixture.user(MANAGER);
    let manager = anonymous
        .sign_in(
            &manager_user.email,
            manager_user.password.as_deref().unwrap_or_default(),
        )
        .await?;
    let member_user = fixture.user(MEMBER);
    let member = anonymous
        .sign_in(
            &member_user.email,
            member_user.password.as_deref().unwrap_or_default(),
        )
        .await?;

    // Data for the agents to read.
    let project = admin
        .create(
            "project",
            &json!({
                "code": fixture.unique("AI"), "title": "AI isolation", "status": "Active",
                "owner": fixture.admin_id(), "currency": "USD", "budget": "10.00"
            }),
        )
        .await?;
    project.expect(201)?;
    let project_id = project.id()?;
    for n in 0..40 {
        admin
            .create(
                "task",
                &json!({"subject": format!("agent read {n}"), "project": project_id}),
            )
            .await?
            .expect(201)?;
    }

    // MCP answers at all, strictly as the manager.
    let probe = manager
        .send_json(
            "POST",
            "/mcp",
            &[],
            &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {}}),
        )
        .await?;
    probe.expect(200)?;

    // The human's latency on its own, for the log.
    let mut alone = Vec::new();
    for _ in 0..20 {
        let began = Instant::now();
        let reply = member.list("task", "limit=20").await?;
        reply.expect(200)?;
        alone.push(began.elapsed());
    }
    eprintln!("l3: isolation: human p95 alone {:?}", p95(alone));

    // 2. Twenty agents in a loop, and a steady human load beside them.
    let stop_at = Instant::now() + ROUND;
    let mut agents = Vec::new();
    for n in 0..AGENTS {
        let client = manager.clone();
        agents.push(tokio::spawn(async move {
            let (mut ok, mut shed, mut retry_after, mut other) = (0_u32, 0_u32, true, 0_u32);
            while Instant::now() < stop_at {
                let body = json!({
                    "jsonrpc": "2.0", "id": n, "method": "tools/call",
                    "params": {"name": "search_task", "arguments": {"limit": 50}}
                });
                match client.send_json("POST", "/mcp", &[], &body).await {
                    Ok(reply) if reply.status == 200 => ok += 1,
                    Ok(reply) if reply.status == 429 => {
                        shed += 1;
                        retry_after &= reply.header("retry-after").is_some();
                        // A client that is told to retry later does not spin: without a pause
                        // twenty tasks in the test process starve the host they are measuring.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                    _ => other += 1,
                }
            }
            (ok, shed, retry_after, other)
        }));
    }
    let mut human = Vec::new();
    let mut human_errors = 0_u32;
    while Instant::now() < stop_at {
        let started = Instant::now();
        match member.list("task", "limit=20").await {
            Ok(reply) if reply.status == 200 => human.push(started.elapsed()),
            _ => human_errors += 1,
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let (mut ok, mut shed, mut every_shed_has_retry_after, mut other) = (0_u32, 0_u32, true, 0_u32);
    for agent in agents {
        let (a, s, r, o) = agent
            .await
            .map_err(|e| failure!("an agent task failed: {e}"))?;
        ok += a;
        shed += s;
        every_shed_has_retry_after &= r;
        other += o;
    }
    fixture.note(&format!(
        "isolation: human samples {}, agent ok {ok}, shed {shed}, other {other}",
        human.len()
    ));

    // 3. The humans are inside the budget and none of them failed.
    ensure!(
        human_errors == 0,
        "{human_errors} human requests failed during the agent load"
    );
    eprintln!("l3: isolation: human p95 under load {:?} over {} samples", p95(human.clone()), human.len());
    ensure!(human.len() >= 20, "too few human samples ({})", human.len());
    let human_p95 = p95(human);
    ensure!(
        human_p95 <= BUDGET,
        "the human p95 was {human_p95:?} with twenty agents running; the budget is {BUDGET:?}"
    );
    // The AI side was the one held back: some calls were shed (twenty callers share one agent
    // identity here, whose concurrency is capped), every shed one says when to retry, and the rest
    // were served or refused cleanly, never a server error.
    ensure!(ok > 0, "no agent call was served at all");
    ensure!(shed > 0, "twenty parallel agents were never shed");
    ensure!(
        every_shed_has_retry_after,
        "a shed answer carried no Retry-After"
    );
    ensure!(
        other == 0,
        "{other} agent calls ended in something other than served or shed"
    );
    Ok(())
}
