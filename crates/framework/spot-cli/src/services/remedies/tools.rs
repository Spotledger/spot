//! The four remedy tools of the MCP service (contract `mcp.md`, section 5, "Extension tools"):
//! `remedy_list`, `remedy_dry_run`, `remedy_apply` and `remedy_undo`.
//!
//! Each is a thin adapter over [`RemedyHost`] and holds no rule of its own: it checks that the
//! caller is a registered agent, maps the arguments, calls the host **as that agent on this
//! site** (the tenant is the site and the agent id is the authenticated principal's, never an
//! argument), and maps the answer. The host does the tier, domain and status checks, the audit
//! record that precedes every apply and undo, the one-dry-run-one-apply rule and the fix pipeline
//! route.
//!
//! - **Listing.** The tools are listed only for an agent principal. `remedy_apply` and
//!   `remedy_undo` are write tools and are listed only when the agent's tier allows an apply (T1
//!   or above: T0 explains and reads). The host still refuses a remedy above the agent's tier.
//! - **Errors.** A refusal carries the code of the framework or of the host
//!   (`remedies.*`, `remedies_host.*`) and a fixed sentence per code: the sentence never quotes a
//!   remedy id, a parameter, a plan step or a token the caller sent.
//! - **The apply answer.** `route` is `direct` for a reversible T1 remedy and `pipeline` for any
//!   other (the host's own rule). `undo_expires_at` is the end of the undo window in Unix seconds,
//!   computed from the instant read **before** the apply, so it is never later than the real
//!   deadline (the host's `Applied` carries no deadline; a later task may expose the record's).

use std::sync::Arc;

use serde_json::{Value as Json, json};
use spot_agent_tiers::{Approval, Tier};
use spot_core::Clock;
use spot_mcp::principal::McpPrincipal;
use spot_mcp::{BoxFuture, ExtensionError, ExtensionSpec, ExtensionTool};
use spot_remedies::{Plan, RemedyId, UndoToken};

use super::host::{HostError, RemedyHost, RemedySummary};
use crate::services::mcp::McpHandle;

/// `remedy_list`.
pub const LIST: &str = "remedy_list";
/// `remedy_dry_run`.
pub const DRY_RUN: &str = "remedy_dry_run";
/// `remedy_apply`.
pub const APPLY: &str = "remedy_apply";
/// `remedy_undo`.
pub const UNDO: &str = "remedy_undo";
/// Every remedy tool name.
pub const NAMES: [&str; 4] = [LIST, DRY_RUN, APPLY, UNDO];

/// The longest remedy id or undo token the schema admits.
const MAX_TEXT: usize = 128;

/// Which remedy tool an adapter is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    List,
    DryRun,
    Apply,
    Undo,
}

impl Which {
    fn name(self) -> &'static str {
        match self {
            Which::List => LIST,
            Which::DryRun => DRY_RUN,
            Which::Apply => APPLY,
            Which::Undo => UNDO,
        }
    }

    /// Whether the tool can change the site.
    fn writes(self) -> bool {
        matches!(self, Which::Apply | Which::Undo)
    }
}

/// One remedy tool.
pub struct RemedyTool {
    host: Arc<RemedyHost>,
    site: String,
    clock: Arc<dyn Clock>,
    which: Which,
}

impl RemedyTool {
    /// The four tools over `host` for `site`.
    #[must_use]
    pub fn all(
        host: &Arc<RemedyHost>,
        site: &str,
        clock: &Arc<dyn Clock>,
    ) -> Vec<Arc<dyn ExtensionTool>> {
        [Which::List, Which::DryRun, Which::Apply, Which::Undo]
            .into_iter()
            .map(|which| {
                Arc::new(RemedyTool {
                    host: Arc::clone(host),
                    site: site.to_owned(),
                    clock: Arc::clone(clock),
                    which,
                }) as Arc<dyn ExtensionTool>
            })
            .collect()
    }

    /// Takes the four tools off the MCP service.
    pub fn uninstall(mcp: &McpHandle) {
        for name in NAMES {
            mcp.uninstall_tool(name);
        }
    }

    /// Puts the four tools on the MCP service, taking any earlier copy off first (a site built
    /// again on a reload). `Err` is a sentence naming the tool the MCP service refused.
    ///
    /// # Errors
    ///
    /// The sentence naming the first tool that could not be installed; none stays installed.
    pub fn install(
        mcp: &McpHandle,
        host: &Arc<RemedyHost>,
        site: &str,
        clock: &Arc<dyn Clock>,
    ) -> Result<(), String> {
        Self::uninstall(mcp);
        for tool in Self::all(host, site, clock) {
            let name = tool.name().to_owned();
            if let Err(error) = mcp.install_tool(tool) {
                Self::uninstall(mcp);
                return Err(format!(
                    "the tool `{name}` could not be added to the MCP service ({error})"
                ));
            }
        }
        Ok(())
    }

    fn schema(&self) -> Json {
        let text = json!({ "type": "string", "minLength": 1, "maxLength": MAX_TEXT });
        let approval = json!({
            "type": "object",
            "additionalProperties": false,
            "required": ["approver", "ref", "at"],
            "properties": {
                "approver": { "type": "string", "minLength": 1, "maxLength": 128 },
                "ref": { "type": "string", "minLength": 1, "maxLength": 128 },
                "at": { "type": "integer" }
            }
        });
        match self.which {
            Which::List => json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {}
            }),
            Which::DryRun => json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["remedy"],
                "properties": {
                    "remedy": text,
                    "params": { "type": "object" }
                }
            }),
            Which::Apply => json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["plan"],
                "properties": {
                    "plan": {
                        "type": "object",
                        "required": ["remedy", "params", "steps", "hash"]
                    },
                    "approval": approval
                }
            }),
            Which::Undo => json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["remedy", "undo_token"],
                "properties": {
                    "remedy": text,
                    "undo_token": text,
                    "approval": approval
                }
            }),
        }
    }

    fn description(&self) -> &'static str {
        match self.which {
            Which::List => {
                "Lists the catalogued remedies the agent's tier allows, with what each affects, \
                 whether it can be undone and the schema of its parameters."
            }
            Which::DryRun => {
                "Plans a remedy without changing anything and returns the sealed plan. One \
                 dry-run authorises one apply."
            }
            Which::Apply => {
                "Applies a plan that remedy_dry_run returned, unmodified. Returns an undo token \
                 and the end of the undo window."
            }
            Which::Undo => "Undoes an applied remedy by its undo token, inside the undo window.",
        }
    }

    async fn call_as(&self, agent: &str, arguments: &Json) -> Result<Json, ExtensionError> {
        match self.which {
            Which::List => self.list(agent).await,
            Which::DryRun => self.dry_run(agent, arguments).await,
            Which::Apply => self.apply(agent, arguments).await,
            Which::Undo => self.undo(agent, arguments).await,
        }
    }

    async fn list(&self, agent: &str) -> Result<Json, ExtensionError> {
        let remedies = self
            .host
            .list(&self.site, agent)
            .await
            .map_err(|error| host_error(&error))?;
        let remedies: Vec<Json> = remedies.iter().map(summary_json).collect();
        Ok(json!({ "remedies": remedies }))
    }

    async fn dry_run(&self, agent: &str, arguments: &Json) -> Result<Json, ExtensionError> {
        let remedy = remedy_id(arguments)?;
        let params = match arguments.get("params") {
            None | Some(Json::Null) => json!({}),
            Some(params @ Json::Object(_)) => params.clone(),
            Some(_) => return Err(invalid("`params` must be an object")),
        };
        let plan = self
            .host
            .dry_run(&self.site, agent, &remedy, &params, None)
            .await
            .map_err(|error| host_error(&error))?;
        serde_json::to_value(&plan).map_err(|_| internal())
    }

    async fn apply(&self, agent: &str, arguments: &Json) -> Result<Json, ExtensionError> {
        let plan: Plan = arguments
            .get("plan")
            .cloned()
            .and_then(|plan| serde_json::from_value(plan).ok())
            .ok_or_else(|| invalid("`plan` is not a plan"))?;
        let approval = approval_of(arguments)?;
        let route = match self.host.meta(&plan.remedy) {
            Some(meta) if meta.tier <= Tier::T1 && meta.undo.is_reversible() => "direct",
            _ => "pipeline",
        };
        // Read before the apply: the stated deadline is never later than the real one.
        let before = self.clock.now_utc().unix_timestamp();
        let applied = self
            .host
            .apply(&self.site, agent, &plan, approval)
            .await
            .map_err(|error| host_error(&error))?;
        let window = self.host.settings().undo_window_secs;
        let changes: Vec<Json> = applied
            .changes
            .iter()
            .map(|change| {
                json!({ "target": change.target, "before": change.before, "after": change.after })
            })
            .collect();
        Ok(json!({
            "remedy": applied.remedy.as_str(),
            "plan_hash": applied.plan_hash.as_str(),
            "changes": changes,
            "undo_token": applied.undo_token.as_str(),
            "undo_expires_at": before.saturating_add(window),
            "route": route,
        }))
    }

    async fn undo(&self, agent: &str, arguments: &Json) -> Result<Json, ExtensionError> {
        let remedy = remedy_id(arguments)?;
        let token: UndoToken = arguments
            .get("undo_token")
            .cloned()
            .and_then(|token| serde_json::from_value(token).ok())
            .ok_or_else(|| invalid("`undo_token` is not an undo token"))?;
        let approval = approval_of(arguments)?;
        let undone = self
            .host
            .undo(&self.site, agent, &remedy, &token, approval)
            .await
            .map_err(|error| host_error(&error))?;
        let changes: Vec<Json> = undone
            .changes
            .iter()
            .map(|change| {
                json!({ "target": change.target, "before": change.before, "after": change.after })
            })
            .collect();
        Ok(json!({
            "remedy": undone.remedy.as_str(),
            "undo_token": undone.token.as_str(),
            "changes": changes,
        }))
    }
}

impl ExtensionTool for RemedyTool {
    fn name(&self) -> &str {
        self.which.name()
    }

    fn spec(&self, principal: &McpPrincipal) -> Option<ExtensionSpec> {
        agent_of(principal)?;
        let tier = principal.tier()?;
        if self.which.writes() && tier < Tier::T1 {
            return None;
        }
        let spec = ExtensionSpec::new(self.description(), self.schema());
        Some(if self.which.writes() {
            spec.writing()
        } else {
            spec
        })
    }

    fn call<'a>(
        &'a self,
        principal: &'a McpPrincipal,
        arguments: &'a Json,
    ) -> BoxFuture<'a, Result<Json, ExtensionError>> {
        Box::pin(async move {
            let Some(agent) = agent_of(principal) else {
                return Err(ExtensionError::refused(
                    "permission.denied",
                    "the caller may not do this",
                ));
            };
            self.call_as(agent, arguments).await
        })
    }
}

/// The agent id of a principal that is a registered agent and not the platform.
fn agent_of(principal: &McpPrincipal) -> Option<&str> {
    if principal.assert_not_system().is_err() {
        return None;
    }
    principal.agent_id()
}

fn remedy_id(arguments: &Json) -> Result<RemedyId, ExtensionError> {
    let text = arguments
        .get("remedy")
        .and_then(Json::as_str)
        .ok_or_else(|| invalid("`remedy` is required"))?;
    // A text that is not a remedy id is the same refusal as an id nobody registered.
    RemedyId::parse(text).map_err(|_| {
        ExtensionError::refused("remedies.unknown", "no remedy is catalogued under that id")
    })
}

fn approval_of(arguments: &Json) -> Result<Option<Approval>, ExtensionError> {
    match arguments.get("approval") {
        None | Some(Json::Null) => Ok(None),
        Some(approval) => serde_json::from_value(approval.clone())
            .map(Some)
            .map_err(|_| invalid("`approval` is not an approval")),
    }
}

fn invalid(message: &'static str) -> ExtensionError {
    ExtensionError::refused("api.invalid_request", message)
}

fn internal() -> ExtensionError {
    ExtensionError::failed("remedies_host.internal", "the remedy call did not complete")
}

fn summary_json(summary: &RemedySummary) -> Json {
    json!({
        "id": summary.id.as_str(),
        "title": summary.title,
        "tier": summary.tier.as_str(),
        "risk": format!("{:?}", summary.risk).to_lowercase(),
        "affects": summary.affects,
        "preconditions": summary.preconditions,
        "reversible": summary.reversible,
        "params_schema": summary.params_schema,
    })
}

/// The tool error of a host refusal: its code and a fixed sentence (never the parameters, a plan
/// step or a remedy id the caller sent).
fn host_error(error: &HostError) -> ExtensionError {
    let code = error.code();
    let message = sentence(code);
    if failed(code) {
        ExtensionError::failed(code, message)
    } else {
        ExtensionError::refused(code, message)
    }
}

/// Whether a code is a failure of the service rather than a refusal the caller can act on.
fn failed(code: &str) -> bool {
    matches!(
        code,
        "remedies.op_failed"
            | "remedies_host.no_world"
            | "remedies_host.world_unavailable"
            | "remedies_host.audit_unavailable"
            | "remedies_host.undo_store_unavailable"
            | "remedies_host.internal"
    )
}

/// The fixed sentence of a code.
fn sentence(code: &str) -> &'static str {
    match code {
        "remedies.unknown" => "no remedy is catalogued under that id",
        "remedies.refused" => "the agent's tier, domain or status does not allow this remedy",
        "remedies.params_invalid" => "the parameters do not fit the remedy's schema",
        "remedies.precondition_failed" => "the remedy's precondition does not hold",
        "remedies.plan_stale" => {
            "the plan was not issued by a dry-run of this host, was altered, or was already applied"
        }
        "remedies.undo_unknown" => "no undo record exists for that token",
        "remedies.undo_expired" => "the undo window has closed",
        "remedies.undo_conflict" => "the state changed since the apply, so it cannot be undone",
        "remedies.irreversible" => "this remedy cannot be undone",
        "remedies.hard_floor_financial" => "a remedy never changes a submitted financial record",
        "remedies.op_failed" => "the remedy's operation failed",
        "remedies_host.not_an_agent" => "the caller is not a registered agent of this site",
        "remedies_host.no_world" => "no site world is provided, so no remedy can run",
        "remedies_host.world_unavailable" => "the site world could not be opened",
        "remedies_host.apply_closed" => {
            "this remedy must go through the fix pipeline, which is not installed"
        }
        "remedies_host.audit_unavailable" => {
            "the audit chain did not take the record, so nothing changed"
        }
        "remedies_host.undo_store_unavailable" => {
            "the undo record could not be kept, so the change was rolled back"
        }
        _ => "the remedy call did not complete",
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn the_names_are_the_contracts() {
        assert_eq!(
            NAMES,
            [
                "remedy_list",
                "remedy_dry_run",
                "remedy_apply",
                "remedy_undo"
            ]
        );
        assert!(Which::Apply.writes() && Which::Undo.writes());
        assert!(!Which::List.writes() && !Which::DryRun.writes());
    }

    #[test]
    fn every_failure_code_has_a_fixed_sentence_naming_no_value() {
        for code in [
            "remedies.unknown",
            "remedies.refused",
            "remedies.params_invalid",
            "remedies.precondition_failed",
            "remedies.plan_stale",
            "remedies.undo_unknown",
            "remedies.undo_expired",
            "remedies.undo_conflict",
            "remedies.irreversible",
            "remedies.hard_floor_financial",
            "remedies.op_failed",
            "remedies_host.not_an_agent",
            "remedies_host.no_world",
            "remedies_host.world_unavailable",
            "remedies_host.apply_closed",
            "remedies_host.audit_unavailable",
            "remedies_host.undo_store_unavailable",
        ] {
            assert_ne!(
                sentence(code),
                "the remedy call did not complete",
                "{code} has its own sentence"
            );
        }
        assert!(failed("remedies_host.no_world") && !failed("remedies.plan_stale"));
    }

    #[test]
    fn a_text_that_is_not_a_remedy_id_is_the_unknown_remedy_refusal() {
        let error = remedy_id(&json!({"remedy": "Not A Remedy!"})).unwrap_err();
        assert_eq!(error.code, "remedies.unknown");
        assert!(!error.message.contains("Not A Remedy"));
        assert_eq!(
            remedy_id(&json!({})).unwrap_err().code,
            "api.invalid_request"
        );
    }

    #[test]
    fn an_approval_is_a_person_s_reference_and_nothing_else() {
        assert!(
            approval_of(&json!({"approval": {"approver": "ada", "ref": "CHG-1", "at": 7}}))
                .unwrap()
                .is_some()
        );
        assert!(approval_of(&json!({})).unwrap().is_none());
        assert!(approval_of(&json!({"approval": {"approver": "ada", "extra": 1}})).is_err());
    }
}
