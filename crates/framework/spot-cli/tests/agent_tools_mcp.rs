//! WP-AI-04.T7: the remedy tools as the MCP service offers them (contract `mcp.md`, section 5). The
//! tools run over the real `RemedyHost` (the framework's in-memory world, an in-memory audit chain
//! and stub agent registrations, as `remedies_service.rs`), called through the `ExtensionTool`
//! interface the MCP service calls: who they are listed for, the dry-run / apply / undo round trip
//! with the `undo_token`, `route` and `undo_expires_at` the contract names, a tampered or replayed
//! plan refused, and a fixed sentence per code. The seam itself (listing, limits, deadline, audit)
//! is proved in `spot-mcp/tests/extension_tools.rs` and `mcp_extension.rs`; the retrieval and
//! memory tools need PostgreSQL with pgvector and an embedding provider and are covered by their
//! service suites (`retrieval_service.rs`, `agent_memory_service.rs`) and, once
//! WP-AI-03.T7 provides the embedding port, by the l3-http cases.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::{Value as Json, json};
use spot_agent_tiers::{AgentIdentity, AgentRegistry, Domain, Status, Tier};
use spot_app_api::context::Actor;
use spot_app_api::prelude::RecordId;
use spot_cli::services::remedies::host::{AgentSource, Deps, Fut, RemedyHost};
use spot_cli::services::remedies::tools::{self, RemedyTool};
use spot_cli::services::remedies::world::{
    CtxRequest, ShippedCatalogue, WorldAccess, WorldError, WorldKind,
};
use spot_core::{Clock, ManualClock};
use spot_functions::{check_schema, validate_arguments};
use spot_mcp::principal::McpPrincipal;
use spot_mcp::{AuditSink, ExtensionError, ExtensionTool};
use spot_permissions::AssignmentContext;
use spot_remedies::ports::{IndexHealth, JobLife};
use spot_remedies::{FakeWorld, RemedyCtx, RemedySettings};
use time::OffsetDateTime;

const START: i64 = 1_795_262_400;

/// The framework's in-memory world, once live and once as the rehearsal snapshot.
struct Worlds {
    live: Mutex<FakeWorld>,
    rehearsal: Mutex<FakeWorld>,
}

impl Worlds {
    fn seeded() -> Arc<Self> {
        let seed = || {
            let mut world = FakeWorld::new();
            world.jobs.seed("j1", JobLife::Failed, 1, None);
            world.index.seed("retrieval", IndexHealth::Stale, 3);
            world
        };
        Arc::new(Self {
            live: Mutex::new(seed()),
            rehearsal: Mutex::new(seed()),
        })
    }

    fn live_state(&self) -> String {
        format!("{:?}", *self.live.lock().unwrap())
    }
}

impl WorldAccess for Worlds {
    fn with_ctx(
        &self,
        kind: WorldKind,
        request: CtxRequest<'_>,
        work: &mut dyn FnMut(&mut RemedyCtx<'_>),
    ) -> Result<(), WorldError> {
        let cell = match kind {
            WorldKind::Live => &self.live,
            WorldKind::Rehearsal => &self.rehearsal,
        };
        let mut guard = cell.lock().unwrap();
        let world = &mut *guard;
        let mut ctx = RemedyCtx::new(
            &mut world.jobs,
            &mut world.cache,
            &mut world.index,
            &mut world.edge,
            &mut world.quarantine,
            &mut world.case,
            &mut world.document,
            request.audit,
            request.actor,
            request.now,
            request.settings,
        );
        work(&mut ctx);
        Ok(())
    }
}

/// Stored registrations, as the tier service would answer them.
struct Agents(HashMap<String, AgentIdentity>);

impl Agents {
    fn standard() -> Arc<Self> {
        let mut map = HashMap::new();
        let mut add = |name: &str, domain: Domain, tier: Tier, status: Status| {
            let mut registry = AgentRegistry::new();
            registry
                .enroll(AgentIdentity::new(name, "owner", domain, tier).with_status(status))
                .unwrap();
            map.insert(name.to_owned(), registry.get(name).cloned().unwrap());
        };
        add("agent-t0", Domain::Support, Tier::T0, Status::Active);
        add("agent-t1", Domain::Support, Tier::T1, Status::Active);
        add("agent-t2", Domain::Support, Tier::T2, Status::Active);
        Arc::new(Self(map))
    }
}

impl AgentSource for Agents {
    fn identity<'a>(
        &'a self,
        tenant: &'a str,
        agent_id: &'a str,
    ) -> Fut<'a, Option<AgentIdentity>> {
        Box::pin(async move {
            if tenant != "site-1" {
                return None;
            }
            self.0.get(agent_id).cloned()
        })
    }
}

struct Rig {
    worlds: Arc<Worlds>,
    audit: Arc<spot_mcp::MemoryAudit>,
    tools: Vec<Arc<dyn ExtensionTool>>,
}

fn rig() -> Rig {
    let worlds = Worlds::seeded();
    let audit = Arc::new(spot_mcp::MemoryAudit::new());
    let clock: Arc<dyn Clock> = Arc::new(ManualClock::new(
        OffsetDateTime::from_unix_timestamp(START).unwrap(),
    ));
    let host = RemedyHost::new(Deps {
        site: "site-1".to_owned(),
        catalogue: Arc::new(ShippedCatalogue),
        settings: RemedySettings::default(),
        world: Some(Arc::clone(&worlds) as Arc<dyn WorldAccess>),
        agents: Agents::standard(),
        apply_path: None,
        sink: Arc::clone(&audit) as Arc<dyn AuditSink>,
        clock: Arc::clone(&clock),
    })
    .unwrap();
    let tools = RemedyTool::all(&Arc::new(host), "site-1", &clock);
    Rig {
        worlds,
        audit,
        tools,
    }
}

impl Rig {
    fn tool(&self, name: &str) -> &Arc<dyn ExtensionTool> {
        self.tools.iter().find(|t| t.name() == name).unwrap()
    }

    async fn call(
        &self,
        name: &str,
        principal: &McpPrincipal,
        arguments: Json,
    ) -> Result<Json, ExtensionError> {
        self.tool(name).call(principal, &arguments).await
    }
}

fn agent(name: &str, tier: Tier) -> McpPrincipal {
    McpPrincipal::new_agent(
        &format!("user-{name}"),
        Actor::User {
            id: RecordId::new_v7(),
            via: None,
        },
        vec![AssignmentContext::unscoped("a1", &["reader"])],
        name,
        tier,
    )
    .at(START)
}

fn user() -> McpPrincipal {
    McpPrincipal::new(
        "user-plain",
        Actor::User {
            id: RecordId::new_v7(),
            via: None,
        },
        vec![AssignmentContext::unscoped("a1", &["reader"])],
    )
    .at(START)
}

const RETRY: &str = concat!("job", ".retry_failed");

#[test]
fn the_four_tools_have_the_contracts_names() {
    let rig = rig();
    let mut names: Vec<&str> = rig.tools.iter().map(|t| t.name()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "remedy_apply",
            "remedy_dry_run",
            "remedy_list",
            "remedy_undo"
        ]
    );
    assert_eq!(tools::NAMES.len(), 4);
}

#[test]
fn the_tools_are_listed_only_for_an_agent_and_the_write_tools_only_for_a_tier_that_applies() {
    let rig = rig();
    for name in tools::NAMES {
        assert!(
            rig.tool(name).spec(&user()).is_none(),
            "{name} is for a registered agent only"
        );
    }
    let t0 = agent("agent-t0", Tier::T0);
    assert!(rig.tool("remedy_list").spec(&t0).is_some());
    assert!(rig.tool("remedy_dry_run").spec(&t0).is_some());
    assert!(
        rig.tool("remedy_apply").spec(&t0).is_none(),
        "T0 explains and reads: no apply tool"
    );
    assert!(rig.tool("remedy_undo").spec(&t0).is_none());
    let t1 = agent("agent-t1", Tier::T1);
    for name in tools::NAMES {
        let spec = rig.tool(name).spec(&t1).unwrap();
        let writes = name == "remedy_apply" || name == "remedy_undo";
        assert_eq!(spec.writes, writes, "{name}");
        assert!(
            spec.input_schema["additionalProperties"] == json!(false),
            "{name} refuses unknown arguments"
        );
    }
}

#[test]
fn the_tool_schemas_are_well_formed_and_refuse_an_unknown_argument_by_path() {
    // The calls below go straight to `ExtensionTool::call` (the MCP service's own argument
    // check is proved in `spot-mcp/tests/argument_schema.rs`); this checks the schemas the
    // service would apply: each is well formed, accepts the argument shape the tests send,
    // and refuses an argument it does not declare with that argument's path.
    let rig = rig();
    let t1 = agent("agent-t1", Tier::T1);
    let shapes = [
        ("remedy_list", json!({})),
        (
            "remedy_dry_run",
            json!({"remedy": RETRY, "params": {"job_id": "j1"}}),
        ),
        (
            "remedy_apply",
            json!({"plan": {"remedy": RETRY, "params": {}, "steps": [], "hash": "h"}}),
        ),
        ("remedy_undo", json!({"remedy": RETRY, "undo_token": "t"})),
    ];
    for (name, valid) in shapes {
        let spec = rig.tool(name).spec(&t1).unwrap();
        assert_eq!(check_schema(&spec.input_schema), Ok(()), "{name}");
        assert_eq!(
            validate_arguments(&spec.input_schema, &valid),
            Ok(()),
            "{name} accepts the arguments the tests send"
        );
        let mut extra = valid.clone();
        extra["tier"] = json!(3);
        let error = validate_arguments(&spec.input_schema, &extra).unwrap_err();
        assert_eq!(error.path, "tier", "{name}");
        assert_eq!(error.message, "unexpected property", "{name}");
    }
}

#[tokio::test]
async fn a_list_is_filtered_by_the_agents_tier() {
    let rig = rig();
    let t1 = rig
        .call("remedy_list", &agent("agent-t1", Tier::T1), json!({}))
        .await
        .unwrap();
    let remedies = t1["remedies"].as_array().unwrap();
    assert!(!remedies.is_empty());
    for remedy in remedies {
        for key in [
            "id",
            "title",
            "tier",
            "risk",
            "affects",
            "preconditions",
            "reversible",
            "params_schema",
        ] {
            assert!(remedy.get(key).is_some(), "a remedy carries {key}");
        }
        assert!(
            matches!(remedy["tier"].as_str(), Some("T0" | "T1")),
            "{remedy}"
        );
    }
    let t0 = rig
        .call("remedy_list", &agent("agent-t0", Tier::T0), json!({}))
        .await
        .unwrap();
    assert!(t0["remedies"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn a_t1_remedy_dry_runs_applies_and_undoes_through_the_tools() {
    let rig = rig();
    let caller = agent("agent-t1", Tier::T1);
    let before = rig.worlds.live_state();

    let plan = rig
        .call(
            "remedy_dry_run",
            &caller,
            json!({"remedy": RETRY, "params": {"job_id": "j1"}}),
        )
        .await
        .unwrap();
    assert!(plan["hash"].as_str().is_some_and(|hash| !hash.is_empty()));
    assert_eq!(rig.worlds.live_state(), before, "a dry-run changes nothing");

    let applied = rig
        .call("remedy_apply", &caller, json!({"plan": plan.clone()}))
        .await
        .unwrap();
    assert_eq!(applied["route"], "direct");
    assert_eq!(applied["remedy"], RETRY);
    assert_eq!(applied["plan_hash"], plan["hash"]);
    assert_eq!(
        applied["undo_expires_at"],
        json!(START + RemedySettings::default().undo_window_secs)
    );
    let token = applied["undo_token"].as_str().unwrap().to_owned();
    assert!(!token.is_empty());
    assert_ne!(rig.worlds.live_state(), before);
    for change in applied["changes"].as_array().unwrap() {
        for key in ["target", "before", "after"] {
            assert!(change.get(key).is_some(), "a change carries {key}");
        }
    }

    // One dry-run authorises one apply: the same plan again is refused.
    let again = rig
        .call("remedy_apply", &caller, json!({"plan": plan}))
        .await
        .unwrap_err();
    assert_eq!(again.code, "remedies.plan_stale");

    let undone = rig
        .call(
            "remedy_undo",
            &caller,
            json!({"remedy": RETRY, "undo_token": token}),
        )
        .await
        .unwrap();
    assert_eq!(undone["undo_token"], json!(token));
    assert_eq!(
        rig.worlds.live_state(),
        before,
        "undo restores the pre-state exactly"
    );
    assert!(
        rig.audit
            .records()
            .iter()
            .any(|record| record.kind.ends_with("apply_started")),
        "the intent record is on the chain"
    );
}

#[tokio::test]
async fn a_tampered_plan_is_refused_and_nothing_changes() {
    let rig = rig();
    let caller = agent("agent-t1", Tier::T1);
    let plan = rig
        .call(
            "remedy_dry_run",
            &caller,
            json!({"remedy": RETRY, "params": {"job_id": "j1"}}),
        )
        .await
        .unwrap();
    let before = rig.worlds.live_state();
    let mut tampered = plan.clone();
    tampered["params"] = json!({"job_id": "j2"});
    let error = rig
        .call("remedy_apply", &caller, json!({"plan": tampered}))
        .await
        .unwrap_err();
    assert!(error.code.starts_with("remedies."), "{}", error.code);
    assert_eq!(rig.worlds.live_state(), before);
    // A plan that is not a plan at all is an argument error.
    let error = rig
        .call("remedy_apply", &caller, json!({"plan": {"remedy": RETRY}}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "api.invalid_request");
}

#[tokio::test]
async fn a_remedy_above_the_agents_tier_is_refused_with_a_fixed_sentence() {
    let rig = rig();
    let error = rig
        .call(
            "remedy_dry_run",
            &agent("agent-t1", Tier::T1),
            json!({"remedy": concat!("index", ".rebuild_retrieval"), "params": {"index": "retrieval"}}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "remedies.refused");
    assert!(!error.message.contains("retrieval"), "{}", error.message);
}

#[tokio::test]
async fn an_unknown_remedy_and_a_text_that_is_no_remedy_id_are_the_same_refusal() {
    let rig = rig();
    let caller = agent("agent-t1", Tier::T1);
    for remedy in [concat!("job", ".no_such_remedy"), "Not A Remedy"] {
        let error = rig
            .call("remedy_dry_run", &caller, json!({"remedy": remedy}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "remedies.unknown", "{remedy}");
        assert!(!error.message.contains(remedy), "{}", error.message);
    }
}

#[tokio::test]
async fn a_caller_that_is_not_an_agent_is_refused_even_if_it_reaches_the_tool() {
    let rig = rig();
    let error = rig
        .call("remedy_list", &user(), json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission.denied");
}
