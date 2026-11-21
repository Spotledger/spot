//! Lineage of one site: the production `LineagePort` behind `POST /api/v1/lineage`
//! (`lineage_trace`, WP-DAG-05.T2), over `spot-lineage` and the hosted graph (`services/graph.rs`).
//!
//! - **Chain configuration** is app data: each installed app may ship `<app>/lineage.toml` (the
//!   text `ChainConfig::parse` accepts); the files of all apps are read together when the site
//!   starts. The platform names no ERP DocType. A site with no file answers
//!   `422 lineage.kind_unknown` for every kind; a file that does not parse is reported by
//!   readiness (`lineage`, degraded) and serves nothing: never a partial chain.
//! - **As the user.** Every level runs through `GraphHost::execute` as the requesting caller, so
//!   the graph's permission filter applies to each record; a hidden record is removed with its
//!   paths and only `withheld` says so. A start record the caller may not read and one that does
//!   not exist are one `graph.not_found`.
//! - **Exact quantities**: `bom` quantities and level totals are decimal strings (`Quantity`).
//! - **Limits**: `depth` (1 to 12) and `limit` (1 to `max_nodes`, default 5,000) as the route
//!   checked them; `truncated` with `depth` or `nodes` as `spot-lineage` reports.
//!
//! The route group is mounted by `services/graph.rs` once every module registered; this module
//! only provides the `LineagePort`. Settings `[services.lineage]`: `enabled` (true), `max_nodes`
//! (5,000, 1 to 5,000). Level: optional. Audit target `spot::audit::lineage`: who, kind, counts,
//! refusal codes; never a value or record id.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde::Deserialize;
use spot_graph_query::{
    Direction, GraphExecutor, GraphQuery, GraphQueryError, GraphResult, Invocation,
};
use spot_http::api::codec::parse_record_id;
use spot_http::api::ports::graph::{
    GraphResult as PortResult, LineageAnswerOut, LineageKindIn, LineageLevelOut, LineageNodeOut,
    LineagePort, LineageQueryIn, LineageTruncationOut, NodeRef as WireNode,
};
use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, RequestCtx};
use spot_lineage::{
    ChainConfig, LineageError, LineageQuery, LineageResult, Quantity, Truncation, compile, run,
};
use spot_storage_contract::{Name, NodeRef};

use crate::services::graph::{Caller, GraphHost};
use crate::services::{CheckFuture, Idle, Level, SiteCtx};
use crate::{Error, Result};

/// Start order: after the graph (1200), whose host port it runs over.
pub const ORDER: u32 = 1210;

const AUDIT_TARGET: &str = "spot::audit::lineage";
/// The file an app ships its chain configuration in.
pub const CONFIG_FILE: &str = "lineage.toml";

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Settings {
    enabled: bool,
    max_nodes: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            max_nodes: 5_000,
        }
    }
}

/// The graph executor a lineage walk runs over: the hosted graph, as one caller.
struct HostExecutor {
    host: Arc<dyn GraphHost>,
    caller: Caller,
}

impl GraphExecutor for HostExecutor {
    async fn execute(
        &mut self,
        query: &GraphQuery,
        _who: &Invocation<'_>,
    ) -> std::result::Result<GraphResult, GraphQueryError> {
        self.host.execute(&self.caller, query).await
    }
}

struct Lineage {
    site: String,
    host: Arc<dyn GraphHost>,
    config: std::result::Result<ChainConfig, String>,
    max_nodes: u32,
}

fn invalid(path: &str) -> ApiError {
    ApiError::from_code(Code::GraphQueryInvalid).detail("path", serde_json::Value::from(path))
}

fn node_of(node: &WireNode) -> std::result::Result<NodeRef, ApiError> {
    Ok(NodeRef {
        entity: Name::new(&node.doctype).map_err(|_| invalid("body.doctype"))?,
        id: parse_record_id(&node.id).ok_or_else(|| invalid("body.id"))?,
    })
}

fn typed(request: &LineageQueryIn) -> std::result::Result<LineageQuery, ApiError> {
    let start = node_of(&request.start)?;
    let max_depth = u16::from(request.depth.unwrap_or(12));
    Ok(match request.kind {
        LineageKindIn::Chain => LineageQuery::Document {
            doctype: start.entity,
            name: start.id,
            direction: Direction::Both,
            max_depth,
        },
        LineageKindIn::TraceForward => LineageQuery::BatchTrace {
            batch: start,
            direction: Direction::In,
        },
        LineageKindIn::TraceBack => LineageQuery::BatchTrace {
            batch: start,
            direction: Direction::Out,
        },
        LineageKindIn::Bom => LineageQuery::BomExplosion {
            item: start,
            qty_basis: Quantity::one(),
            max_depth,
        },
        LineageKindIn::WhereUsed => LineageQuery::WhereUsed { item: start },
    })
}

fn answer_of(result: &LineageResult) -> LineageAnswerOut {
    let mut by_depth: BTreeMap<u16, Vec<LineageNodeOut>> = BTreeMap::new();
    for node in &result.nodes {
        by_depth
            .entry(node.depth)
            .or_default()
            .push(LineageNodeOut {
                node: WireNode::new(node.node.entity.as_str(), node.node.id.to_string()),
                quantity: node.quantity.map(|q| q.to_string()),
            });
    }
    let levels = by_depth
        .into_iter()
        .map(|(depth, nodes)| {
            let total = result
                .levels
                .iter()
                .find(|l| l.depth == depth)
                .and_then(|l| l.quantity);
            LineageLevelOut {
                depth: u8::try_from(depth).unwrap_or(u8::MAX),
                node_count: u32::try_from(nodes.len()).unwrap_or(u32::MAX),
                nodes,
                quantity_total: total.map(|q| q.to_string()),
            }
        })
        .collect();
    let reason = match result.truncated {
        Some(Truncation::Depth { .. }) => Some(LineageTruncationOut::Depth),
        Some(Truncation::Nodes { .. }) => Some(LineageTruncationOut::Nodes),
        _ => None,
    };
    LineageAnswerOut {
        levels,
        truncated: reason.is_some(),
        truncation_reason: reason,
        withheld: matches!(result.truncated, Some(Truncation::Permission)),
    }
}

impl Lineage {
    async fn trace(
        &self,
        ctx: &RequestCtx,
        request: &LineageQueryIn,
    ) -> PortResult<LineageAnswerOut> {
        let caller = self.host.caller(ctx).await?;
        let outcome = self.answer(caller.clone(), request).await;
        match &outcome {
            Ok(a) => tracing::info!(target: AUDIT_TARGET, site = %self.site, who = %caller.user,
                kind = request.kind.as_str(), levels = a.levels.len(), truncated = a.truncated,
                withheld = a.withheld, "lineage trace"),
            Err(e) => tracing::info!(target: AUDIT_TARGET, site = %self.site, who = %caller.user,
                kind = request.kind.as_str(), code = %e.code.as_str(), "lineage refused"),
        }
        outcome
    }

    async fn answer(
        &self,
        caller: Caller,
        request: &LineageQueryIn,
    ) -> PortResult<LineageAnswerOut> {
        // A configuration that did not parse serves nothing, for every kind.
        let kind_unknown = || ApiError::from_code(Code::LineageKindUnknown);
        let config = self.config.as_ref().map_err(|_| kind_unknown())?;
        let query = typed(request)?;
        let start = node_of(&request.start)?;
        let mut plan = compile(&query, config).map_err(|e| match e {
            LineageError::UnknownChain { .. } | LineageError::UnknownBom { .. } => kind_unknown(),
            _ => invalid("body"),
        })?;
        if let Some(limit) = request.limit {
            plan = plan
                .with_max_nodes(limit.min(self.max_nodes))
                .map_err(|_| invalid("body.limit"))?;
        }
        let engine = self
            .host
            .engine()
            .ok_or_else(|| ApiError::from_code(Code::GraphUnavailable))?;
        let who = Invocation {
            principal: &caller.principal,
            engine: &engine,
        };
        let mut executor = HostExecutor {
            host: Arc::clone(&self.host),
            caller: caller.clone(),
        };
        let result = run(&mut executor, &plan, &who).await.map_err(|e| match e {
            LineageError::Graph(_) => ApiError::from_code(Code::GraphUnavailable),
            _ => invalid("body"),
        })?;
        // A start the caller may not read, and one that does not exist, are one answer.
        if !result.nodes.iter().any(|n| n.depth == 0 && n.node == start) {
            return Err(ApiError::from_code(Code::GraphNotFound));
        }
        Ok(answer_of(&result))
    }
}

struct Port(Arc<Lineage>);

impl LineagePort for Port {
    fn trace<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        query: &'a LineageQueryIn,
    ) -> BoxFuture<'a, PortResult<LineageAnswerOut>> {
        Box::pin(async move { self.0.trace(ctx, query).await })
    }
}

/// The chain configuration of every installed app, read together (`[[chain]]` and `[[bom]]` tables
/// repeat across files). Absent files contribute nothing.
fn read_config(ctx: &SiteCtx<'_>) -> Result<(String, bool)> {
    let env = *ctx.env()?;
    let mut text = String::new();
    for app in super::controllers::installed(env.root, env.loaded, env.site)? {
        if let Ok(part) = std::fs::read_to_string(app.dir.join(CONFIG_FILE)) {
            text.push_str(&part);
            text.push('\n');
        }
    }
    let any = !text.trim().is_empty();
    Ok((text, any))
}

/// Registers the lineage port (and a readiness check that reports a configuration that does not
/// parse).
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("lineage")?;
    if !settings.enabled || !(1..=5_000).contains(&settings.max_nodes) {
        return if settings.enabled {
            Err(Error("services.lineage.max_nodes: out of range".to_owned()))
        } else {
            Ok(())
        };
    }
    let Some(host) = ctx.port::<dyn GraphHost>() else {
        return Ok(());
    };
    let (text, _any) = read_config(ctx)?;
    let config = ChainConfig::parse(&text).map_err(|e| e.to_string());
    let problem = config.as_ref().err().cloned();
    if ctx.assembling() {
        ctx.add(Idle::new(
            "lineage",
            Level::Optional,
            move || -> CheckFuture<'static> {
                let problem = problem.clone();
                Box::pin(async move {
                    problem.map_or(Ok(()), |p| {
                        Err(format!("the lineage configuration does not parse: {p}"))
                    })
                })
            },
        ));
    }
    let site = ctx.env()?.site.site.clone();
    let lineage = Arc::new(Lineage {
        site,
        host,
        config,
        max_nodes: settings.max_nodes,
    });
    ctx.provide::<dyn LineagePort>(Arc::new(Port(lineage)) as Arc<dyn LineagePort>)?;
    Ok(())
}
