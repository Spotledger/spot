//! The parallel safety proof: 200 sessions over one site, mixed creates, updates, submits and
//! lists, and the end-state invariants checked after the load.
//!
//! This is the first L7-style check on the real stack. It does not mock anything: the sessions
//! are real HTTP connections with real tokens, the writes go through the real engine, and the
//! invariants are read back over the API (and, for the outbox, from the database the host
//! itself wrote).
//!
//! One invariant cannot be enforced while `spot serve` does not register the installed apps'
//! controllers (WP-PRD-01.T4): a submitted Time Entry does not roll hours up, so a task's
//! `logged_hours` stays null. The check is written both ways: when the field is set it must
//! equal the sum of the submitted entries; when it is null the test says why it could not be
//! checked instead of pretending it passed.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use l3_http::Fixture;
use l3_http::client::Client;
use serde_json::json;

const SESSIONS: usize = 200;
/// The ceiling on the load phase (200 sessions, 1,200 tasks, 1,200 submitted entries). It
/// guards against a hang, not against slowness: the measured phase took 227 s on a 6-core
/// machine at a load average of 21 (`parallel_diagnosis.md`), and each request has its own
/// 30 s timeout. (The earlier 30 s per-session deadline also cut the work short under load,
/// so a loaded run did less than a quiet one and could miss the contended-update checks.)
const BUDGET: Duration = Duration::from_secs(600);
/// Requests the host serves at once. The 200 sessions stay open and keep working, but only this
/// many requests are in flight together: the fixture's host has a 48-connection pool with a 5 s
/// wait (`pool_max_size` in the fixture), a request runs several storage transactions, and 200
/// simultaneous requests overran the pool on a loaded machine (see `parallel_diagnosis.md`).
const IN_FLIGHT: usize = 16;

/// The last `lines` lines of a log file, or why it cannot be read.
fn log_tail(path: &std::path::Path, lines: usize) -> String {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let all: Vec<&str> = text.lines().collect();
            all[all.len().saturating_sub(lines)..].join("\n")
        }
        Err(error) => format!("(cannot read the log: {error})"),
    }
}

struct Created {
    id: String,
    subject: String,
}

struct Entry {
    task: String,
    hours: i64,
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn two_hundred_sessions_keep_every_invariant() {
    let Some(mut fixture) = Fixture::start().await else {
        println!("SKIPPED: set SPOT_TEST_PG_URL (and SPOT_REQUIRE_PG=1 so CI cannot skip)");
        return;
    };
    let admin = fixture.admin();
    let (project_id, _, _) = fixture.project("Parallel workload").await.unwrap();
    let contended = admin
        .create(
            "task",
            &json!({"subject": "Contended task", "project": project_id}),
        )
        .await
        .unwrap();
    let contended_id = contended.id().unwrap();

    // Sign in first, one at a time: 200 concurrent Argon2 verifications would saturate the
    // machine and make the load about hashing, not about the framework. The 200 sessions then
    // run concurrently.
    let email = fixture.user(l3_http::fixture::MANAGER).email.clone();
    let password = fixture
        .user(l3_http::fixture::MANAGER)
        .password
        .clone()
        .expect("the manager has a password");
    let mut clients = Vec::with_capacity(SESSIONS);
    for _ in 0..SESSIONS / 8 {
        let mut batch = Vec::new();
        for _ in 0..8 {
            let client = fixture.client().clone();
            let email = email.clone();
            let password = password.clone();
            batch.push(tokio::spawn(async move {
                client.sign_in(&email, &password).await
            }));
        }
        for handle in batch {
            clients.push(
                handle
                    .await
                    .expect("a sign-in task panicked")
                    .expect("a session signs in"),
            );
        }
    }
    let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(IN_FLIGHT));
    let mut sessions = Vec::with_capacity(SESSIONS);
    for (session, client) in clients.into_iter().enumerate() {
        sessions.push(tokio::spawn(run_session(
            session,
            client.with_in_flight_gate(gate.clone()),
            project_id.clone(),
            contended_id.clone(),
        )));
    }
    let started = Instant::now();
    let mut created = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();
    let mut successes = 0u64;
    for session in sessions {
        let outcome = match session.await {
            Ok(outcome) => outcome,
            Err(error) => {
                // The envelope of a 500 hides the storage error (the sign-in service answers
                // "temporarily unavailable" for any failed read); the server's own log has it.
                eprintln!(
                    "--- the last lines of {} ---\n{}",
                    fixture.log().display(),
                    log_tail(fixture.log(), 60)
                );
                panic!("a session task panicked: {error}");
            }
        };
        created.extend(outcome.created);
        entries.extend(outcome.entries);
        successes += outcome.contended_successes;
    }
    let elapsed = started.elapsed();
    println!(
        "{} sessions: {} tasks created, {} entries submitted, {} contended updates, {:.1}s",
        SESSIONS,
        created.len(),
        entries.len(),
        successes,
        elapsed.as_secs_f64()
    );
    assert!(
        elapsed <= BUDGET,
        "the workload overran its budget: {elapsed:?}"
    );
    assert!(
        created.len() >= SESSIONS * 2,
        "the workload created only {} tasks",
        created.len()
    );
    assert!(!entries.is_empty(), "the workload submitted no Time Entry");

    // Every task the workload wrote, read back in one pass (pages of 500) rather than 800
    // single-document reads: the invariants are about the end state, not about speed.
    let mut rows = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut body = json!({
            "filter": {"field": "project", "op": "eq", "value": project_id},
            "limit": 500,
        });
        if let Some(cursor) = &cursor {
            body["cursor"] = json!(cursor);
        }
        let page = admin.query("task", &body).await.unwrap();
        page.expect(200).unwrap();
        rows.extend(page.rows().unwrap());
        match page.next_cursor() {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    let by_id: std::collections::BTreeMap<String, serde_json::Value> = rows
        .into_iter()
        .filter_map(|row| {
            let id = row["id"].as_str()?.to_owned();
            Some((id, row))
        })
        .collect();

    // No lost update: every acknowledged write is what the document reports now.
    for task in &created {
        let row = by_id
            .get(&task.id)
            .unwrap_or_else(|| panic!("task {} is missing from the end state", task.id));
        assert_eq!(
            row["subject"],
            json!(task.subject),
            "task {} lost an acknowledged write: {row}",
            task.id
        );
        assert!(
            row["version"].as_i64().unwrap_or(0) >= 2,
            "task {} is at version {:?} after an acknowledged update",
            task.id,
            row["version"]
        );
    }

    // No lost update on the contended document: every successful patch advanced the version.
    let contended_now = admin.document("task", &contended_id).await.unwrap();
    contended_now.expect(200).unwrap();
    assert_eq!(
        contended_now.version().unwrap(),
        1 + i64::try_from(successes).unwrap(),
        "{} successful patches but the contended task is at version {}",
        successes,
        contended_now.version().unwrap()
    );

    // No duplicate name.
    let mut subjects: Vec<&str> = created.iter().map(|task| task.subject.as_str()).collect();
    subjects.sort_unstable();
    let before = subjects.len();
    subjects.dedup();
    assert_eq!(
        subjects.len(),
        before,
        "the workload produced duplicate task subjects"
    );

    // No negative roll-up, and the roll-up equals the sum of submitted entries where the
    // controller has run. While `spot serve` does not register the sample controller
    // (WP-PRD-01.T4), `logged_hours` is null; the check says so instead of pretending.
    let mut roll_up_checked = 0;
    for task in &created {
        let row = &by_id[&task.id];
        let logged = &row["logged_hours"];
        let sum: i64 = entries
            .iter()
            .filter(|entry| entry.task == task.id)
            .map(|entry| entry.hours)
            .sum();
        if logged.is_null() {
            continue;
        }
        let logged = cents(logged.as_str().expect("a decimal logged_hours"));
        assert!(logged >= 0, "task {} has negative logged hours", task.id);
        assert_eq!(
            logged, sum,
            "task {} rolled up {logged} but its submitted entries sum to {sum}",
            task.id
        );
        roll_up_checked += 1;
    }
    if roll_up_checked == 0 {
        println!(
            "NOTE: the roll-up invariant could not be enforced: no task has `logged_hours` \
             (the host does not register the sample controller, WP-PRD-01.T4)"
        );
    }

    // The outbox: when the host writes it, every task must have an event and every published
    // row must be an event that exists. When it does not write it, say so.
    let client = fixture.db().raw_client().await;
    let exists: bool = client
        .query_one("select to_regclass('public.spot_outbox') is not null", &[])
        .await
        .expect("the outbox probe")
        .get(0);
    if exists {
        let rows: i64 = client
            .query_one("select count(*) from spot_outbox", &[])
            .await
            .expect("the outbox count")
            .get(0);
        let published: i64 = client
            .query_one("select count(*) from spot_outbox_pub", &[])
            .await
            .expect("the published count")
            .get(0);
        let covered: i64 = client
            .query_one(
                "select count(distinct subject_id) from spot_outbox where subject_entity = 'task'",
                &[],
            )
            .await
            .expect("the outbox coverage")
            .get(0);
        println!("outbox: {rows} rows, {published} published, {covered} distinct task subjects");
        assert!(
            published <= rows,
            "{published} published rows for {rows} outbox rows"
        );
        assert!(
            covered >= i64::try_from(created.len()).unwrap(),
            "the outbox holds {covered} distinct task subjects for {} created tasks",
            created.len()
        );
    } else {
        println!(
            "NOTE: the outbox invariant could not be enforced: `spot_outbox` does not exist \
             (the host does not wire the feed, WP-PRD-01.T4)"
        );
    }

    fixture.finish().await;
}

struct Outcome {
    created: Vec<Created>,
    entries: Vec<Entry>,
    contended_successes: u64,
}

async fn run_session(
    session: usize,
    client: Client,
    project: String,
    contended: String,
) -> Outcome {
    let session_info = client.session().await.expect("a session");
    session_info.expect(200).expect("a session");
    let user_id = session_info
        .data()
        .pointer("/user/id")
        .and_then(|id| id.as_str())
        .expect("the signed-in user's id")
        .to_owned();
    let mut created = Vec::new();
    let mut entries = Vec::new();
    let mut contended_successes = 0;
    let mut round = 0;
    while round < 3 {
        round += 1;
        for n in 0..2 {
            let subject = format!("parallel-{session}-{round}-{n}");
            let task = client
                .create("task", &json!({"subject": subject, "project": project}))
                .await
                .expect("a create");
            task.expect(201).expect("a created task");
            let id = task.id().expect("an id");
            let edited = format!("{subject}-edited");
            let update = client
                .update(
                    "task",
                    &id,
                    task.version().expect("a version"),
                    &json!({"subject": edited}),
                )
                .await
                .expect("an update");
            update.expect(200).expect("an updated task");
            created.push(Created {
                id: id.clone(),
                subject: edited,
            });
            // A draft Time Entry, then submit it.
            let entry = client
                .create(
                    "time_entry",
                    &json!({"task": id, "person": user_id.clone(),
                            "created_by": user_id.clone(),
                            "work_date": "2026-10-22", "hours": "0.25"}),
                )
                .await
                .expect("an entry");
            entry.expect(201).expect("a draft entry");
            let posted = client
                .lifecycle(
                    "time_entry",
                    &entry.id().expect("an entry id"),
                    "post",
                    entry.version().expect("a version"),
                    None,
                )
                .await
                .expect("a post");
            posted.expect(200).expect("a posted entry");
            entries.push(Entry {
                task: id,
                hours: 25,
            });
        }
        // Two attempts on the contended document; a conflict is expected, not a failure.
        for _ in 0..2 {
            let current = client.document("task", &contended).await.expect("a read");
            current.expect(200).expect("a contended read");
            let patch = client
                .update(
                    "task",
                    &contended,
                    current.version().expect("a version"),
                    &json!({"priority": "High"}),
                )
                .await
                .expect("a contended update");
            match patch.status {
                200 => contended_successes += 1,
                412 => {}
                other => panic!("a contended update answered HTTP {other}: {}", patch.text()),
            }
        }
        let list = client.list("task", "limit=10").await.expect("a list");
        list.expect(200).expect("a task list");
    }
    Outcome {
        created,
        entries,
        contended_successes,
    }
}

/// Exact decimal text (`1.50`) as hundredths, so the check never uses floating point.
fn cents(text: &str) -> i64 {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let whole: i64 = whole.parse().expect("a whole part");
    let mut fraction = fraction.to_owned();
    while fraction.len() < 2 {
        fraction.push('0');
    }
    fraction.truncate(2);
    let fraction: i64 = fraction.parse().expect("a fraction part");
    whole * 100 + fraction
}
