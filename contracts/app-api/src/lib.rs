//! App API contract v0.3 for the Spotledger Platform (WP-CON-02, ADR-057 as amended by
//! ADR-068, field security from amendment A1).
//!
//! This is the **only** crate an app may depend on (ADR-06, gate G4). Native ERP crates, WASM
//! apps (through the WIT world, WP-EXT-05) and TypeScript scripts (WP-EXT-08) all see the same
//! concepts: the same document handle, the same lifecycle hooks with the same phase rules, the
//! same messages and typed errors (blueprint §2, "One host API").
//!
//! # What an app works with
//!
//! - [`Document`]: the dynamic document handle: fields, child tables, state, version, and what
//!   changed since it was loaded (ADR-05). [`DocView`] is the typed view generated over it
//!   (WP-MET-04); custom fields stay reachable dynamically.
//! - [`DocumentPatch`]: a merge patch with the native API's semantics (ADR-055).
//! - [`lifecycle`]: the ADR-51 operations, phases and hook points, the exact order they run in
//!   ([`lifecycle::sequence`]), and what each phase may do ([`lifecycle::PhaseRules`]).
//! - [`Controller`]: a DocType's behaviour: named pipeline steps on hook points and whitelisted
//!   methods (ADR-16). Native, WASM and script controllers implement the same trait.
//! - [`HookContext`]: what a step gets: the document, the [`Repository`] (documents and queries,
//!   subject to the phase rules), messages, jobs, realtime notices and the cache.
//! - [`Seam`]: named, versioned service seams other apps can override (ADR-08, WP-EXT-02).
//! - [`AppError`] and [`Message`]: typed, explained errors and non-fatal messages with field
//!   paths (ADR-26), mapped onto the native API's error registry.
//!
//! # Hosts
//!
//! The platform's document engine (WP-DOC-04, WP-DOC-10) is the real host. [`stub::StubHost`] is
//! an in-memory host with the same semantics, for app tests and for WP-TST-10. Both must pass
//! [`contract_tests::run_all`].
//!
//! # Object safety
//!
//! Every trait here is object-safe, and async methods return [`BoxFuture`]. The storage contract
//! avoids boxing on its hot path (ADR-053); the app API boxes once per hook or call, which is
//! what lets native, WASM and script code sit behind the same `dyn` interfaces.

use std::future::Future;
use std::pin::Pin;

pub mod context;
pub mod contract_tests;
pub mod controller;
pub mod document;
pub mod effects;
pub mod error;
pub mod lifecycle;
pub mod lookup;
pub mod message;
pub mod meta;
pub mod seams;
pub mod stub;
pub mod view;

pub use context::{
    Actor, HookContext, Invocation, JobRequest, MethodContext, PrincipalKind, PrincipalRef,
    Repository,
};
pub use controller::{Controller, MethodDecl, MethodRight, StepDecl, StepName};
pub use document::{ChildRow, DocState, Document, DocumentPatch, RowPatch};
pub use effects::{
    ActionRef, EffectClassName, EffectDecl, EffectKind, EffectReceipt, Hex32, OpaqueRef,
    OperationReceipt, ReceiptState, Reversal, Reversibility, StepEffects, StoreName,
};
pub use error::{AppError, AppResult, ErrorBody, ErrorClass, FieldError, FieldPath};
pub use lifecycle::{HookPoint, Operation, Phase, PhaseRules, Stage, Target};
pub use lookup::{
    LOOKUP_VERSION, LookupPage, LookupRequest, LookupRow, LookupRule, LookupSpec,
    PLATFORM_LINK_SEARCH, lookup_seam,
};
pub use message::{Level, Message};
pub use meta::{
    AttachFormat, ControlHint, DecimalFormat, DocTypeDef, FieldDef, FieldHints, FieldKind,
    FieldSecurity, GroupName, IntFormat, JsonFormat, LongTextFormat, MaskFormat,
    ProtectionGroupDef, SyncClassHint, Tag, TextFormat,
};
pub use seams::{OverrideKind, Seam, Seams, StepOverride};
pub use view::DocView;

/// Re-exports apps need from lower layers, so they never depend on those crates directly (G4).
pub mod prelude {
    pub use rust_decimal::Decimal;
    pub use spot_core::{Clock, SiteId};
    pub use spot_events_contract::CommandId;
    pub use spot_storage_contract::{
        AggFunc, CmpOp, Expr, FieldRef, Join, JoinKind, Name, Nulls, Order, OrderDir, Query,
        QueryPath, RecordId, Rows, Selection, Source, TextMatch, TextMode, Value, Version,
    };
    pub use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time};
}

/// A boxed, `Send` future: the return type of every async method in this contract.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The semantic version of this contract. Implementers pin it (blueprint §4).
pub const CONTRACT_VERSION: &str = "0.7.0";
