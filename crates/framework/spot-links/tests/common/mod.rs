//! The links test harness: the real engine over the in-memory backend, the effective metadata
//! from the TOML fixtures, and a scoped [`Repository`] the checks run through.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use spot_app_api::context::{DocumentService, Invocation, Repository};
use spot_app_api::error::StorageOp;
use spot_app_api::prelude::{Name, Query, QueryPath, RecordId, Rows, Value, Version};
use spot_app_api::{
    Actor, AppError, AppResult, BoxFuture, DocTypeDef, Document, DocumentPatch, ErrorClass,
};
use spot_core::{SiteId, SystemClock};
use spot_document::engine::{Engine, V7Ids};
use spot_document::mapping;
use spot_links::{FieldIssue, RepoChecks, WhereUsed};
use spot_meta::{Customisations, EffectiveMeta, Registry, load_dir};
use spot_seams::RegistryBuilder;
use spot_storage_contract::{
    Catalog, Collation, DocumentStore, Isolation, QueryExecutor, RowLock, StorageBackend,
    Transaction, TxOptions,
};
use spot_storage_fake::FakeBackend;

/// The engine, its metadata and its catalog, all over one in-memory store.
pub struct Harness {
    /// The effective metadata from the fixtures.
    pub meta: Arc<EffectiveMeta>,
    /// The compiled catalog (the engine's own is private).
    pub catalog: Arc<Catalog>,
    /// The store the engine and the checks share.
    pub backend: FakeBackend,
    /// The engine documents are driven through.
    pub engine: Engine<FakeBackend>,
}

/// Loads the fixture DocTypes and builds the effective metadata.
pub fn meta() -> Arc<EffectiveMeta> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/doctypes");
    let definitions = match load_dir(&dir) {
        Ok(d) => d,
        Err(errors) => panic!("the fixture DocTypes must load: {errors:?}"),
    };
    let effective = match Registry::build(definitions, &Customisations::new()) {
        Ok(m) => m,
        Err(errors) => panic!("the fixture metadata must compile: {errors:?}"),
    };
    Arc::new(effective)
}

impl Harness {
    /// Builds the harness and prepares the store for the catalog.
    pub async fn new() -> Self {
        let meta = meta();
        let collation = Collation::new("und").unwrap_or_else(|e| panic!("und: {e}"));
        let catalog = Arc::new(
            meta.catalog(collation)
                .unwrap_or_else(|e| panic!("the metadata must compile to a catalog: {e}")),
        );
        let backend = FakeBackend::new();
        let seams = Arc::new(
            RegistryBuilder::new()
                .build()
                .unwrap_or_else(|c| panic!("the empty registry has no conflicts: {c:?}")),
        );
        let engine = Engine::new(
            backend.clone(),
            meta.clone(),
            seams,
            Arc::new(SystemClock::new()),
            Arc::new(V7Ids),
        )
        .unwrap_or_else(|e| panic!("the engine must build: {e}"));
        engine
            .prepare()
            .await
            .unwrap_or_else(|e| panic!("the store must prepare: {e}"));
        Self {
            meta,
            catalog,
            backend,
            engine,
        }
    }

    /// The app view of `doctype`.
    pub fn def(&self, doctype: &str) -> Arc<DocTypeDef> {
        self.meta
            .doctype(doctype)
            .unwrap_or_else(|| panic!("the fixture defines {doctype}"))
    }

    /// An invocation for the tests.
    pub fn invocation(&self) -> Invocation {
        Invocation {
            site: SiteId::new("links.test").unwrap_or_else(|e| panic!("site: {e}")),
            actor: Actor::System {
                component: Name::new("links_tests").unwrap_or_else(|e| panic!("name: {e}")),
            },
            command_id: None,
            language: None,
            now: spot_app_api::prelude::OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// Creates a document through the engine.
    pub async fn create(&self, doctype: &str, fields: &[(&str, Value)]) -> Document {
        let mut doc = Document::new(name(doctype), RecordId::new_v7());
        for (field, value) in fields {
            doc.set(name(field), value.clone());
        }
        self.create_doc(doc).await
    }

    /// Creates a fully built document through the engine.
    pub async fn create_doc(&self, doc: Document) -> Document {
        let inv = self.invocation();
        match self.engine.create(&inv, doc).await {
            Ok(outcome) => outcome.value,
            Err(error) => panic!("create must succeed: {error}"),
        }
    }

    /// Writes a document straight to storage, around the engine, in `state`: how a test makes a
    /// link the engine now refuses (a cancelled or wrong-typed target, a posted source pointing at
    /// a draft) so the validator itself can be shown to catch it.
    pub async fn create_raw(
        &self,
        doctype: &str,
        fields: &[(&str, Value)],
        state: spot_app_api::DocState,
    ) -> Document {
        let mut doc = Document::new(name(doctype), RecordId::new_v7());
        for (field, value) in fields {
            doc.set(name(field), value.clone());
        }
        doc.mark_saved(Version::FIRST, Some(state));
        let data = mapping::to_storage(&self.meta, &doc)
            .unwrap_or_else(|e| panic!("the document must map to storage: {e}"));
        let options = TxOptions::new(name("links_raw"), Isolation::ReadCommitted);
        let mut tx = self
            .backend
            .begin(self.catalog.clone(), options)
            .await
            .unwrap_or_else(|e| panic!("begin: {e}"));
        tx.insert(&data)
            .await
            .unwrap_or_else(|e| panic!("the raw insert must succeed: {e}"));
        tx.commit()
            .await
            .unwrap_or_else(|e| panic!("the raw commit must succeed: {e}"));
        doc
    }

    /// Posts a document through the engine.
    pub async fn post(&self, doctype: &str, doc: &Document) -> Document {
        let inv = self.invocation();
        let version = doc.version().unwrap_or(Version::FIRST);
        match self.engine.post(&inv, doctype, doc.id(), version).await {
            Ok(outcome) => outcome.value,
            Err(error) => panic!("post must succeed: {error}"),
        }
    }

    /// Cancels a posted document through the engine.
    pub async fn cancel(&self, doctype: &str, doc: &Document) -> Document {
        let inv = self.invocation();
        let version = doc.version().unwrap_or(Version::FIRST);
        match self.engine.cancel(&inv, doctype, doc.id(), version).await {
            Ok(outcome) => outcome.value,
            Err(error) => panic!("cancel must succeed: {error}"),
        }
    }

    /// Renames a document through the engine.
    pub async fn rename(
        &self,
        doctype: &str,
        doc: &Document,
        new_name: &str,
    ) -> AppResult<Document> {
        let inv = self.invocation();
        let version = doc.version().unwrap_or(Version::FIRST);
        Ok(self
            .engine
            .rename(&inv, doctype, doc.id(), new_name, version)
            .await?
            .value)
    }

    /// A scoped repository for the checks: reads through the shared store, writes refused.
    fn repo(&self) -> TestRepo {
        TestRepo {
            meta: self.meta.clone(),
            catalog: self.catalog.clone(),
            backend: self.backend.clone(),
        }
    }

    /// Runs [`spot_links::validate_links`] through a scoped repository.
    pub async fn validate(&self, doctype: &str, doc: &Document) -> AppResult<Vec<FieldIssue>> {
        let def = self.def(doctype);
        let mut repo = self.repo();
        let mut checks = RepoChecks::new(self.meta.clone(), &mut repo);
        spot_links::validate_links(&def, doc, &mut checks).await
    }

    /// Runs [`spot_links::where_used`] through a scoped repository, with the default sample.
    pub async fn where_used(&self, doctype: &str, id: RecordId) -> AppResult<WhereUsed> {
        self.where_used_limit(doctype, id, 0).await
    }

    /// Runs [`spot_links::where_used`] with an explicit sample limit.
    pub async fn where_used_limit(
        &self,
        doctype: &str,
        id: RecordId,
        limit: usize,
    ) -> AppResult<WhereUsed> {
        let mut repo = self.repo();
        let mut checks = RepoChecks::new(self.meta.clone(), &mut repo);
        spot_links::where_used(&mut checks, doctype, id, limit).await
    }

    /// Runs [`spot_links::check_rename_available`] through a scoped repository.
    pub async fn check_rename(&self, doctype: &str, id: RecordId, new_name: &str) -> AppResult<()> {
        let def = self.def(doctype);
        let mut repo = self.repo();
        let mut checks = RepoChecks::new(self.meta.clone(), &mut repo);
        spot_links::check_rename_available(&def, id, new_name, &mut checks).await
    }

    /// The storage view of a document (via the engine's peek), for existence assertions.
    pub async fn stored(&self, doctype: &str, id: RecordId) -> Option<Document> {
        self.engine
            .peek(doctype, id)
            .await
            .unwrap_or_else(|e| panic!("peek must succeed: {e}"))
    }
}

/// A storage name.
pub fn name(value: &str) -> Name {
    Name::new(value).unwrap_or_else(|e| panic!("`{value}` is a valid name: {e}"))
}

/// A text value.
pub fn text(value: &str) -> Value {
    Value::Text(value.to_owned())
}

/// A child row with `fields`.
pub fn row(fields: &[(&str, Value)]) -> spot_app_api::ChildRow {
    let mut map = BTreeMap::new();
    for (field, value) in fields {
        map.insert(name(field), value.clone());
    }
    spot_app_api::ChildRow {
        id: RecordId::new_v7(),
        fields: map,
    }
}

/// The app API's scoped repository over the shared store: reads run, writes are refused (the
/// engine owns them in the tests).
struct TestRepo {
    meta: Arc<EffectiveMeta>,
    catalog: Arc<Catalog>,
    backend: FakeBackend,
}

fn unsupported() -> AppError {
    AppError::new(
        ErrorClass::Internal,
        "platform.unsupported",
        "the links test repository only reads",
    )
}

impl TestRepo {
    fn storage_error(error: &spot_storage_contract::StorageError) -> AppError {
        spot_app_api::error::map_storage_error(error, StorageOp::Other)
    }
}

impl Repository for TestRepo {
    fn meta<'a>(&'a mut self, doctype: &'a str) -> BoxFuture<'a, AppResult<Arc<DocTypeDef>>> {
        Box::pin(async move {
            self.meta.doctype(doctype).ok_or_else(|| {
                AppError::new(
                    ErrorClass::UnknownDocType,
                    "meta.unknown_doctype",
                    format!("no DocType {doctype}"),
                )
            })
        })
    }

    fn load<'a>(
        &'a mut self,
        doctype: &'a str,
        id: RecordId,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async move {
            let entity = name(doctype);
            let options = TxOptions::new(name("links_load"), Isolation::ReadCommitted).read_only();
            let mut tx = self
                .backend
                .begin(self.catalog.clone(), options)
                .await
                .map_err(|e| TestRepo::storage_error(&e))?;
            let stored = tx.load(&entity, id, RowLock::None).await;
            let _ = tx.rollback().await;
            let stored = stored.map_err(|e| TestRepo::storage_error(&e))?;
            match stored {
                Some(stored) => mapping::from_storage(&self.meta, stored),
                None => Err(AppError::new(
                    ErrorClass::NotFound,
                    "document.not_found",
                    "The document does not exist.",
                )),
            }
        })
    }

    fn query<'a>(&'a mut self, query: &'a Query) -> BoxFuture<'a, AppResult<Rows>> {
        Box::pin(async move {
            if query.path != QueryPath::Transactional {
                return Err(unsupported());
            }
            let options = TxOptions::new(name("links_query"), Isolation::ReadCommitted).read_only();
            let mut tx = self
                .backend
                .begin(self.catalog.clone(), options)
                .await
                .map_err(|e| TestRepo::storage_error(&e))?;
            let rows = tx.query(query).await;
            let _ = tx.rollback().await;
            rows.map_err(|e| TestRepo::storage_error(&e))
        })
    }

    fn create(&mut self, _doc: Document) -> BoxFuture<'_, AppResult<Document>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn update<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
        _patch: DocumentPatch,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn post<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn cancel<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<Document>> {
        Box::pin(async { Err(unsupported()) })
    }

    fn delete<'a>(
        &'a mut self,
        _doctype: &'a str,
        _id: RecordId,
        _expected: Version,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async { Err(unsupported()) })
    }
}
