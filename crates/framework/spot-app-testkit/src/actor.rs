//! An actor: one declared user, calling the engine as that user. Every operation is the
//! engine's own, permission checks included; the kit adds none and removes none.

use std::sync::atomic::Ordering;

use serde_json::Value as Json;
use spot_app_api::AppError;
use spot_app_api::context::{Actor as EngineActor, Invocation};
use spot_app_api::document::Document;
use spot_app_api::prelude::{CommandId, RecordId, Version};
use spot_document::engine::IdGenerator;

use crate::error::{Result, TestkitError};
use crate::harness::AppHarness;
use crate::json;
use crate::listing;

/// What one operation did: the document (when there is one), or the engine's refusal.
#[derive(Debug)]
pub struct DocResult {
    outcome: std::result::Result<Option<Document>, AppError>,
    result: Option<Json>,
}

impl DocResult {
    fn ok(doc: Option<Document>) -> Self {
        Self {
            outcome: Ok(doc),
            result: None,
        }
    }

    fn err(error: AppError) -> Self {
        Self {
            outcome: Err(error),
            result: None,
        }
    }

    fn bad_input(message: &str) -> Self {
        Self::err(AppError::new(
            spot_app_api::ErrorClass::Validation,
            "document.validation_failed",
            message,
        ))
    }

    /// Whether the operation succeeded.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.outcome.is_ok()
    }

    /// The document the operation returned (a create, get, update, submit, cancel or amend).
    #[must_use]
    pub fn doc(&self) -> Option<&Document> {
        self.outcome.as_ref().ok().and_then(Option::as_ref)
    }

    /// The returned document's id, as text.
    #[must_use]
    pub fn id(&self) -> Option<String> {
        self.doc().map(|d| d.id().to_string())
    }

    /// The returned document's id.
    #[must_use]
    pub fn record_id(&self) -> Option<RecordId> {
        self.doc().map(Document::id)
    }

    /// The engine's refusal.
    #[must_use]
    pub fn error(&self) -> Option<&AppError> {
        self.outcome.as_ref().err()
    }

    /// The refusal's stable code.
    #[must_use]
    pub fn error_code(&self) -> Option<&str> {
        self.error().map(|e| e.code.as_str())
    }

    /// The refusal's field errors as `(path, code)`.
    #[must_use]
    pub fn field_errors(&self) -> Vec<(String, String)> {
        self.error()
            .map(|e| {
                e.fields
                    .iter()
                    .map(|f| (f.field.to_string(), f.code.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// What a method call returned.
    #[must_use]
    pub fn method_result(&self) -> Option<&Json> {
        self.result.as_ref()
    }

    /// One line for a failed assertion: the refusal's code, class and field paths and codes.
    /// Never a field value.
    #[must_use]
    pub fn diagnostic(&self) -> String {
        match self.error() {
            None => "the operation succeeded".to_owned(),
            Some(e) => {
                let fields: Vec<String> = self
                    .field_errors()
                    .into_iter()
                    .map(|(path, code)| format!("{path}: {code}"))
                    .collect();
                // The engine's own `details` are codes (a reason, a storage code), never a value:
                // they say why a refusal with no field names one.
                let why = if fields.is_empty() && !e.details.is_empty() {
                    format!("; details: {:?}", e.details)
                } else {
                    String::new()
                };
                format!(
                    "refused with `{}` ({:?}); field errors: [{}]{why}",
                    e.code,
                    e.class,
                    fields.join(", ")
                )
            }
        }
    }
}

/// One declared user's view of the app.
pub struct Actor<'h> {
    harness: &'h AppHarness,
    name: String,
    id: RecordId,
}

impl<'h> Actor<'h> {
    pub(crate) fn new(harness: &'h AppHarness, name: &str, id: RecordId) -> Self {
        Self {
            harness,
            name: name.to_owned(),
            id,
        }
    }

    /// The user's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    fn invocation(&self) -> Invocation {
        let n = self.harness.commands.fetch_add(1, Ordering::SeqCst) + 1;
        Invocation {
            site: self.harness.site.clone(),
            actor: EngineActor::User {
                id: self.id,
                via: None,
            },
            command_id: CommandId::new(&format!("testkit-{n}")).ok(),
            language: None,
            now: self.harness.engine_now(),
        }
    }

    async fn version_of(
        &self,
        doctype: &str,
        id: RecordId,
    ) -> std::result::Result<Version, AppError> {
        self.harness
            .engine
            .peek(doctype, id)
            .await?
            .and_then(|d| d.version())
            .ok_or_else(|| {
                AppError::new(
                    spot_app_api::ErrorClass::NotFound,
                    "document.not_found",
                    "the document does not exist",
                )
            })
    }

    // The error is the test double's own answer type, built once per refused id.
    #[allow(clippy::result_large_err)]
    fn parse(&self, id: &str) -> std::result::Result<RecordId, DocResult> {
        json::parse_id(id).map_err(|m| DocResult::bad_input(&m))
    }

    /// Creates a document from JSON (decimals as strings, dates `YYYY-MM-DD`, links as id
    /// text, child tables as arrays of objects).
    pub async fn create(&self, doctype: &str, input: &Json) -> DocResult {
        let meta = self.harness.meta();
        let Some(def) = meta.doctype(doctype) else {
            return DocResult::err(AppError::new(
                spot_app_api::ErrorClass::UnknownDocType,
                "meta.unknown_doctype",
                "the DocType is not defined",
            ));
        };
        let ids = self.harness.ids.clone();
        let mut next = || ids.next_id();
        let doc = match json::document(&meta, &def, next(), input, &mut next) {
            Ok(doc) => doc,
            Err(message) => return DocResult::bad_input(&message),
        };
        match self.harness.service().create(&self.invocation(), doc).await {
            Ok(done) => {
                self.harness
                    .created
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((doctype.to_owned(), done.value.id()));
                DocResult::ok(Some(done.value))
            }
            Err(error) => DocResult::err(error),
        }
    }

    /// Reads a document through the engine's read path (hidden fields absent, masked ones masked).
    pub async fn get(&self, doctype: &str, id: &str) -> DocResult {
        let id = match self.parse(id) {
            Ok(id) => id,
            Err(result) => return result,
        };
        match self
            .harness
            .service()
            .load(&self.invocation(), doctype, id)
            .await
        {
            Ok(done) => DocResult::ok(Some(done.value)),
            Err(error) => DocResult::err(error),
        }
    }

    /// Updates a document with a JSON merge patch, at its current version.
    pub async fn update(&self, doctype: &str, id: &str, patch: &Json) -> DocResult {
        let id = match self.parse(id) {
            Ok(id) => id,
            Err(result) => return result,
        };
        let meta = self.harness.meta();
        let Some(def) = meta.doctype(doctype) else {
            return DocResult::bad_input("the DocType is not defined");
        };
        let patch = match json::patch(&meta, &def, patch) {
            Ok(patch) => patch,
            Err(message) => return DocResult::bad_input(&message),
        };
        let version = match self.version_of(doctype, id).await {
            Ok(version) => version,
            Err(error) => return DocResult::err(error),
        };
        match self
            .harness
            .service()
            .update(&self.invocation(), doctype, id, patch, version)
            .await
        {
            Ok(done) => DocResult::ok(Some(done.value)),
            Err(error) => DocResult::err(error),
        }
    }

    /// Deletes a document at its current version.
    pub async fn delete(&self, doctype: &str, id: &str) -> DocResult {
        let id = match self.parse(id) {
            Ok(id) => id,
            Err(result) => return result,
        };
        let version = match self.version_of(doctype, id).await {
            Ok(version) => version,
            Err(error) => return DocResult::err(error),
        };
        match self
            .harness
            .service()
            .delete(&self.invocation(), doctype, id, version)
            .await
        {
            Ok(_) => DocResult::ok(None),
            Err(error) => DocResult::err(error),
        }
    }

    /// Posts a draft (the lifecycle's submit).
    pub async fn submit(&self, doctype: &str, id: &str) -> DocResult {
        self.lifecycle(doctype, id, Lifecycle::Post).await
    }

    /// Cancels a posted document.
    pub async fn cancel(&self, doctype: &str, id: &str) -> DocResult {
        self.lifecycle(doctype, id, Lifecycle::Cancel).await
    }

    /// Cancels a posted document and creates its amended draft.
    pub async fn amend(&self, doctype: &str, id: &str) -> DocResult {
        self.lifecycle(doctype, id, Lifecycle::Correct).await
    }

    async fn lifecycle(&self, doctype: &str, id: &str, step: Lifecycle) -> DocResult {
        let id = match self.parse(id) {
            Ok(id) => id,
            Err(result) => return result,
        };
        let version = match self.version_of(doctype, id).await {
            Ok(version) => version,
            Err(error) => return DocResult::err(error),
        };
        let inv = self.invocation();
        let service = self.harness.service();
        let done = match step {
            Lifecycle::Post => service.post(&inv, doctype, id, version).await,
            Lifecycle::Cancel => service.cancel(&inv, doctype, id, version).await,
            Lifecycle::Correct => service.correct(&inv, doctype, id, version).await,
        };
        match done {
            Ok(done) => DocResult::ok(Some(done.value)),
            Err(error) => DocResult::err(error),
        }
    }

    /// Calls a whitelisted method with JSON arguments.
    pub async fn call(&self, doctype: &str, id: &str, method: &str, args: Json) -> DocResult {
        let id = match self.parse(id) {
            Ok(id) => id,
            Err(result) => return result,
        };
        match self
            .harness
            .service()
            .call_method(&self.invocation(), doctype, id, method, args, None)
            .await
        {
            Ok(done) => {
                let (doc, result) = done.value;
                DocResult {
                    outcome: Ok(Some(doc)),
                    result: Some(result),
                }
            }
            Err(error) => DocResult::err(error),
        }
    }

    /// Calls the whitelisted function `app`/`module`/`function` through the real function
    /// registry as this user: the verb, guest rule, rate limit and argument schema are the
    /// registry's own, and the handler is the one the test registered with
    /// [`HarnessBuilder::function`](crate::HarnessBuilder::function). A function that is not
    /// declared answers `function.not_found`; a handler's refusal passes through untouched. The
    /// function's value is [`DocResult::method_result`].
    ///
    /// A function that answers HTTP is called with its first declared verb.
    pub async fn call_function(
        &self,
        app: &str,
        module: &str,
        function: &str,
        args: Json,
    ) -> DocResult {
        let key = match spot_functions::FunctionKey::parse(app, module, function) {
            Ok(key) => key,
            Err(_) => return DocResult::bad_input("the function address is not valid"),
        };
        let registry = self.harness.functions();
        let verb = registry
            .list()
            .into_iter()
            .find(|info| info.key == key)
            .and_then(|info| info.verbs.and_then(|verbs| verbs.first().copied()));
        let mut context = spot_functions::Context::new(
            self.harness.site.as_str(),
            spot_functions::Caller::User(self.id.to_string()),
        );
        context.verb = verb;
        match registry.dispatch(&context, &key, args).await {
            Ok(value) => DocResult {
                outcome: Ok(None),
                result: Some(value),
            },
            Err(error) => DocResult::err(error),
        }
    }

    /// The documents this user can read, filtered and ordered (see the `listing` rules: the
    /// candidates are the documents created through the harness, each loaded as this user).
    ///
    /// The query is a JSON object with optional `where` (equality on scalar fields), `order_by`,
    /// `desc` and `limit`; `null` or `{}` lists everything in creation order.
    ///
    /// # Errors
    ///
    /// `app_testkit.app_invalid` for a DocType that is not defined or a query that is not valid.
    pub async fn list(&self, doctype: &str, query: &Json) -> Result<Vec<Document>> {
        let meta = self.harness.meta();
        let Some(def) = meta.doctype(doctype) else {
            return Err(TestkitError::invalid(
                "the DocType is not defined",
                vec![doctype.to_owned()],
            ));
        };
        let parsed = listing::parse(&def, query).map_err(|message| {
            TestkitError::invalid("the list query is not valid", vec![message])
        })?;
        let ids: Vec<RecordId> = self
            .harness
            .created
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .filter(|(created, _)| created == doctype)
            .map(|(_, id)| *id)
            .collect();
        let mut docs = Vec::new();
        for id in ids {
            // A document this user may not read (or that was deleted) is simply not in the list.
            if let Ok(done) = self
                .harness
                .service()
                .load(&self.invocation(), doctype, id)
                .await
            {
                docs.push(done.value);
            }
        }
        Ok(listing::apply(&parsed, docs))
    }
}

enum Lifecycle {
    Post,
    Cancel,
    Correct,
}
