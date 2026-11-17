//! `spot-edge-offline-host`: an edge that works offline (WP-RTB-03.T2 decisions, T7 hosting).
//!
//! The libraries exist (`spot-offline-identity`, `spot-leased-sequences`, `spot-lease-grants`,
//! `spot-shared-balance`); this crate is the host's policy and glue around them. The decisions
//! read no clock and no randomness (every instant is an argument); the only I/O is in
//! [`store_pg`] and [`identity_pg`], on the shared blocking bridge:
//!
//! | Module | Decides |
//! | --- | --- |
//! | [`series`] | when a leased number range is refilled, and the exhaustion error |
//! | [`lease`] | whether the identity lease is alive; a clock moved back never extends it |
//! | [`balance`] | whether an offline spend is allowed under the balance policy (default `OnlineOnly`) |
//! | [`carrier`] | the typed lease request and answer records on the sync stream |
//! | [`kinds`] | the three wire kinds, in one place |
//! | [`carrier_loop`] | the loop that exchanges them over a `CarrierLink` port (T7) |
//! | [`ranges`], [`store_pg`], [`sequences`] | the held ranges, their durable store, the naming path over them (T7) |
//! | [`identity`], [`identity_pg`] | the offline identity snapshot keeper and its state slots (T7) |
//! | [`edge_balances`] | the edge's shared-balance view behind the policy gate (T7) |
//! | [`readiness`] | the operator line |
//!
//! # Fail closed
//!
//! Every decision here errs toward refusing: an exhausted series refuses document creation (never
//! reusing a number), an expired lease refuses sign-in and privileged actions, an unknown balance
//! policy is `OnlineOnly`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod balance;
pub mod carrier;
pub mod carrier_loop;
pub mod edge_balances;
pub mod identity;
pub mod identity_pg;
pub mod kinds;
pub mod lease;
pub mod ranges;
pub mod readiness;
pub mod sequences;
pub mod series;
pub mod store_pg;

mod blocking;

/// The name of this crate.
pub const CRATE_NAME: &str = "spot-edge-offline-host";
