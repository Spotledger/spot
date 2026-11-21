//! Extension without monkey patching (ADR-08): step overrides and service seams.
//!
//! - A [`StepOverride`] changes the pipeline around a named step: run another step **before**
//!   or **after** it, **wrap** it, **replace** it, or **disable** it. Overrides are declared in
//!   the app manifest (WP-EXT-04); conflicting overrides are refused at install (WP-EXT-03).
//! - A [`Seam`] is a named, versioned service another app can provide or replace, such as a tax
//!   calculator. Calls cross the seam as JSON in the native API value encoding, so native,
//!   WASM and script providers are interchangeable. A host may skip the JSON round trip when
//!   both sides are native; that is an optimisation, not a contract.

use std::marker::PhantomData;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value as Json;

use crate::BoxFuture;
use crate::controller::StepName;
use crate::error::{AppError, AppResult, ErrorClass};

/// How an override changes a step.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OverrideKind {
    /// `by` runs immediately before the target.
    Before,
    /// `by` runs immediately after the target.
    After,
    /// `by` runs in the target's place and may run the target once through
    /// [`HookContext::run_wrapped`](crate::HookContext::run_wrapped).
    Wrap,
    /// `by` runs in the target's place; the target never runs.
    Replace,
    /// The target does not run; `by` is `None`.
    Disable,
}

/// One override of one step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepOverride {
    /// The step being changed.
    pub target: StepName,
    /// How.
    pub kind: OverrideKind,
    /// The step that runs instead of or around it; `None` only for `Disable`. It must be a
    /// step some controller of the same DocType declares.
    pub by: Option<StepName>,
    /// Why, for the admin override listing (WP-EXT-03).
    pub reason: String,
}

/// Calling seams. The host resolves the provider, honouring overrides.
pub trait Seams: Send + Sync {
    /// Calls seam `name` at major `version` with a JSON input.
    fn call_json<'a>(
        &'a self,
        name: &'a str,
        version: u32,
        input: Json,
    ) -> BoxFuture<'a, AppResult<Json>>;
}

/// A provider of one seam.
pub trait SeamProvider: Send + Sync {
    /// Handles one call.
    fn call(&self, input: Json) -> BoxFuture<'_, AppResult<Json>>;
}

/// A typed handle for a seam, declared once by the app that owns it:
///
/// ```
/// use spot_app_api::Seam;
/// #[derive(serde::Serialize, serde::Deserialize)] struct TaxIn { amount: String }
/// #[derive(serde::Serialize, serde::Deserialize)] struct TaxOut { tax: String }
/// pub const LINE_TAX: Seam<TaxIn, TaxOut> = Seam::new("erp_tax:line_tax", 1);
/// ```
pub struct Seam<I, O> {
    name: &'static str,
    version: u32,
    _types: PhantomData<fn(I) -> O>,
}

impl<I, O> Seam<I, O> {
    /// Declares a seam: `<app>:<seam>` at a major version.
    pub const fn new(name: &'static str, version: u32) -> Self {
        Self {
            name,
            version,
            _types: PhantomData,
        }
    }

    /// Its name.
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Its major version.
    pub fn version(&self) -> u32 {
        self.version
    }
}

impl<I: Serialize + Sync, O: DeserializeOwned> Seam<I, O> {
    /// Calls the seam through `seams`.
    pub async fn call(&self, seams: &dyn Seams, input: &I) -> AppResult<O> {
        let json = serde_json::to_value(input).map_err(|e| bad(self.name, &e))?;
        let out = seams.call_json(self.name, self.version, json).await?;
        serde_json::from_value(out).map_err(|e| bad(self.name, &e))
    }
}

fn bad(name: &str, e: &serde_json::Error) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "platform.seam_encoding",
        format!("seam {name}: {e}"),
    )
}
