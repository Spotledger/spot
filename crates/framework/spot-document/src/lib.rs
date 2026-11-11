//! The document engine's value layer (WP-DOC-01.T1; ADR-05, ADR-12, ADR-053).
//!
//! This crate decides what makes a [`Document`](spot_app_api::Document) valid for its DocType
//! and how it maps to and from storage. It is the base of the lifecycle engine (WP-DOC-04).
//! Everything here is a pure function of the effective metadata ([`spot_meta::EffectiveMeta`])
//! and the document; nothing reads the clock or the database.
//!
//! - [`defaults::apply`] fills unset fields from their defaults (`today` and `now` come from the
//!   invocation's time).
//! - [`coerce::normalise`] makes the only two changes allowed before validation.
//! - [`validate::check`] reports every problem at once, each with its field path.
//! - [`mapping::to_storage`] and [`mapping::from_storage`] convert between a `Document` and the
//!   storage contract's documents, carrying the document state in `doc_state`.
//!
//! Functions that need the DocType's definition return an error (`meta.unknown_doctype`) when
//! it is not in the metadata, so each returns an [`AppResult`](spot_app_api::AppResult).
//!
//! # Values are never silently changed
//!
//! Money and quantities must stay exactly what the user entered (ADR-12). Nothing here rounds,
//! trims or folds case. The complete list of coercions, applied by [`coerce::normalise`], is:
//!
//! | Field type | Value found | Becomes |
//! | --- | --- | --- |
//! | `decimal` | whole `Int` | the same number as a `Decimal` |
//! | `int` | `Decimal` without a fractional part | the same number as an `Int`, if it fits |
//!
//! A decimal with more decimal places than its field's scale is an *error* in
//! [`validate::check`], with the code `document.invalid_value`.
//!
//! # Error codes
//!
//! [`validate::check`] uses `document.invalid_value`, `document.required`,
//! `document.unknown_field`, `document.too_long`, `document.invalid_option` and
//! `document.read_only_field` (see the constants in [`validate`]).
//!
//! # The engine
//!
//! [`engine::Engine`] is the real host of the app API (WP-DOC-04): it runs the ADR-51 lifecycle
//! of create, update and delete over any storage backend, with these functions as its value
//! layer.
//!
//! # Secrets
//!
//! A `secret` field is never part of a document: the engine writes it sealed, in the same
//! transaction as its record, through a narrow capability ([`secrets::SecretAccess`], see
//! `Engine::set_secret`, `open_secret`, `find_by_secret` and `rewrap_secrets`). A document, its
//! events, its audit diff and every API response carry only whether it is set.
//!
//! # Not here
//!
//! Permissions are WP-PLT-03; post, cancel, correct, rename and whitelisted methods are
//! WP-DOC-04.T2.

pub mod coerce;
pub mod defaults;
pub mod engine;
pub mod mapping;
pub mod secrets;
pub mod validate;

mod lookup;
mod parse;
