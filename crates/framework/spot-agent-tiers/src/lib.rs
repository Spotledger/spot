//! The authoriser for AI agents (blueprint WS13, WP-AI-06.T1, ADR-26): agent identity with
//! a tier, the T0 to T3 ladder, registered deep agents, support versus operations
//! separation, and a pure decision core that refuses every out-of-tier action and records
//! every decision — allowed or refused — through a sink.
//!
//! # Shape
//!
//! - [`tier`]: [`Tier`](tier::Tier), the four-tier ladder, and
//!   [`unattended_allowed`](tier::Tier::unattended_allowed).
//! - [`action`]: [`ActionSpec`](action::ActionSpec) (`name`, `tier`, `domain`, `effect`),
//!   [`Effect`](action::Effect) with its minimum-tier table, [`Domain`](action::Domain),
//!   and [`ActionCatalogue`](action::ActionCatalogue) loaded from TOML.
//! - [`identity`]: [`AgentIdentity`](identity::AgentIdentity) — id, owner, domain, max
//!   tier, deep flag, [`Status`](identity::Status) — the ADR-070 `agent` identity kind.
//! - [`registry`]: [`AgentRegistry`](registry::AgentRegistry); enrolling agents from data,
//!   and the only path to `registered_deep`:
//!   [`register_deep`](registry::AgentRegistry::register_deep) /
//!   [`revoke_deep`](registry::AgentRegistry::revoke_deep), each returning an audit event.
//! - [`authoriser`]: [`Authoriser`](authoriser::Authoriser) with the nine checks, first
//!   failing check wins: [`Decision`](authoriser::Decision),
//!   [`RefusalReason`](authoriser::RefusalReason) with its stable `agent.*` codes,
//!   [`Context`](authoriser::Context), [`Approval`](authoriser::Approval),
//!   [`FinancialRoute`](authoriser::FinancialRoute), [`Policy`](authoriser::Policy).
//! - [`sink`]: the record that never skips a decision:
//!   [`DecisionSink`](sink::DecisionSink),
//!   [`authorise_and_record`](sink::Authoriser::authorise_and_record) — one record per
//!   call, always, refusals first of all — and [`MemorySink`](sink::MemorySink) for tests.
//! - [`order`]: the nine checks' order in one table, the property test's reference.
//! - [`error`]: [`TierError`](error::TierError) for catalogue loads and registry changes.
//!
//! # Rules (the behaviour this crate is, in our own words)
//!
//! - **An unknown action is refused, never allowed by default.**
//! - **The tiers**: T0 explains, T1 configures (reversible settings), T2 repairs data
//!   (catalogued remedies), T3 changes structure (customizations, scripts, app installs,
//!   migrations). T0 to T2 may run unattended; T3 only for a registered deep agent or with
//!   a human approval carried in the request.
//! - **Support and operations never cross**: a support agent runs support actions, an
//!   operations agent operations actions.
//! - **The hard floor binds every agent, deep ones included**: a change is refused unless
//!   the change journal is on and a snapshot is taken; a submitted financial record
//!   changes only through cancel, amend or a journal entry — a direct edit is always
//!   refused.
//! - **Every decision is recorded**, allowed or refused, refusals first of all. The record
//!   carries the action's name, never its arguments.
//! - **The deep flag is registry-only**: a data record cannot arrive deep, and
//!   `register_deep` is the only setter, with an audit event to show for it.
//!
//! # Purity
//!
//! No storage, no clock (the caller passes `now: i64`), no network, no floats, no
//! `unsafe`, no `unwrap`/`expect` outside tests (the project coding rules).
//!
//! # Wording
//!
//! The approver is a person; where a request names an agent as approver, or — when
//! [`Policy::separate_approver`](authoriser::Policy::separate_approver) is set — the
//! agent's owner tries to bless its own request, [`Decision::Refuse`] names
//! `agent.approval_invalid`. "The agent's owner acting on the agent's behalf" is the
//! blueprint's phrase; here it means the owner standing as the approver.

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod action;
pub mod authoriser;
pub mod error;
pub mod identity;
pub mod order;
pub mod registry;
#[cfg(test)]
mod registry_tests;
pub mod sink;
pub mod tier;

pub use action::{ActionCatalogue, ActionSpec, Domain, Effect};
pub use authoriser::{
    AllowReason, Approval, Authoriser, Context, Decision, FinancialRoute, Policy, RefusalReason,
};
pub use error::TierError;
pub use identity::{AgentIdentity, Owner, Status};
pub use order::CHECK_ORDER;
pub use registry::{AgentRegistry, RegistryEvent};
pub use sink::{DecisionRecord, DecisionSink, MemorySink};
pub use tier::Tier;

/// The name of this crate, for diagnostics and record headers.
pub const CRATE_NAME: &str = "spot-agent-tiers";
