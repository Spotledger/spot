//! The engine as the platform itself, and as the triggering user (WP-EXT-06.T4).

use super::*;


/// What the engine-backed adapters share.
#[derive(Clone)]
pub struct EngineAccess {
    site: SiteId,
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    catalog: Arc<Catalog>,
}

impl EngineAccess {
    /// An access to the live engine of `site`.
    #[must_use]
    pub fn new(
        site: SiteId,
        live: Arc<SiteLive>,
        clock: Arc<dyn Clock>,
        catalog: Arc<Catalog>,
    ) -> EngineAccess {
        EngineAccess {
            site,
            live,
            clock,
            catalog,
        }
    }

    fn engine(&self) -> std::result::Result<Arc<Engine<PgBackend>>, &'static str> {
        self.live.engine().ok_or("engine unavailable")
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

    /// The ids of the `server_script` rows whose `field` equals `value`.
    async fn ids_where(
        &self,
        field: &str,
        value: &str,
    ) -> std::result::Result<Vec<RecordId>, &'static str> {
        let bad = |_| "name";
        let entity = Name::new(SERVER_SCRIPT_DOCTYPE).map_err(bad)?;
        let alias = Name::new("s").map_err(bad)?;
        let id = Name::new("id").map_err(bad)?;
        let mut query = Query::from(&entity, &alias, QueryPath::Report);
        query.select.push(Selection {
            expr: Expr::field(&alias, &id),
            alias: id,
        });
        query.filter = Some(Expr::Text(
            Box::new(Expr::field(&alias, &Name::new(field).map_err(bad)?)),
            TextMatch {
                mode: TextMode::Equals,
                pattern: value.to_owned(),
                case_insensitive: false,
            },
        ));
        query.limit = Some(MAX_SCRIPTS_PER_DOCTYPE + 1);
        let options = TxOptions::new(
            Name::new("scripts_read").map_err(bad)?,
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
        if rows.rows.len() > MAX_SCRIPTS_PER_DOCTYPE as usize {
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

    async fn load(&self, id: RecordId) -> std::result::Result<Option<Document>, &'static str> {
        let engine = self.engine()?;
        let inv = self.inv()?;
        match engine.load(&inv, SERVER_SCRIPT_DOCTYPE, id).await {
            Ok(loaded) => Ok(Some(loaded.value)),
            Err(e) if e.class == ErrorClass::NotFound => Ok(None),
            Err(_) => Err("engine unavailable"),
        }
    }

    /// Sets `fields` on the script named `name`, when it exists. A lost version race loses one
    /// run-time summary and nothing else.
    async fn patch_named(
        &self,
        name: &str,
        fields: Vec<(&'static str, Value)>,
    ) -> std::result::Result<(), &'static str> {
        let Some(id) = self.ids_where("name", name).await?.into_iter().next() else {
            return Ok(());
        };
        let Some(doc) = self.load(id).await? else {
            return Ok(());
        };
        let mut patch = DocumentPatch::default();
        for (field, value) in fields {
            patch
                .fields
                .insert(Name::new(field).map_err(|_| "name")?, value);
        }
        let engine = self.engine()?;
        let inv = self.inv()?;
        engine
            .update(
                &inv,
                SERVER_SCRIPT_DOCTYPE,
                id,
                patch,
                doc.version().unwrap_or(Version::FIRST),
            )
            .await
            .map(|_| ())
            .map_err(|_| "update failed")
    }
}

pub(super) fn store_failed(reason: &'static str) -> AppError {
    AppError::new(
        ErrorClass::Internal,
        spot_server_scripts::STORE_FAILED,
        format!("The Server Script store failed: {reason}."),
    )
}

/// `server_script` documents as the library's [`ScriptDocuments`].
pub struct EngineScriptDocuments {
    access: EngineAccess,
    generation: Arc<AtomicU64>,
}

impl EngineScriptDocuments {
    /// The adapter. `generation` is [`Runtime::generation`]: a disable bumps it.
    #[must_use]
    pub fn new(access: EngineAccess, generation: Arc<AtomicU64>) -> EngineScriptDocuments {
        EngineScriptDocuments { access, generation }
    }
}

impl ScriptDocuments for EngineScriptDocuments {
    fn enabled_for<'a>(&'a self, doctype: &'a str) -> BoxFuture<'a, AppResult<Vec<ServerScript>>> {
        Box::pin(async move {
            let ids = self
                .access
                .ids_where("doctype", doctype)
                .await
                .map_err(store_failed)?;
            let mut scripts = Vec::new();
            for id in ids {
                let Some(doc) = self.access.load(id).await.map_err(store_failed)? else {
                    continue;
                };
                // A record that no longer reads as a script was saved before a rule changed:
                // it does not run (it cannot be saved again either), and nothing else is hurt.
                if let Ok(script) = ServerScript::from_document(&doc)
                    && script.enabled
                {
                    scripts.push(script);
                }
            }
            scripts.sort_by(|a, b| a.name.cmp(&b.name));
            Ok(scripts)
        })
    }

    /// The compile step stores the hash on the very document being saved, so there is nothing to
    /// write here: writing it back would re-enter the save.
    fn record_compile<'a>(
        &'a self,
        _name: &'a str,
        _outcome: &'a CompileOutcome,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Ok(()) })
    }

    /// Records a failure on the script for the administrator's listing: the code, the event and
    /// the duration, never a message of the script. A success writes nothing (one write per
    /// document event would be a write per request).
    fn record_run<'a>(&'a self, run: &'a ScriptRun) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            if run.ok {
                return Ok(());
            }
            let line = format!(
                "{} (event {}, {} ms)",
                run.code,
                run.event.as_str(),
                run.duration_ms
            );
            self.access
                .patch_named(&run.script, vec![("last_error", Value::Text(line))])
                .await
                .map_err(store_failed)
        })
    }

    fn disable<'a>(&'a self, name: &'a str, reason: &'a str) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            let reason: String = reason.chars().take(500).collect();
            let done = self
                .access
                .patch_named(
                    name,
                    vec![
                        ("enabled", Value::Bool(false)),
                        ("disabled_reason", Value::Text(reason)),
                    ],
                )
                .await
                .map_err(store_failed);
            self.generation.fetch_add(1, Ordering::AcqRel);
            done
        })
    }
}

/// The document view of the triggering user: the site's authorizer decides which fields they may
/// read and which are masked. With no authorizer attached the view **refuses** (fail closed).
pub struct AuthorizerView {
    live: Arc<SiteLive>,
}

impl AuthorizerView {
    /// A view over the site's live authorizer.
    #[must_use]
    pub fn new(live: Arc<SiteLive>) -> AuthorizerView {
        AuthorizerView { live }
    }
}

impl FieldView for AuthorizerView {
    fn readable<'a>(
        &'a self,
        inv: &'a Invocation,
        def: &'a DocTypeDef,
        doc: &'a Document,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            let Some(authorizer) = self.live.authorizer() else {
                return Err(not_ready());
            };
            let principal = authorizer.principal_for(inv).await;
            authorizer.readable(&principal, doc.clone(), def).await
        })
    }
}

/// `host.call`: a whitelisted method, called through the engine as the triggering user.
pub struct EngineMethodCaller {
    live: Arc<SiteLive>,
}

impl EngineMethodCaller {
    /// A caller over the site's live engine.
    #[must_use]
    pub fn new(live: Arc<SiteLive>) -> EngineMethodCaller {
        EngineMethodCaller { live }
    }
}

impl MethodCaller for EngineMethodCaller {
    fn call<'a>(
        &'a self,
        inv: &'a Invocation,
        doctype: &'a str,
        id: RecordId,
        method: &'a str,
        args: Json,
    ) -> BoxFuture<'a, AppResult<Json>> {
        Box::pin(async move {
            let Some(engine) = self.live.engine() else {
                return Err(not_ready());
            };
            engine
                .call_method(inv, doctype, id, method, args, None)
                .await
                .map(|outcome| outcome.value.1)
        })
    }
}

