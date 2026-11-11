//! Loading a DocType's enabled scripts and recording what happened to them.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use spot_app_api::{AppResult, BoxFuture};

use crate::compile::CompileOutcome;
use crate::log::ScriptRun;
use crate::record::ServerScript;

/// Where a DocType's scripts come from, and where their compile and run results go.
///
/// The document engine's host supplies an implementation; [`FakeScriptStore`] serves tests and
/// [`DocumentScriptStore`] is the document-backed one.
pub trait ScriptStore: Send + Sync {
    /// The enabled scripts for `doctype`, in name order. A disabled script is not returned.
    fn enabled_for<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Vec<ServerScript>>>;

    /// Records the result of compiling `name`: its hash, or its diagnostics.
    fn record_compile<'a>(
        &'a self,
        doctype: &'a str,
        name: &'a str,
        outcome: CompileOutcome,
    ) -> BoxFuture<'a, AppResult<()>>;

    /// Records one finished run (the run count, and the last failure when there was one).
    fn record_run<'a>(&'a self, run: ScriptRun) -> BoxFuture<'a, AppResult<()>>;

    /// Disables `name` with a visible reason, after a run of failures.
    fn disable<'a>(
        &'a self,
        doctype: &'a str,
        name: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, AppResult<()>>;
}

/// An in-memory store: the reference the document-backed one must behave like.
#[derive(Default)]
pub struct FakeScriptStore {
    scripts: Mutex<BTreeMap<String, ServerScript>>,
    compiles: Mutex<Vec<(String, CompileOutcome)>>,
    runs: Mutex<Vec<ScriptRun>>,
    disabled: Mutex<Vec<(String, String)>>,
}

impl FakeScriptStore {
    /// An empty store.
    pub fn new() -> FakeScriptStore {
        FakeScriptStore::default()
    }

    /// A store holding `scripts`.
    pub fn with_scripts(scripts: impl IntoIterator<Item = ServerScript>) -> FakeScriptStore {
        let store = FakeScriptStore::new();
        for script in scripts {
            store.put(script);
        }
        store
    }

    /// Adds or replaces a script.
    pub fn put(&self, script: ServerScript) {
        self.scripts().insert(script.name.clone(), script);
    }

    /// The script named `name`, when it is stored.
    pub fn get(&self, name: &str) -> Option<ServerScript> {
        self.scripts().get(name).cloned()
    }

    /// Every compile result recorded, in order.
    pub fn compiles(&self) -> Vec<(String, CompileOutcome)> {
        self.compiles_lock().clone()
    }

    /// Every run recorded, in order.
    pub fn runs(&self) -> Vec<ScriptRun> {
        self.runs_lock().clone()
    }

    /// Every disable recorded, as `(name, reason)`, in order.
    pub fn disabled(&self) -> Vec<(String, String)> {
        self.disabled_lock().clone()
    }

    fn scripts(&self) -> MutexGuard<'_, BTreeMap<String, ServerScript>> {
        self.scripts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn compiles_lock(&self) -> MutexGuard<'_, Vec<(String, CompileOutcome)>> {
        self.compiles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn runs_lock(&self) -> MutexGuard<'_, Vec<ScriptRun>> {
        self.runs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn disabled_lock(&self) -> MutexGuard<'_, Vec<(String, String)>> {
        self.disabled
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl ScriptStore for FakeScriptStore {
    fn enabled_for<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Vec<ServerScript>>> {
        let scripts: Vec<ServerScript> = self
            .scripts()
            .values()
            .filter(|s| s.enabled && s.doctype == doctype)
            .cloned()
            .collect();
        Box::pin(async move { Ok(scripts) })
    }

    fn record_compile<'a>(
        &'a self,
        _doctype: &'a str,
        name: &'a str,
        outcome: CompileOutcome,
    ) -> BoxFuture<'a, AppResult<()>> {
        if let Some(script) = self.scripts().get_mut(name) {
            script.compiled_hash = outcome.hash.clone();
        }
        self.compiles_lock().push((name.to_owned(), outcome));
        Box::pin(async move { Ok(()) })
    }

    fn record_run<'a>(&'a self, run: ScriptRun) -> BoxFuture<'a, AppResult<()>> {
        self.runs_lock().push(run);
        Box::pin(async move { Ok(()) })
    }

    fn disable<'a>(
        &'a self,
        _doctype: &'a str,
        name: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, AppResult<()>> {
        if let Some(script) = self.scripts().get_mut(name) {
            script.enabled = false;
            script.disabled_reason = Some(reason.to_owned());
        }
        self.disabled_lock()
            .push((name.to_owned(), reason.to_owned()));
        Box::pin(async move { Ok(()) })
    }
}

/// The document operations a [`DocumentScriptStore`] needs.
///
/// A host implements this over the document engine (`spot-document`) acting as the **system
/// principal**: the engine owns the mapping between the app API's `Document` and storage, and
/// enforces the `Server Script` DocType's own rules (its controller, its fields, its lifecycle).
/// Keeping the port here is what lets this crate stay free of a runtime dependency on the
/// document engine while still being document-backed.
pub trait ScriptDocuments: Send + Sync {
    /// The enabled `Server Script` documents for `doctype`, in name order.
    fn enabled_for<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Vec<ServerScript>>>;
    /// Records a compile result on the named script.
    fn record_compile<'a>(
        &'a self,
        name: &'a str,
        outcome: &'a CompileOutcome,
    ) -> BoxFuture<'a, AppResult<()>>;
    /// Records a run on the named script (its run count, and the last failure).
    fn record_run<'a>(&'a self, run: &'a ScriptRun) -> BoxFuture<'a, AppResult<()>>;
    /// Disables the named script with a reason.
    fn disable<'a>(&'a self, name: &'a str, reason: &'a str) -> BoxFuture<'a, AppResult<()>>;
}

/// The document-backed store: every call is delegated to a [`ScriptDocuments`] adapter, which a
/// host implements over the document engine as the system principal.
pub struct DocumentScriptStore {
    documents: Arc<dyn ScriptDocuments>,
}

impl DocumentScriptStore {
    /// A store over `documents`.
    pub fn new(documents: Arc<dyn ScriptDocuments>) -> DocumentScriptStore {
        DocumentScriptStore { documents }
    }
}

impl ScriptStore for DocumentScriptStore {
    fn enabled_for<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Vec<ServerScript>>> {
        self.documents.enabled_for(doctype)
    }

    fn record_compile<'a>(
        &'a self,
        _doctype: &'a str,
        name: &'a str,
        outcome: CompileOutcome,
    ) -> BoxFuture<'a, AppResult<()>> {
        let documents = Arc::clone(&self.documents);
        Box::pin(async move { documents.record_compile(name, &outcome).await })
    }

    fn record_run<'a>(&'a self, run: ScriptRun) -> BoxFuture<'a, AppResult<()>> {
        let documents = Arc::clone(&self.documents);
        Box::pin(async move { documents.record_run(&run).await })
    }

    fn disable<'a>(
        &'a self,
        _doctype: &'a str,
        name: &'a str,
        reason: &'a str,
    ) -> BoxFuture<'a, AppResult<()>> {
        self.documents.disable(name, reason)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::record::ScriptEvent;

    fn script(name: &str, enabled: bool) -> ServerScript {
        ServerScript {
            record_id: None,
            name: name.into(),
            doctype: "note".into(),
            event: ScriptEvent::Validate,
            source: "export function on_validate() {}".into(),
            compiled_hash: None,
            timeout_ms: 200,
            priority: 0,
            enabled,
            disabled_reason: None,
        }
    }

    #[tokio::test]
    async fn only_enabled_scripts_of_the_doctype_are_returned() {
        let store = FakeScriptStore::with_scripts([
            script("a", true),
            script("b", false),
            ServerScript {
                doctype: "other".into(),
                ..script("c", true)
            },
        ]);
        let found = store.enabled_for("note").await.expect("the store answers");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "a");
    }

    #[tokio::test]
    async fn disabling_a_script_hides_it() {
        let store = FakeScriptStore::with_scripts([script("a", true)]);
        store
            .disable("note", "a", "too many failures")
            .await
            .expect("recorded");
        assert!(store.enabled_for("note").await.expect("answers").is_empty());
        assert_eq!(
            store.disabled(),
            vec![("a".into(), "too many failures".into())]
        );
    }
}
