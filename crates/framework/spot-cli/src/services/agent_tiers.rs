//! Agent identity and tiers on a running site (WP-AI-06.T4; blueprint WS13): the tier authoriser in
//! the MCP call path, registered agents and support sessions stored durably, and every out-of-tier
//! action refused and audited.
//!
//! # What it does
//!
//! - **The tier comes from stored data.** A user is an agent only if a registration row names its
//!   user id (`spot_agent_tier_registration`); the tier is that row's, never a header or a tool argument.
//!   The MCP service asks this module's [`AgentDirectory`] who the caller is on every request; a
//!   store failure answers "an agent at T0", never "a human".
//! - **T1 and above are closed.** While `services.agent_tiers.unattended_policy_open` is `false`
//!   (the default, until the M9 gate task flips it) every agent runs at T0: it explains and reads, and
//!   anything that changes data is refused, even for an agent registered T2 or deep.
//! - **A gate in front of every `tools/call`** ([`CallGate`]): a suspended or retired agent, a tool
//!   above the agent's tier, and for a support agent the lack of a live session that covers the tool
//!   (tenant, window, ceiling, scope, not revoked) are refused as a permission error. Each refusal is
//!   appended to the `mcp-<site>` audit chain (`agent.refusal`: agent, tier the tool needed, tool,
//!   code; never arguments) by this module, whether or not the caller's own record is written.
//! - **Support sessions** are stored as the `spot-support-session` grant (JSON); expiry is derived
//!   from the stored times at each call, extension needs a new approval (`extend_session`), revocation
//!   is read on the next call. A session is per tenant: its tenant must be this site.
//! - **Registrations and sessions survive a restart** (three tables of the site's database, created
//!   idempotently at start); every registry change is a row in `spot_agent_tier_event`.
//!
//! # Level
//!
//! Optional. If the tables cannot be created the site serves and readiness says `degraded:
//! agent_tiers (...)`; the MCP call gate is **not installed**, so MCP agents are left with no
//! registration lookup: the module therefore closes `/mcp` for the site (503) instead, because an
//! agent path with no tier source is an unguarded one. The module is inert without the `mcp` module
//! (`services.mcp.enabled = false`) and with `services.agent_tiers.enabled = false`.
//!
//! T1/T2 unattended execution and break glass are not wired here (milestone M9 and cluster C).

use std::sync::Arc;

use serde::Deserialize;
use spot_agent_tiers::{
    AgentIdentity as TierIdentity, AgentRegistry, Domain, RegistryEvent, Status, Tier,
};
use spot_audit_chain::{AuditRecord, Payload, PayloadValue};
use spot_core::Clock;
use spot_mcp::AuditSink;
use spot_support_session::{Approver, SessionGrant, SessionPolicy};

use crate::services::mcp::seams::{
    AgentDirectory, AgentIdentity, CallGate, Denied, Fut, GateCall, McpHandle,
};
use crate::services::{Idle, Level, SiteCtx};
use crate::{Error, Result};

#[path = "agent_tiers/gate.rs"]
pub mod gate;
#[path = "agent_tiers/store.rs"]
pub mod store;

use store::{Registration, Store, StoreError};

/// Start order: after the `mcp` module (1060), whose port it plugs into.
pub const ORDER: u32 = 1070;

fn default_true() -> bool {
    true
}

/// `[services.agent_tiers]`.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Host the tier service. Default true; when `services.mcp` is off this module does nothing.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Whether agents may run above T0 (the M9 gate). Default **false**: every agent runs at T0.
    #[serde(default)]
    pub unattended_policy_open: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            unattended_policy_open: false,
        }
    }
}

/// What the service could not do, as a fixed sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TierError(pub String);

impl std::fmt::Display for TierError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TierError {}

impl From<StoreError> for TierError {
    fn from(e: StoreError) -> Self {
        TierError(e.0.to_owned())
    }
}

impl From<spot_agent_tiers::TierError> for TierError {
    fn from(e: spot_agent_tiers::TierError) -> Self {
        // The crate's messages name ids and domains, never a secret; keep the sentence.
        TierError(e.to_string())
    }
}

impl From<spot_support_session::SessionError> for TierError {
    fn from(e: spot_support_session::SessionError) -> Self {
        TierError(e.to_string())
    }
}

/// The agent tier service of one site: the operations a control plane (or an operator tool) calls,
/// and the directory and gate the MCP service consults.
pub struct TierService {
    site: String,
    store: Store,
    policy_open: bool,
    clock: Arc<dyn Clock>,
    sink: Arc<dyn AuditSink>,
}

impl TierService {
    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    /// Registers an agent (a user id, its owner, domain and ceiling). Never deep.
    ///
    /// # Errors
    ///
    /// [`TierError`] when the id is empty, the agent is already registered or the write fails.
    pub async fn enroll(
        &self,
        agent_id: &str,
        owner: &str,
        domain: Domain,
        max_tier: Tier,
        by: &str,
    ) -> std::result::Result<(), TierError> {
        // The crate's registry validates the record exactly as it does everywhere else.
        AgentRegistry::new().enroll(TierIdentity::new(agent_id, owner, domain, max_tier))?;
        if self.store.agent(agent_id).await?.is_some() {
            return Err(TierError("the agent is already registered".to_owned()));
        }
        let now = self.now();
        self.store
            .put_agent(
                &Registration {
                    agent_id: agent_id.to_owned(),
                    owner: owner.to_owned(),
                    domain,
                    max_tier,
                    deep: false,
                    status: Status::Active,
                },
                now,
            )
            .await?;
        self.store
            .event("agent.enrolled", agent_id, by, "", "", now)
            .await?;
        Ok(())
    }

    async fn registration(&self, agent_id: &str) -> std::result::Result<Registration, TierError> {
        self.store
            .agent(agent_id)
            .await?
            .ok_or_else(|| TierError("no such agent".to_owned()))
    }

    /// Registers an agent deep: only for a support agent, with evidence, and with its audit event.
    ///
    /// # Errors
    ///
    /// [`TierError`] for an unknown agent, a non-support domain, an agent already deep or a failed write.
    pub async fn register_deep(
        &self,
        agent_id: &str,
        by: &str,
        evidence_ref: &str,
    ) -> std::result::Result<RegistryEvent, TierError> {
        let mut reg = self.registration(agent_id).await?;
        let now = self.now();
        // Let the crate's registry decide, from a copy of the stored facts.
        let mut registry = AgentRegistry::new();
        registry.enroll(
            TierIdentity::new(&reg.agent_id, &reg.owner, reg.domain, reg.max_tier)
                .with_status(reg.status),
        )?;
        if reg.deep {
            return Err(TierError("the agent is already registered deep".to_owned()));
        }
        let event = registry.register_deep(agent_id, by, evidence_ref, now)?;
        reg.deep = true;
        self.store.put_agent(&reg, now).await?;
        self.store
            .event(&event.event, agent_id, by, evidence_ref, "", now)
            .await?;
        Ok(event)
    }

    /// Revokes the deep registration.
    ///
    /// # Errors
    ///
    /// [`TierError`] for an unknown agent, one that is not deep, or a failed write.
    pub async fn revoke_deep(
        &self,
        agent_id: &str,
        by: &str,
    ) -> std::result::Result<RegistryEvent, TierError> {
        let mut reg = self.registration(agent_id).await?;
        if !reg.deep {
            return Err(TierError("the agent is not registered deep".to_owned()));
        }
        let now = self.now();
        reg.deep = false;
        self.store.put_agent(&reg, now).await?;
        let event = RegistryEvent {
            event: "agent.deep_revoked".to_owned(),
            agent_id: agent_id.to_owned(),
            by: by.to_owned(),
            evidence_ref: String::new(),
            at: now,
        };
        self.store
            .event(&event.event, agent_id, by, "", "", now)
            .await?;
        Ok(event)
    }

    /// Suspends, reactivates or retires an agent; effective on its next call.
    ///
    /// # Errors
    ///
    /// [`TierError`] for an unknown agent or a failed write.
    pub async fn set_status(
        &self,
        agent_id: &str,
        status: Status,
        by: &str,
    ) -> std::result::Result<(), TierError> {
        let mut reg = self.registration(agent_id).await?;
        let now = self.now();
        reg.status = status;
        self.store.put_agent(&reg, now).await?;
        self.store
            .event(
                &format!("agent.{}", status.as_str()),
                agent_id,
                by,
                "",
                "",
                now,
            )
            .await?;
        Ok(())
    }

    /// Stores an approved session grant (built by `SessionGrant::request` and `approve`).
    ///
    /// # Errors
    ///
    /// [`TierError`] when the grant is about another tenant or the write fails.
    pub async fn save_session(
        &self,
        grant: &SessionGrant,
        by: &str,
    ) -> std::result::Result<(), TierError> {
        let now = self.now();
        self.store.put_session(grant, now).await?;
        self.store
            .event("session.saved", &grant.subject.id, by, &grant.id, "", now)
            .await?;
        Ok(())
    }

    /// Revokes a session: final, and read by the very next call.
    ///
    /// # Errors
    ///
    /// [`TierError`] for an unknown or already revoked session, or a failed write.
    pub async fn revoke_session(
        &self,
        id: &str,
        by: &str,
        reason: &str,
    ) -> std::result::Result<(), TierError> {
        let mut grant = self
            .store
            .session(id)
            .await?
            .ok_or_else(|| TierError("no such session".to_owned()))?;
        let now = self.now();
        let event = grant.revoke(by, reason, now)?;
        self.store.put_session(&grant, now).await?;
        let detail = serde_json::to_string(&event).unwrap_or_default();
        self.store
            .event("session.revoked", &grant.subject.id, by, id, &detail, now)
            .await?;
        Ok(())
    }

    /// Extends a session by a **new approval**; the policy decides (distinct approver, ceiling,
    /// duration from the original start, extension count).
    ///
    /// # Errors
    ///
    /// [`TierError`] for an unknown session, a refused extension or a failed write.
    pub async fn extend_session(
        &self,
        id: &str,
        new_expires_at: i64,
        approver: &Approver,
        policy: &SessionPolicy,
    ) -> std::result::Result<(), TierError> {
        let mut grant = self
            .store
            .session(id)
            .await?
            .ok_or_else(|| TierError("no such session".to_owned()))?;
        let now = self.now();
        let event = grant.extend(new_expires_at, approver, now, policy)?;
        self.store.put_session(&grant, now).await?;
        let detail = serde_json::to_string(&event).unwrap_or_default();
        self.store
            .event(
                "session.extended",
                &grant.subject.id,
                &approver.id,
                id,
                &detail,
                now,
            )
            .await?;
        Ok(())
    }

    /// The store, for the tests and for an operator tool that lists.
    #[must_use]
    pub fn store(&self) -> &Store {
        &self.store
    }

    async fn refuse(&self, call: &GateCall, agent_id: &str, tier: &str, code: &str) {
        let mut payload = Payload::new();
        payload.insert("tool".to_owned(), PayloadValue::Text(call.tool.clone()));
        payload.insert("tier".to_owned(), PayloadValue::Text(tier.to_owned()));
        payload.insert("code".to_owned(), PayloadValue::Text(code.to_owned()));
        payload.insert(
            "outcome".to_owned(),
            PayloadValue::Text("refused".to_owned()),
        );
        let record = AuditRecord {
            kind: "agent.refusal".to_owned(),
            actor: format!("agent:{agent_id}"),
            on_behalf_of: None,
            subject: None,
            at: call.now_unix.saturating_mul(1_000_000),
            payload,
            masked: true,
        };
        if self.sink.record(record).await.is_err() {
            // The refusal stands whether or not the record could be written; say so loudly.
            tracing::error!(
                site = %self.site,
                code = "agent_tiers.refusal_not_audited",
                "a refusal could not be appended to the audit chain"
            );
        }
        let _ = self
            .store
            .event(
                "agent.refused",
                agent_id,
                "",
                &call.tool,
                code,
                call.now_unix,
            )
            .await;
    }
}

impl AgentDirectory for TierService {
    fn resolve<'a>(&'a self, tenant: &'a str, user: &'a str) -> Fut<'a, Option<AgentIdentity>> {
        Box::pin(async move {
            if tenant != self.site {
                // Another tenant's id never matches a row of this site's store.
                return None;
            }
            match self.store.agent(user).await {
                Ok(None) => None,
                Ok(Some(reg)) => Some(AgentIdentity {
                    agent_id: reg.agent_id.clone(),
                    tier: gate::effective_tier(&reg, self.policy_open),
                }),
                // The lookup failed: act as an agent at the lowest tier, never as an unchecked human.
                Err(_) => Some(AgentIdentity {
                    agent_id: user.to_owned(),
                    tier: Tier::T0,
                }),
            }
        })
    }
}

impl CallGate for TierService {
    fn check<'a>(&'a self, call: &'a GateCall) -> Fut<'a, std::result::Result<(), Denied>> {
        Box::pin(async move {
            let Some(agent_id) = call.agent_id.as_deref() else {
                return Ok(());
            };
            let need = gate::classify(&call.tool);
            let deny = |code: &str| Denied {
                code: code.to_owned(),
                message: gate::Refused(code.to_owned()).sentence(),
            };
            if call.tenant != self.site {
                self.refuse(call, agent_id, need.tier.as_str(), "agent.wrong_tenant")
                    .await;
                return Err(deny("agent.wrong_tenant"));
            }
            let outcome = async {
                let reg = self
                    .store
                    .agent(agent_id)
                    .await
                    .map_err(|_| gate::Refused("agent.unavailable".to_owned()))?
                    .ok_or_else(|| gate::Refused("agent.unregistered".to_owned()))?;
                let sessions = if reg.domain == Domain::Support {
                    self.store
                        .sessions_of(agent_id)
                        .await
                        .map_err(|_| gate::Refused("agent.unavailable".to_owned()))?
                } else {
                    Vec::new()
                };
                gate::decide(
                    &reg,
                    &sessions,
                    &self.site,
                    &call.tool,
                    call.now_unix,
                    self.policy_open,
                )
            }
            .await;
            match outcome {
                Ok(()) => Ok(()),
                Err(refused) => {
                    self.refuse(call, agent_id, need.tier.as_str(), refused.code())
                        .await;
                    Err(deny(refused.code()))
                }
            }
        })
    }
}

/// Registers the agent tier service: its tables, its readiness entry, and its hooks in the MCP
/// service. Inert when `services.agent_tiers.enabled = false` or when the `mcp` module provides no
/// handle (`services.mcp.enabled = false`).
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("agent_tiers")?;
    if !settings.enabled {
        return Ok(());
    }
    let Some(handle) = ctx.port::<McpHandle>() else {
        return Ok(());
    };
    if !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site = env.site.site.clone();
    let store = Store::new(env.backend.pool(), &site);
    let service = Arc::new(TierService {
        site: site.clone(),
        store: store.clone(),
        policy_open: settings.unattended_policy_open,
        clock: Arc::clone(ctx.clock()),
        sink: crate::services::mcp::seams::audit_sink(env.backend, &site),
    });
    // The tables first: the gate must never run over tables that are not there.
    match store.prepare().await {
        Ok(()) => {
            handle.install_directory(Arc::clone(&service) as Arc<dyn AgentDirectory>);
            handle.install_call_gate(Arc::clone(&service) as Arc<dyn CallGate>);
        }
        Err(e) => {
            // No tier source: an agent path without one is an unguarded one, so MCP is closed.
            handle.close("the agent tier tables are not available");
            let reason = e.to_string();
            ctx.add(Idle::new("agent_tiers", Level::Optional, move || {
                let reason = reason.clone();
                Box::pin(async move { Err(reason) })
            }));
            return Ok(());
        }
    }
    let probe = store.clone();
    ctx.add(Idle::new("agent_tiers", Level::Optional, move || {
        let probe = probe.clone();
        Box::pin(async move { probe.agent("").await.map(|_| ()).map_err(|e| e.to_string()) })
    }));
    ctx.provide(service)
        .map_err(|e| Error(format!("services.agent_tiers: {e}")))?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_on_with_the_unattended_policy_closed() {
        let s = Settings::default();
        assert!(s.enabled);
        assert!(!s.unattended_policy_open);
        let parsed: Settings = toml::from_str("").unwrap();
        assert!(parsed.enabled);
        assert!(!parsed.unattended_policy_open);
        assert!(toml::from_str::<Settings>("nope = 1").is_err());
    }
}
