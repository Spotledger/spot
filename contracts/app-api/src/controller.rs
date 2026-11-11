//! Controllers: a DocType's behaviour as named pipeline steps and whitelisted methods (ADR-16).
//!
//! A DocType may have several controllers: its own app's, plus **controller extensions** from
//! other apps (ADR-17). All their steps on one hook point run in order of `priority` (lower
//! first), then step name. Steps can be overridden without touching their code through
//! [`StepOverride`](crate::StepOverride)s (ADR-08). A DocType with no controller at all still
//! works: the host's default controller has no steps (WP-DOC-10).

use std::fmt;
use std::time::Duration;

use serde_json::Value as Json;
use spot_storage_contract::Name;

use crate::BoxFuture;
use crate::context::{HookContext, MethodContext};
use crate::effects::StepEffects;
use crate::error::{AppError, AppResult, ErrorClass};
use crate::lifecycle::HookPoint;

/// A step's globally unique name: `<app>:<step>`, both storage-style names, for example
/// `erp_selling:check_credit_limit`. Overrides and the admin listing refer to steps by it.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(try_from = "String", into = "String")]
pub struct StepName(String);

impl StepName {
    /// Validates a step name.
    pub fn new(s: &str) -> AppResult<Self> {
        let ok = s
            .split_once(':')
            .is_some_and(|(a, n)| Name::new(a).is_ok() && Name::new(n).is_ok());
        if ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(AppError::new(
                ErrorClass::Internal,
                "platform.invalid_step_name",
                format!("step names are <app>:<step>, got {s:?}"),
            ))
        }
    }

    /// The app part.
    pub fn app(&self) -> &str {
        self.0.split_once(':').map_or("", |(a, _)| a)
    }

    /// The name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for StepName {
    type Error = AppError;
    fn try_from(s: String) -> AppResult<Self> {
        Self::new(&s)
    }
}

impl From<StepName> for String {
    fn from(v: StepName) -> String {
        v.0
    }
}

impl fmt::Display for StepName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One step a controller contributes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepDecl {
    /// Its name.
    pub name: StepName,
    /// Where it runs.
    pub point: HookPoint,
    /// Lower runs first; ties by name. Platform steps use multiples of 100 so apps can go
    /// between them.
    pub priority: i32,
    /// Longest time it may take; the host stops and fails it after that (ADR-51).
    pub budget: Duration,
}

/// The right a whitelisted method needs (ADR-16; app API 0.6.0, WP-DOC-11.T2).
///
/// The engine authorises a call by this right and not by [`MethodDecl::mutates`]: `Read` for a
/// read, `Write` for an update, `Create` for a create, `Submit` for a post, `Cancel` for a
/// cancel, and `Permission` for an app-registered permission, by name. The set is
/// `#[non_exhaustive]`: a later operation adds a variant additively.
///
/// # The consistency rule
///
/// `mutates` says whether a call changes documents, so the two must agree: `Read` requires
/// `mutates == false`; `Write`, `Create`, `Submit` and `Cancel` require `mutates == true`; and a
/// `Permission` implies `mutates == true` unless the declaration states otherwise, because a
/// named permission may guard a read-only method (a report, say) and the declaration says so by
/// keeping `mutates == false`. [`MethodRight::contradicts`] answers the question for one
/// declaration and
/// [`check_method_rights`](crate::contract_tests::check_method_rights) checks every method of a
/// controller.
///
/// On the wire it is the native API's `right` value: `"read"`, `"write"`, `"create"`,
/// `"submit"`, `"cancel"`, or `{"permission": "<name>"}`.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(into = "MethodRightWire", try_from = "MethodRightWire")]
#[non_exhaustive]
pub enum MethodRight {
    /// Read the document.
    Read,
    /// Update the document.
    Write,
    /// Create a document.
    Create,
    /// Post a submitted document.
    Submit,
    /// Cancel a posted document.
    Cancel,
    /// An app-registered permission, by name.
    Permission(Name),
}

impl MethodRight {
    /// The right as the native API's `right` values and `#[whitelist(right = "...")]` spell it;
    /// for [`MethodRight::Permission`] the permission's name.
    pub fn as_str(&self) -> &str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Create => "create",
            Self::Submit => "submit",
            Self::Cancel => "cancel",
            Self::Permission(name) => name.as_str(),
        }
    }

    /// Whether a method with this right changes documents when the declaration does not say
    /// otherwise: false only for [`MethodRight::Read`].
    pub fn implies_mutates(&self) -> bool {
        !matches!(self, Self::Read)
    }

    /// The app permission's name, if this is one.
    pub fn permission(&self) -> Option<&Name> {
        match self {
            Self::Permission(name) => Some(name),
            _ => None,
        }
    }

    /// Whether declaring this right with `mutates` contradicts the consistency rule: `Read` on
    /// a method that changes documents, or any other right on one that does not. A `Permission`
    /// never contradicts it, because a named permission may guard either kind of method.
    pub fn contradicts(&self, mutates: bool) -> bool {
        match self {
            Self::Read => mutates,
            Self::Permission(_) => false,
            _ => !mutates,
        }
    }
}

impl fmt::Display for MethodRight {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The wire form of [`MethodRight`]: its name, or `{"permission": "<name>"}`.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum MethodRightWire {
    Read,
    Write,
    Create,
    Submit,
    Cancel,
    Permission(String),
}

impl From<MethodRight> for MethodRightWire {
    fn from(v: MethodRight) -> Self {
        match v {
            MethodRight::Read => Self::Read,
            MethodRight::Write => Self::Write,
            MethodRight::Create => Self::Create,
            MethodRight::Submit => Self::Submit,
            MethodRight::Cancel => Self::Cancel,
            MethodRight::Permission(name) => Self::Permission(name.as_str().to_owned()),
        }
    }
}

impl TryFrom<MethodRightWire> for MethodRight {
    type Error = String;

    fn try_from(v: MethodRightWire) -> Result<Self, String> {
        Ok(match v {
            MethodRightWire::Read => Self::Read,
            MethodRightWire::Write => Self::Write,
            MethodRightWire::Create => Self::Create,
            MethodRightWire::Submit => Self::Submit,
            MethodRightWire::Cancel => Self::Cancel,
            MethodRightWire::Permission(name) => Self::Permission(
                Name::new(&name).map_err(|e| format!("`{name}` is not a permission name: {e}"))?,
            ),
        })
    }
}

/// A whitelisted method (ADR-16).
///
/// Build it with [`MethodDecl::new`], which derives the right from `mutates`, and state a finer
/// right with [`MethodDecl::with_right`]. A struct literal works too, but then keeping `right`
/// and `mutates` consistent is the caller's job: see [`MethodRight`] for the rule and
/// [`MethodDecl::contradicts`] for the check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodDecl {
    /// Its name, a storage-style name.
    pub name: Name,
    /// Whether it changes documents; the native API then requires `If-Match`.
    pub mutates: bool,
    /// The right it needs (0.6.0). A declaration that does not set it gets [`MethodRight::Read`]
    /// when it does not mutate and [`MethodRight::Write`] when it does.
    pub right: MethodRight,
}

impl MethodDecl {
    /// A declaration whose right follows from `mutates`: [`MethodRight::Read`] when the method
    /// does not change documents, [`MethodRight::Write`] when it does.
    #[must_use]
    pub fn new(name: Name, mutates: bool) -> Self {
        let right = if mutates {
            MethodRight::Write
        } else {
            MethodRight::Read
        };
        Self {
            name,
            mutates,
            right,
        }
    }

    /// The same declaration with the right it needs: `Read`, `Write`, `Create`, `Submit`,
    /// `Cancel`, or an app permission by name. It does not change `mutates`: a right that
    /// contradicts it is refused by [`MethodDecl::contradicts`] rather than silently fixed.
    #[must_use]
    pub fn with_right(mut self, right: MethodRight) -> Self {
        self.right = right;
        self
    }

    /// Whether the declared right contradicts `mutates` (see [`MethodRight::contradicts`]).
    #[must_use]
    pub fn contradicts(&self) -> bool {
        self.right.contradicts(self.mutates)
    }
}

/// A DocType's controller. Native Rust, WASM and TypeScript controllers all implement it
/// (through host adapters for the last two).
pub trait Controller: Send + Sync {
    /// The DocType it belongs to.
    fn doctype(&self) -> &str;

    /// Its steps. Read once when the controller is registered.
    fn steps(&self) -> Vec<StepDecl>;

    /// The effects its steps declare (ADR-080 section 6, contract 0.5.0). Default: none, which
    /// means "no declared effects". Check them with [`validate_declarations`](crate::effects::validate_declarations).
    fn step_effects(&self) -> Vec<StepEffects> {
        Vec::new()
    }

    /// Its whitelisted methods.
    fn methods(&self) -> Vec<MethodDecl> {
        Vec::new()
    }

    /// Runs one of its steps.
    fn run_step<'a>(
        &'a self,
        step: &'a StepName,
        ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>>;

    /// Runs one of its whitelisted methods with JSON arguments; returns the result as JSON.
    fn call_method<'a>(
        &'a self,
        method: &'a str,
        args: Json,
        ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        let _ = (args, ctx);
        Box::pin(async move {
            Err(AppError::new(
                ErrorClass::MethodNotFound,
                "method.not_found",
                format!("no method {method}"),
            ))
        })
    }
}
