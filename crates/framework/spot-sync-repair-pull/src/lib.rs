//! `spot-sync-repair-pull`: the closure repair loop of the sync receiver (WP-SYN-04.T2;
//! ADR-064 sections 7 and 8, ADR-067 section 3).
//!
//! When a parked bundle lacks a record, [`RepairLoop`] asks the hub for exactly those keys
//! (desired state: nothing missing; observed state: the tracked missing set):
//!
//! * asks are emitted on `Tick` only, batched (`max_keys` per request), oldest position first,
//!   and a key is not asked again within `ask_interval` (exponential backoff to `max_interval`
//!   with deterministic jitter from a seed);
//! * after `max_asks_per_key` asks of one key, or more than `escalate_keys` missing at once, one
//!   [`ScopedResend`] is emitted and single asks stay suppressed until the keys arrive (it is
//!   repeated on the backoff schedule if they do not);
//! * a key missing longer than `raise_after` raises `CLOSURE_UNRESOLVED` for `node:<id>` and
//!   the tenant; the condition clears when the tenant's last key arrives;
//! * a key outside the node's scope is never asked: it goes to the lead as
//!   `sync_repair_pull.out_of_scope`, once until it is seen applied (a bounded set per tenant,
//!   `max_out_of_scope`);
//! * memory is bounded: at most `max_tracked` keys per node; beyond it the oldest key of the
//!   tenant with the most tracked keys is dropped (counted) and that tenant escalates so the
//!   dropped key is still repaired; at most `max_tenants` tenants are held;
//! * a key the hub answers `Gone` for ([`Input::Gone`]: the record was deleted) is resolved, not
//!   missing: it is no longer asked for and [`Output::Gone`] hands it to
//!   `NodeIntake::resolve_gone`, which releases the bundles parked for it;
//! * [`Input::ResendComplete`] confirms a scoped re-send: keys still missing are asked for
//!   again one by one, and the tenant's escalation state resets.
//!
//! Everything is per tenant (ADR-061 section 1): the same entity and id missing for two tenants
//! are two tracked keys, asked for separately; a request never mixes tenants. The request names
//! the tenant and keys only. The hub re-checks scope on its side; the node's own check only
//! avoids a refuse-and-retry loop. A pure library over traits: no network, clock, randomness or
//! async runtime. Out of scope: transport and hub-side answering.

mod config;
mod error;
mod model;
mod repair;

pub use config::RepairConfig;
pub use error::{INVALID_CONFIG, OUT_OF_SCOPE, RepairError, TOO_MANY_TENANTS};
pub use model::{
    CLOSURE_UNRESOLVED, ConditionClear, ConditionPort, ConditionRaise, Counters, Input, Output,
    PullRequest, ScopeCheck, ScopedResend, forward_conditions,
};
pub use repair::RepairLoop;
