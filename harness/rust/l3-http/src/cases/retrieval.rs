//! Retrieval over the running server (WP-AI-10.T3, WP-AI-10.T4): the acceptance sentence of
//! WP-AI-10, "a restricted user's agent never surfaces an unreadable document", end to end over
//! `POST /mcp`.
//!
//! Each case starts a site of its own that embeds text ([`Fixture::start_secondary_with_embedding`]):
//! an LLM tenant whose endpoint is a deterministic in-process fake ([`EmbedFake`]), the egress
//! entry that lets the product reach that loopback port, and `[services.retrieval]` on with an
//! index policy for Projects (readable by a Viewer) and Roles (readable by the administrators
//! only). The administrator creates one document of each kind through REST; a restricted user (a
//! Viewer) then calls `tools/call retrieve` and must never see the Role, and when the Viewer
//! assignment is deleted the very next call must show no hit at all and cause no re-index.
//!
//! The corpus is polled, never slept for: the administrator's own `retrieve` calls (who may read
//! both documents) show when the indexer has caught up, with a bound, and a failure names the last
//! answer's outcome (never a snippet).
//!
//! Retrieval needs the `vector` extension in the PostgreSQL server. Without it the site reports
//! `degraded: retrieval (retrieval.pgvector_missing ...)`, `retrieve` is never listed and a case
//! fails with the readiness text: an environment that cannot run the product's retrieval is a
//! failing run of this suite, not a skipped one.
//!
//! The behaviour is also proved without the network in `crates/framework/spot-cli/tests/
//! retrieval_service.rs`. A defect this suite finds is a defect to report, never a case to relax.

use std::time::{Duration, Instant};

use serde_json::{Value as Json, json};

use super::CaseDef;
use crate::client::Client;
use crate::embed_fake::{DIMENSIONS, EmbedFake};
use crate::fixture::{Fixture, RETRIEVAL_INDEX_POLICY, User, VIEWER};
use crate::report::{CaseResult, Failure};
use crate::{ensure, failure};

/// After the MCP cases (70); each case owns its fixture.
pub const ORDER: u32 = 75;

const SECTION: &str = "Retrieval";

/// A word only the Project's title carries (not a topic word, so it does not move its vector).
const PROJECT_MARK: &str = "zephyrquay7";

/// A word only the Role's name carries.
const ROLE_MARK: &str = "vaultmark9";

/// The query that points at the Project's topic words (`harbor`, `survey`).
const PROJECT_QUERY: &str = "harbor survey";

/// The query that points at the Role's topic words (`treasury`, `ledger`, `vault`).
const ROLE_QUERY: &str = "treasury ledger vault";

/// How long the indexer may take to catch up on the two documents.
const INDEX_BOUND: Duration = Duration::from_secs(90);

/// The pause between two polls.
const POLL_STEP: Duration = Duration::from_millis(300);

pub fn cases() -> Vec<CaseDef> {
    vec![
        CaseDef::run(
            "retrieval: a_restricted_users_agent_never_surfaces_an_unreadable_document",
            SECTION,
            "an agent signed in as a restricted user calls the `retrieve` MCP tool and gets no hit for a document that user may not read, and the unreadable document's text is in no snippet",
            false,
            |_| Box::pin(own_restricted()),
        ),
        CaseDef::run(
            "retrieval: a_revoked_share_applies_to_the_next_retrieve_call",
            SECTION,
            "after the share that made a document readable is revoked over REST, the very next `retrieve` call returns no hit for it, with no re-index in between",
            false,
            |_| Box::pin(own_revoked()),
        ),
    ]
}

/// The fake endpoint and the site that embeds through it.
async fn start() -> Result<(EmbedFake, Fixture), Failure> {
    let fake = EmbedFake::start(DIMENSIONS, None)
        .await
        .map_err(|e| failure!("the fake embeddings endpoint did not start: {e}"))?;
    let Some(fixture) =
        Fixture::start_secondary_with_embedding(&fake, RETRIEVAL_INDEX_POLICY).await
    else {
        return Err(failure!(
            "the retrieval case could not start its own fixture"
        ));
    };
    Ok((fake, fixture))
}

async fn own_restricted() -> CaseResult {
    let (fake, mut fixture) = start().await?;
    let result = restricted(&fixture, &fake).await;
    fixture.finish().await;
    result
}

async fn own_revoked() -> CaseResult {
    let (fake, mut fixture) = start().await?;
    let result = revoked(&fixture, &fake).await;
    fixture.finish().await;
    result
}

// ---------------------------------------------------------------------------------------------
// The MCP client
// ---------------------------------------------------------------------------------------------

/// One JSON-RPC answer: the HTTP status, the message the body carries and its text.
struct Answer {
    status: u16,
    message: Json,
    text: String,
}

impl Answer {
    /// The hits of a successful `retrieve` call.
    fn hits(&self) -> Option<Vec<Json>> {
        if self.message["result"]["isError"] == json!(true) {
            return None;
        }
        self.message["result"]["structuredContent"]["hits"]
            .as_array()
            .cloned()
    }

    /// What happened, in words that carry no snippet: for a failure message.
    fn outcome(&self) -> String {
        match self.hits() {
            Some(hits) => format!("HTTP {}, {} hits", self.status, hits.len()),
            None => {
                let code = self.message["error"]["data"]["code"]
                    .as_str()
                    .or_else(|| self.message["result"]["structuredContent"]["code"].as_str())
                    .unwrap_or("no code");
                format!("HTTP {}, no hits ({code})", self.status)
            }
        }
    }

    /// Whether some hit names `id`.
    fn names(&self, id: &str) -> bool {
        self.hits()
            .is_some_and(|hits| hits.iter().any(|hit| hit["id"] == json!(id)))
    }
}

/// `tools/call retrieve` as `client`.
async fn retrieve(client: &Client, id: u64, query: &str) -> Result<Answer, Failure> {
    let reply = client
        .send_json(
            "POST",
            "/mcp",
            &[],
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": "retrieve", "arguments": {"query": query, "k": 10}},
            }),
        )
        .await?;
    let text = reply.text();
    Ok(Answer {
        status: reply.status,
        message: serde_json::from_str(&text).unwrap_or(Json::Null),
        text,
    })
}

/// Calls `retrieve` as the administrator until `done` holds for the answer, within
/// [`INDEX_BOUND`]; a failure names the last outcome and the site's readiness.
async fn until(
    fixture: &Fixture,
    query: &str,
    what: &str,
    done: impl Fn(&Answer) -> bool,
) -> Result<Answer, Failure> {
    let admin = fixture.admin();
    let deadline = Instant::now() + INDEX_BOUND;
    let mut last: String;
    let mut id = 100;
    loop {
        let answer = retrieve(&admin, id, query).await?;
        if done(&answer) {
            return Ok(answer);
        }
        last = answer.outcome();
        if Instant::now() > deadline {
            break;
        }
        id += 1;
        tokio::time::sleep(POLL_STEP).await;
    }
    let ready = match fixture.client().health("ready").await {
        Ok(reply) => reply.text().chars().take(400).collect::<String>(),
        Err(e) => format!("(readiness unreadable: {e})"),
    };
    Err(failure!(
        "{what} did not show within {} s; last outcome: {last}; readiness: {ready}",
        INDEX_BOUND.as_secs()
    ))
}

/// Waits until the fake has served no request for a few poll steps (the indexer has drained the
/// change feed), bounded.
async fn settle(fake: &EmbedFake) {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut stable = 0;
    let mut last = fake.calls();
    while Instant::now() < deadline && stable < 3 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let now = fake.calls();
        if now == last {
            stable += 1;
        } else {
            stable = 0;
            last = now;
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------------------------

/// What the cases share: the restricted user, its client and the two documents.
struct World {
    user: User,
    restricted: Client,
    project_id: String,
    role_id: String,
}

/// Creates the corpus as the administrator, signs the restricted user in and waits until the
/// administrator's own `retrieve` shows that both documents are indexed.
async fn seed(fixture: &Fixture) -> Result<World, Failure> {
    // A Viewer reads Projects and nothing of Roles (the platform grants `role` to the
    // administrators and the user managers only).
    let user = fixture
        .create_user_with_password("restricted", VIEWER)
        .await?;
    let password = user
        .password
        .as_deref()
        .ok_or_else(|| failure!("the restricted user has no password"))?;
    let restricted = fixture.client().sign_in(&user.email, password).await?;

    let (project_id, _, _) = fixture
        .project(&format!("Harbor survey {PROJECT_MARK}"))
        .await?;
    let role = fixture
        .admin()
        .create(
            "role",
            &json!({
                "role_name": format!("Treasury ledger vault {ROLE_MARK}"),
                "origin": "reference",
            }),
        )
        .await?;
    role.expect(201)?;
    let role_id = role.id()?;

    // Both documents are readable by the administrator, so its own answers show the indexer.
    let project_id_for_poll = project_id.clone();
    until(fixture, PROJECT_QUERY, "the Project", move |answer| {
        answer.names(&project_id_for_poll)
    })
    .await?;
    let role_id_for_poll = role_id.clone();
    until(fixture, ROLE_QUERY, "the Role", move |answer| {
        answer.names(&role_id_for_poll)
    })
    .await?;
    Ok(World {
        user,
        restricted,
        project_id,
        role_id,
    })
}

// ---------------------------------------------------------------------------------------------
// The cases
// ---------------------------------------------------------------------------------------------

async fn restricted(fixture: &Fixture, _fake: &EmbedFake) -> CaseResult {
    let world = seed(fixture).await?;

    // The restricted user's agent asks for the Role's topic: the nearest chunk is the Role, which
    // the user may not read, so no hit names it and none of its text comes back.
    let answer = retrieve(&world.restricted, 1, ROLE_QUERY).await?;
    ensure!(
        answer.hits().is_some(),
        "the restricted user's retrieve call did not answer hits ({})",
        answer.outcome()
    );
    ensure!(
        !answer.names(&world.role_id),
        "a hit names the Role the restricted user may not read"
    );
    ensure!(
        !answer.text.contains(ROLE_MARK) && !answer.text.contains(&world.role_id),
        "the unreadable Role's text or id is in the restricted user's answer"
    );
    for hit in answer.hits().unwrap_or_default() {
        ensure!(
            hit["doctype"] != json!("role"),
            "a hit of the `role` DocType reached a user who may not read roles"
        );
    }

    // The control: the same user asking for the Project's topic gets the Project, so the empty
    // Role answer above is the permission re-check and not an index that was never filled.
    let answer = retrieve(&world.restricted, 2, PROJECT_QUERY).await?;
    ensure!(
        answer.names(&world.project_id),
        "the restricted user does not get the Project it may read ({})",
        answer.outcome()
    );
    // And the administrator, who may read the Role, does get it for the Role's query.
    let admin = retrieve(&fixture.admin(), 3, ROLE_QUERY).await?;
    ensure!(
        admin.names(&world.role_id),
        "the administrator does not get the Role it may read ({})",
        admin.outcome()
    );
    Ok(())
}

async fn revoked(fixture: &Fixture, fake: &EmbedFake) -> CaseResult {
    let world = seed(fixture).await?;

    // Before the revocation the restricted user reads the Project through retrieve.
    let before = retrieve(&world.restricted, 1, PROJECT_QUERY).await?;
    ensure!(
        before.names(&world.project_id),
        "before the revocation the restricted user does not get the Project ({})",
        before.outcome()
    );

    // The indexer has caught up: from here on only a query may reach the embeddings endpoint.
    settle(fake).await;
    let calls_before = fake.calls();
    let inputs_before = fake.inputs().len();

    // Revoke the share: the restricted user's role assignment, over REST.
    let assignments = fixture
        .admin()
        .list(
            "role_assignment",
            &format!("limit=100&user={}", world.user.id),
        )
        .await?;
    assignments.expect(200)?;
    // The user's own rows only, whatever the list filter did.
    let rows: Vec<_> = assignments
        .rows()?
        .into_iter()
        .filter(|row| row["user"].as_str() == Some(world.user.id.as_str()))
        .collect();
    ensure!(
        !rows.is_empty(),
        "the restricted user has no role assignment to revoke"
    );
    // An assignment is ended by its state (the record is kept as history), and an ended
    // assignment stops counting at once. A delete answered 403 `no_grant` for the harness admin.
    for row in rows {
        let id = row["id"].as_str().unwrap_or_default();
        let version = row["version"].as_i64().unwrap_or(1);
        fixture
            .admin()
            .update("role_assignment", id, version, &json!({"state": "ended"}))
            .await?
            .expect(200)?;
    }

    // The very next call: no hit for the Project, and not one word of it.
    let after = retrieve(&world.restricted, 2, PROJECT_QUERY).await?;
    ensure!(
        !after.names(&world.project_id),
        "the revoked share still shows the Project on the next retrieve call"
    );
    ensure!(
        !after.text.contains(PROJECT_MARK) && !after.text.contains(&world.project_id),
        "the revoked Project's text or id is in the next retrieve answer"
    );

    // No re-index in between: the endpoint was asked for at most the query, and never for a
    // document's text.
    let calls_after = fake.calls();
    ensure!(
        calls_after <= calls_before + 1,
        "the embeddings endpoint served {} calls after the revocation (one query expected at most)",
        calls_after - calls_before
    );
    let new_inputs: Vec<String> = fake.inputs().into_iter().skip(inputs_before).collect();
    ensure!(
        new_inputs.len() <= 1,
        "{} texts were embedded after the revocation (one query expected at most)",
        new_inputs.len()
    );
    ensure!(
        new_inputs
            .iter()
            .all(|text| !text.contains(PROJECT_MARK) && !text.contains(ROLE_MARK)),
        "a document's text was embedded after the revocation: the revocation caused a re-index"
    );
    Ok(())
}
