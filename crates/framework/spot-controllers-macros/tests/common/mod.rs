//! A sample app controller written the way an app crate would: it names only `spot_app_api`,
//! `serde` (for the derives) and `serde_json`.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use spot_app_api::prelude::{Name, Value};
use spot_app_api::{
    AppError, AppResult, BoxFuture, Controller, DocumentPatch, ErrorClass, HookContext,
    MethodContext, MethodDecl, StepDecl, StepName,
};
use spot_controllers_macros::{MethodSchema, controller};

pub fn n(name: &str) -> Name {
    Name::new(name).expect("a valid storage name")
}

#[derive(Debug, Deserialize, MethodSchema)]
pub struct RenameArgs {
    pub title: String,
    pub suffix: Option<String>,
}

#[derive(Debug, Serialize, MethodSchema)]
pub struct Renamed {
    pub old: String,
    pub new: String,
}

#[derive(Clone, Copy, Debug, Deserialize, MethodSchema)]
pub enum Mode {
    Sum,
    #[serde(rename = "largest")]
    Max,
}

#[derive(Debug, Deserialize, MethodSchema)]
pub struct TallyArgs {
    pub numbers: Vec<i64>,
    pub mode: Mode,
    #[serde(default)]
    pub labels: Vec<String>,
}

#[derive(Debug, Serialize, MethodSchema)]
pub struct Tally {
    pub value: i64,
    pub count: u32,
}

/// The sample controller for the `note` DocType.
pub struct NoteController;

#[controller(doctype = "note")]
impl NoteController {
    /// Changes the title through the update lifecycle and reports old and new.
    #[whitelist(right = "write")]
    async fn rename(&self, ctx: &mut dyn MethodContext, args: RenameArgs) -> AppResult<Renamed> {
        let old = match ctx.doc().get("title") {
            Value::Text(t) => t.clone(),
            _ => String::new(),
        };
        let new = format!("{}{}", args.title, args.suffix.unwrap_or_default());
        let (id, version) = (
            ctx.doc().id(),
            ctx.doc()
                .version()
                .ok_or_else(|| AppError::new(ErrorClass::Internal, "test.no_version", "new"))?,
        );
        let mut patch = DocumentPatch::default();
        patch.fields.insert(n("title"), Value::Text(new.clone()));
        ctx.update("note", id, patch, version).await?;
        Ok(Renamed { old, new })
    }

    /// Reads the title; needs only `read`.
    #[whitelist(right = "read")]
    async fn peek_title(&self, ctx: &mut dyn MethodContext) -> AppResult<String> {
        match ctx.doc().get("title") {
            Value::Text(t) => Ok(t.clone()),
            _ => Ok(String::new()),
        }
    }

    /// Pure arithmetic over typed arguments.
    #[whitelist(right = "read")]
    async fn tally(&self, _ctx: &mut dyn MethodContext, args: TallyArgs) -> AppResult<Tally> {
        let count = u32::try_from(args.numbers.len()).unwrap_or(u32::MAX);
        let value = match args.mode {
            Mode::Sum => args.numbers.iter().sum(),
            Mode::Max => args.numbers.iter().copied().max().unwrap_or(0),
        };
        Ok(Tally { value, count })
    }

    /// Dirties the document, then panics: the transaction must roll back.
    #[whitelist]
    async fn explode(&self, ctx: &mut dyn MethodContext) -> AppResult<()> {
        let (id, version) = (ctx.doc().id(), ctx.doc().version());
        let mut patch = DocumentPatch::default();
        patch
            .fields
            .insert(n("title"), Value::Text("dirty".to_owned()));
        if let Some(version) = version {
            ctx.update("note", id, patch, version).await?;
        }
        panic!("boom");
    }

    /// An app-registered permission, and extra argument fields tolerated.
    #[whitelist(permission = "approve_note", allow_unknown)]
    async fn approve(&self, _ctx: &mut dyn MethodContext, args: Json) -> AppResult<Json> {
        Ok(args)
    }

    /// Not whitelisted: must never be callable through the API.
    pub async fn secret(&self, _ctx: &mut dyn MethodContext) -> AppResult<String> {
        Ok("secret".to_owned())
    }
}

impl Controller for NoteController {
    fn doctype(&self) -> &str {
        Self::WHITELIST_DOCTYPE
    }

    fn steps(&self) -> Vec<StepDecl> {
        Vec::new()
    }

    fn methods(&self) -> Vec<MethodDecl> {
        Self::whitelisted_methods()
    }

    fn run_step<'a>(
        &'a self,
        _step: &'a StepName,
        _ctx: &'a mut dyn HookContext,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    fn call_method<'a>(
        &'a self,
        method: &'a str,
        args: Json,
        ctx: &'a mut dyn MethodContext,
    ) -> BoxFuture<'a, AppResult<Json>> {
        self.whitelisted_call(method, args, ctx)
    }
}
