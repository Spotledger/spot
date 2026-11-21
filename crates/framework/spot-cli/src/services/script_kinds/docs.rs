//! Script records through the live engine, as the platform (WP-EXT-06.T7).

use super::*;
use spot_server_scripts::kinds::{API_SCRIPT_DOCTYPE, SCHEDULED_SCRIPT_DOCTYPE};

/// A script record that was not loaded, and why (a code and a field, never a value).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skipped {
    /// The DocType of the record.
    pub doctype: &'static str,
    /// The script's name (or its id when it has none).
    pub name: String,
    /// Why it was skipped.
    pub reason: String,
}

/// What one load found.
#[derive(Clone, Debug)]
pub struct Loaded<T> {
    /// The valid, enabled scripts, in name order.
    pub scripts: Vec<T>,
    /// The records that were not loaded.
    pub skipped: Vec<Skipped>,
}

/// Reads `api_script` and `scheduled_script` documents and writes their run-time summary.
pub struct KindDocs<B: StorageBackend> {
    site: SiteId,
    engine: Arc<dyn Fn() -> Option<Arc<Engine<B>>> + Send + Sync>,
    clock: Arc<dyn Clock>,
    catalog: Arc<Catalog>,
}

pub(super) fn store_failed(reason: &'static str) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        spot_server_scripts::STORE_FAILED,
        format!("The script store failed: {reason}."),
    )
}

impl<B: StorageBackend> KindDocs<B> {
    /// Documents of `site`, through the engine `engine` returns (none while it is being built).
    #[must_use]
    pub fn new(
        site: SiteId,
        engine: Arc<dyn Fn() -> Option<Arc<Engine<B>>> + Send + Sync>,
        clock: Arc<dyn Clock>,
        catalog: Arc<Catalog>,
    ) -> KindDocs<B> {
        KindDocs {
            site,
            engine,
            clock,
            catalog,
        }
    }

    fn engine(&self) -> std::result::Result<Arc<Engine<B>>, &'static str> {
        (self.engine)().ok_or("engine unavailable")
    }

    fn inv(&self) -> std::result::Result<Invocation, &'static str> {
        Ok(Invocation {
            site: self.site.clone(),
            actor: Actor::System {
                component: Name::new(COMPONENT).map_err(|_| "name")?,
            },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        })
    }

    /// The ids of the `doctype` rows, optionally only those whose `field` equals `value`.
    pub(super) async fn ids(
        &self,
        doctype: &str,
        only: Option<(&str, &str)>,
    ) -> std::result::Result<Vec<RecordId>, &'static str> {
        let bad = |_| "name";
        let entity = Name::new(doctype).map_err(bad)?;
        let alias = Name::new("s").map_err(bad)?;
        let id = Name::new("id").map_err(bad)?;
        let mut query = Query::from(&entity, &alias, QueryPath::Report);
        query.select.push(Selection {
            expr: Expr::field(&alias, &id),
            alias: id,
        });
        if let Some((field, value)) = only {
            query.filter = Some(Expr::Text(
                Box::new(Expr::field(&alias, &Name::new(field).map_err(bad)?)),
                TextMatch {
                    mode: TextMode::Equals,
                    pattern: value.to_owned(),
                    case_insensitive: false,
                },
            ));
        }
        query.limit = Some(MAX_ROWS + 1);
        let options = TxOptions::new(
            Name::new("script_kinds_read").map_err(bad)?,
            Isolation::ReadCommitted,
        )
        .read_only();
        let engine = self.engine()?;
        let mut tx = engine
            .backend()
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|_| "storage unavailable")?;
        let rows = tx.query(&query).await;
        let _ = tx.rollback().await;
        let rows = rows.map_err(|_| "storage unavailable")?;
        if rows.rows.len() > MAX_ROWS as usize {
            return Err("too many scripts");
        }
        Ok(rows
            .rows
            .iter()
            .filter_map(|row| match row.first() {
                Some(Value::Id(id)) => Some(*id),
                _ => None,
            })
            .collect())
    }

    pub(super) async fn load(
        &self,
        doctype: &str,
        id: RecordId,
    ) -> std::result::Result<Option<Document>, &'static str> {
        let engine = self.engine()?;
        let inv = self.inv()?;
        match engine.load(&inv, doctype, id).await {
            Ok(loaded) => Ok(Some(loaded.value)),
            Err(e) if e.class == ErrorClass::NotFound => Ok(None),
            Err(_) => Err("engine unavailable"),
        }
    }

    async fn read_all<T>(
        &self,
        doctype: &'static str,
        parse: impl Fn(&Document) -> AppResult<T>,
        enabled: impl Fn(&T) -> bool,
        name_of: impl Fn(&T) -> String,
    ) -> AppResult<Loaded<T>> {
        let ids = self.ids(doctype, None).await.map_err(store_failed)?;
        let mut scripts = Vec::new();
        let mut skipped = Vec::new();
        for id in ids {
            let Some(doc) = self.load(doctype, id).await.map_err(store_failed)? else {
                continue;
            };
            match parse(&doc) {
                Ok(script) if enabled(&script) => scripts.push(script),
                Ok(_) => {}
                Err(error) => {
                    let name = match doc.get("name") {
                        Value::Text(t) if !t.is_empty() => t.clone(),
                        _ => id.to_string(),
                    };
                    let fields: Vec<String> =
                        error.fields.iter().map(|f| f.field.to_string()).collect();
                    skipped.push(Skipped {
                        doctype,
                        name,
                        reason: format!("{}: {}", error.code, fields.join(", ")),
                    });
                }
            }
        }
        scripts.sort_by_key(|script| name_of(script));
        Ok(Loaded { scripts, skipped })
    }

    /// The enabled, valid API scripts and the records that were skipped.
    ///
    /// # Errors
    ///
    /// `serverscript.store_failed` when storage or the engine is unavailable.
    pub async fn api_scripts(&self, limits: &ScriptLimits) -> AppResult<Loaded<ApiScript>> {
        self.read_all(
            API_SCRIPT_DOCTYPE,
            |doc| ApiScript::from_document_with(doc, limits),
            |s| s.enabled,
            |s| s.name.clone(),
        )
        .await
    }

    /// The enabled, valid scheduled scripts and the records that were skipped.
    ///
    /// # Errors
    ///
    /// `serverscript.store_failed` when storage or the engine is unavailable.
    pub async fn scheduled_scripts(
        &self,
        limits: &ScriptLimits,
    ) -> AppResult<Loaded<ScheduledScript>> {
        self.read_all(
            SCHEDULED_SCRIPT_DOCTYPE,
            |doc| ScheduledScript::from_document_with(doc, limits),
            |s| s.enabled,
            |s| s.name.clone(),
        )
        .await
    }

    /// The user named `name` and whether they are active.
    ///
    /// # Errors
    ///
    /// `serverscript.store_failed` when storage or the engine is unavailable.
    pub async fn user(&self, name: &str) -> AppResult<Option<UserRecord>> {
        let Some(id) = self
            .ids("user", Some(("user_name", name)))
            .await
            .map_err(store_failed)?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };
        let Some(doc) = self.load("user", id).await.map_err(store_failed)? else {
            return Ok(None);
        };
        let active = matches!(doc.get("state"), Value::Text(state) if state == "active");
        Ok(Some(UserRecord {
            id,
            enabled: active,
        }))
    }

    /// Sets `fields` on the script named `name` (after `bump` is applied to the current
    /// document), when it exists. A lost version race loses one run-time summary and nothing
    /// else. The write goes through the engine, so the audit participants record it under the
    /// `script_kinds` component.
    pub(super) async fn patch_named(
        &self,
        doctype: &str,
        name: &str,
        fields: Vec<(&'static str, Value)>,
        count_run: bool,
    ) -> std::result::Result<(), &'static str> {
        let Some(id) = self
            .ids(doctype, Some(("name", name)))
            .await?
            .into_iter()
            .next()
        else {
            return Ok(());
        };
        let Some(doc) = self.load(doctype, id).await? else {
            return Ok(());
        };
        let mut patch = DocumentPatch::default();
        for (field, value) in fields {
            patch
                .fields
                .insert(Name::new(field).map_err(|_| "name")?, value);
        }
        if count_run {
            let runs = match doc.get("last_run_count") {
                Value::Int(n) => *n,
                _ => 0,
            };
            patch.fields.insert(
                Name::new("last_run_count").map_err(|_| "name")?,
                Value::Int(runs.saturating_add(1)),
            );
        }
        let engine = self.engine()?;
        let inv = self.inv()?;
        engine
            .update(
                &inv,
                doctype,
                id,
                patch,
                doc.version().unwrap_or(Version::FIRST),
            )
            .await
            .map(|_| ())
            .map_err(|_| "update failed")
    }
}

/// What the runtime needs from the documents, object-safe so the runtime is not generic over
/// the storage backend (the production one is PostgreSQL, the tests' is in memory).
pub trait KindSource: Send + Sync {
    /// The enabled, valid API scripts and the skipped records.
    fn api_scripts<'a>(
        &'a self,
        limits: &'a ScriptLimits,
    ) -> BoxFuture<'a, AppResult<Loaded<ApiScript>>>;

    /// The enabled, valid scheduled scripts and the skipped records.
    fn scheduled_scripts<'a>(
        &'a self,
        limits: &'a ScriptLimits,
    ) -> BoxFuture<'a, AppResult<Loaded<ScheduledScript>>>;

    /// The user named `name`.
    fn user<'a>(&'a self, name: &'a str) -> BoxFuture<'a, AppResult<Option<UserRecord>>>;

    /// Writes the run-time summary of a script (and counts the run when asked).
    fn summarise<'a>(
        &'a self,
        doctype: &'static str,
        name: &'a str,
        fields: Vec<(&'static str, Value)>,
        count_run: bool,
    ) -> BoxFuture<'a, AppResult<()>>;
}

impl<B: StorageBackend> KindSource for KindDocs<B> {
    fn api_scripts<'a>(
        &'a self,
        limits: &'a ScriptLimits,
    ) -> BoxFuture<'a, AppResult<Loaded<ApiScript>>> {
        Box::pin(async move { KindDocs::api_scripts(self, limits).await })
    }

    fn scheduled_scripts<'a>(
        &'a self,
        limits: &'a ScriptLimits,
    ) -> BoxFuture<'a, AppResult<Loaded<ScheduledScript>>> {
        Box::pin(async move { KindDocs::scheduled_scripts(self, limits).await })
    }

    fn user<'a>(&'a self, name: &'a str) -> BoxFuture<'a, AppResult<Option<UserRecord>>> {
        Box::pin(async move { KindDocs::user(self, name).await })
    }

    fn summarise<'a>(
        &'a self,
        doctype: &'static str,
        name: &'a str,
        fields: Vec<(&'static str, Value)>,
        count_run: bool,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            self.patch_named(doctype, name, fields, count_run)
                .await
                .map_err(store_failed)
        })
    }
}

/// The access factory of the running site: every run gets a [`ServiceAccess`] over the live
/// engine, acting as the run's user. `host.query` runs through [`EngineQueries`]
/// (`Engine::query_as`), with the run's own rights.
pub struct EngineAccessFactory<B: StorageBackend> {
    engine: Arc<dyn Fn() -> Option<Arc<Engine<B>>> + Send + Sync>,
}

impl<B: StorageBackend> EngineAccessFactory<B> {
    /// A factory over the engine `engine` returns.
    #[must_use]
    pub fn new(engine: Arc<dyn Fn() -> Option<Arc<Engine<B>>> + Send + Sync>) -> Self {
        EngineAccessFactory { engine }
    }
}

impl<B: StorageBackend> ScriptAccessFactory for EngineAccessFactory<B> {
    fn open(&self, invocation: Invocation) -> Box<dyn ScriptDocumentAccess> {
        match (self.engine)() {
            Some(engine) => Box::new(ServiceAccess::new(
                engine as Arc<dyn DocumentService>,
                Arc::new(EngineQueries::new(Arc::clone(&self.engine))),
                invocation,
            )),
            None => Box::new(UnavailableAccess { invocation }),
        }
    }
}

/// `host.query` for API and scheduled scripts: the engine's `query_as` (WP-EXT-06.T11), which
/// applies the invocation's own read rights, owner restriction and field masks. A script that
/// runs elevated queries as the platform invocation and so reads what the platform may read;
/// every other script reads what its user may.
pub struct EngineQueries<B: StorageBackend> {
    engine: Arc<dyn Fn() -> Option<Arc<Engine<B>>> + Send + Sync>,
}

impl<B: StorageBackend> EngineQueries<B> {
    /// Queries through the engine `engine` returns (none: the query is refused as unavailable).
    #[must_use]
    pub fn new(engine: Arc<dyn Fn() -> Option<Arc<Engine<B>>> + Send + Sync>) -> Self {
        EngineQueries { engine }
    }
}

impl<B: StorageBackend> ScriptQueries for EngineQueries<B> {
    fn query<'a>(
        &'a self,
        inv: &'a Invocation,
        query: &'a Query,
    ) -> BoxFuture<'a, AppResult<Rows>> {
        Box::pin(async move {
            match (self.engine)() {
                Some(engine) => engine.query_as(inv, query).await,
                None => Err(unavailable()),
            }
        })
    }
}

/// A `host.query` that is always refused with a plain message: for a host that has no engine to
/// ask (and for tests). The running site uses [`EngineQueries`].
pub struct NoQueries;

impl ScriptQueries for NoQueries {
    fn query<'a>(
        &'a self,
        _inv: &'a Invocation,
        _query: &'a Query,
    ) -> BoxFuture<'a, AppResult<Rows>> {
        Box::pin(async {
            Err(AppError::new(
                ErrorClass::Internal,
                spot_server_scripts::STORE_FAILED,
                "host.query is not available to API and scheduled scripts on this host.",
            ))
        })
    }
}

/// The access while the engine is being built: every operation fails closed.
struct UnavailableAccess {
    invocation: Invocation,
}

fn unavailable() -> AppError {
    AppError::new(
        ErrorClass::Unavailable,
        spot_server_scripts::STORE_FAILED,
        "The document engine is not available; try again.",
    )
}

impl ScriptDocumentAccess for UnavailableAccess {
    fn invocation(&self) -> &Invocation {
        &self.invocation
    }
    fn meta<'a>(
        &'a mut self,
        _doctype: &'a str,
    ) -> BoxFuture<'a, AppResult<Arc<spot_app_api::DocTypeDef>>> {
        Box::pin(async { Err(unavailable()) })
    }
    fn get<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async { Err(unavailable()) })
    }
    fn query<'a>(&'a mut self, _query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        Box::pin(async { Err(unavailable()) })
    }
    fn insert(&mut self, _doc: Document) -> BoxFuture<'_, AppResult<Document>> {
        Box::pin(async { Err(unavailable()) })
    }
    fn update<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
        _patch: DocumentPatch,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async { Err(unavailable()) })
    }
    fn call<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
        _method: &'a str,
        _args: Json,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async { Err(unavailable()) })
    }
}
