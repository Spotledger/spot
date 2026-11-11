//! File storage for one site: the adapter from the file service to `GET/POST /api/v1/files`.
//!
//! `spot-http` owns the routes ([`FilesPort`]); `spot-files-service` owns the rules (type and
//! size policy, scan seam, `document.not_found` for everything a caller may not see). This module
//! wires the two together and supplies what the service leaves to the host:
//!
//! - **the bytes**: the provider named by `services.files.provider`, `local` (a folder under the
//!   site's own folder, `files` by default) or `memory` (tests);
//! - **the records**: [`DocRecords`], the service's record store over the platform `file`
//!   DocType, written and read through the document engine as the platform itself, so a record
//!   survives a restart and is audited like any other document;
//! - **who is calling**: [`Callers`] resolves the bearer credential with the site's sign-in
//!   service (a session that has not chosen its password yet is a guest here too);
//! - **may this caller read the document a file hangs off**: [`Access`], which asks the same
//!   engine, as the caller, so read permission and field rights are the document's own.
//!
//! # Fail closed
//!
//! Permission is decided inside the service, once per file: an unattached file is readable by
//! its uploader only; an attached file by whoever may read its document (and field). Every
//! refusal, an unknown id included, is the same `404 document.not_found`. No route lives in this
//! crate, no path or storage key ever leaves the process, and file bytes are never logged.

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};

use axum::body::Bytes;
use spot_app_api::meta::FieldKind;
use spot_app_api::{HookPoint, Operation, Phase, Target};
use spot_app_api::context::{Actor, DocumentService, Invocation};
use spot_app_api::prelude::{Name, RecordId, Value, Version};
use spot_app_api::{Document, DocumentPatch};
use spot_core::{Clock, SiteId};
use spot_document::engine::{
    Authorizer, CommittedBatch, CommittedDelivery, CommittedSteps, CommittedStep, OutboxSink,
};
use spot_files::local::LocalFileStore;
use spot_files::memory::MemoryFileStore;
use spot_files::store::{Body, FileStore};
use spot_files_service::{
    AttachAccess, AttachTarget, Caller, CallerSource, Download, DownloadRequest, DownloadStatus,
    FileCtx, FilePolicy, FileRecord, FileRecordStore, FileServiceError, FileState, FilesApi,
    FilesService, FileAttachHook, UploadRequest, storage_key,
};
use spot_http::api::files::{
    BoxByteStream, ByteStream, FileDownload, FileGetRequest, FileGetStatus, FilePortError,
    FileRecord as PortRecord, FileUpload, FilesPort,
};
use spot_http::api::service::{ApiError, BoxFuture, Credential, RequestCtx};
use spot_http::request::RequestId;
use spot_permissions::Action;
use spot_postgres::PgBackend;
use spot_storage_contract::{
    CmpOp, Expr, Isolation, Nulls, Order, OrderDir, Query, QueryExecutor, QueryPath, Selection,
    StorageBackend, Transaction, TxOptions,
};
use tokio::io::{AsyncRead, ReadBuf};
use tokio::sync::watch;

use crate::audit::PathClass;
use crate::live::SiteLive;
use crate::services::{CheckFuture, Level, RunFuture, Service};

type ServiceResult<T> = Result<T, FileServiceError>;

/// The DocType the records live in.
const FILE: &str = "file";

/// The files routes' path: the port is not handed the request path, so the class of a refused
/// key is computed from this, in the one place that maps a path to a class.
const FILES_ROUTE: &str = "/api/v1/files";

fn internal(message: impl Into<String>) -> FileServiceError {
    FileServiceError::Internal {
        message: message.into(),
    }
}

fn name(text: &str) -> ServiceResult<Name> {
    Name::new(text).map_err(|e| internal(e.to_string()))
}

/// The invocation of the platform acting on its own records.
fn system(site: &SiteId, clock: &dyn Clock) -> ServiceResult<Invocation> {
    Ok(Invocation {
        site: site.clone(),
        actor: Actor::System {
            component: name("files")?,
        },
        command_id: None,
        language: None,
        now: clock.now_utc(),
    })
}

/// Everything the file service asks the host for.
pub struct Settings {
    /// The site.
    pub site: SiteId,
    /// What the site serves right now.
    pub live: Arc<SiteLive>,
    /// The site's database.
    pub backend: PgBackend,
    /// The source of `now`.
    pub clock: Arc<dyn Clock>,
    /// `local` or `memory`.
    pub provider: String,
    /// The local provider's root, already inside the site folder.
    pub root: PathBuf,
    /// The hard per-upload limit.
    pub max_bytes: u64,
    /// Whether losing the files stops the site (the cluster profile).
    pub required: bool,
}

/// The `file` DocType as the service's record store.
pub struct DocRecords {
    live: Arc<SiteLive>,
    backend: PgBackend,
    clock: Arc<dyn Clock>,
    site: SiteId,
}

fn text(doc: &Document, field: &str) -> Option<String> {
    match doc.get(field) {
        Value::Text(text) => Some(text.clone()),
        _ => None,
    }
}

fn flag(doc: &Document, field: &str) -> bool {
    matches!(doc.get(field), Value::Bool(true))
}

fn fields_of(record: &FileRecord) -> Vec<(&'static str, Value)> {
    let target = record.attached_to.as_ref();
    vec![
        ("name", Value::Text(record.name.clone())),
        ("original_name", Value::Text(record.original_name.clone())),
        ("content_type", Value::Text(record.content_type.clone())),
        (
            "size",
            Value::Int(i64::try_from(record.size).unwrap_or(i64::MAX)),
        ),
        (
            "digest",
            Value::Text(spot_files_service::hex_lower(&record.digest)),
        ),
        ("private", Value::Bool(record.private)),
        (
            "attached_doctype",
            target.map_or(Value::Null, |t| Value::Text(t.doctype.clone())),
        ),
        (
            "attached_id",
            target.map_or(Value::Null, |t| Value::Text(t.id.to_string())),
        ),
        (
            "attached_field",
            target
                .and_then(|t| t.field.clone())
                .map_or(Value::Null, Value::Text),
        ),
        ("uploaded_by", Value::Id(record.uploaded_by)),
        ("created_at", Value::DateTime(record.created)),
        ("state", Value::Text(record.state.as_str().to_owned())),
        ("quarantined", Value::Bool(record.quarantined)),
    ]
}

impl DocRecords {
    fn engine(&self) -> ServiceResult<Arc<spot_document::engine::Engine<PgBackend>>> {
        self.live
            .engine()
            .ok_or_else(|| internal("the site has no running engine"))
    }

    async fn load(&self, id: RecordId) -> ServiceResult<Option<Document>> {
        let engine = self.engine()?;
        let inv = system(&self.site, self.clock.as_ref())?;
        match engine.load(&inv, FILE, id).await {
            Ok(loaded) => Ok(Some(loaded.value)),
            Err(e) if e.class == spot_app_api::ErrorClass::NotFound => Ok(None),
            Err(e) => Err(FileServiceError::store(e.to_string())),
        }
    }

    fn record(&self, doc: &Document) -> ServiceResult<FileRecord> {
        let digest_text = text(doc, "digest").ok_or_else(|| internal("a file has no digest"))?;
        let mut digest = [0u8; 32];
        if digest_text.len() != 64 {
            return Err(internal("a file has a malformed digest"));
        }
        for (i, byte) in digest.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&digest_text[2 * i..2 * i + 2], 16)
                .map_err(|_| internal("a file has a malformed digest"))?;
        }
        let private = flag(doc, "private");
        let created = match doc.get("created_at") {
            Value::DateTime(at) => *at,
            _ => return Err(internal("a file has no creation time")),
        };
        let attached_to = match (text(doc, "attached_doctype"), text(doc, "attached_id")) {
            (Some(doctype), Some(id)) => {
                let id = spot_http::api::codec::parse_record_id(&id)
                    .ok_or_else(|| internal("a file is attached to a malformed id"))?;
                Some(AttachTarget {
                    doctype,
                    id,
                    field: text(doc, "attached_field"),
                })
            }
            _ => None,
        };
        Ok(FileRecord {
            id: doc.id(),
            site: self.site.clone(),
            name: text(doc, "name").unwrap_or_default(),
            original_name: text(doc, "original_name").unwrap_or_default(),
            content_type: text(doc, "content_type").unwrap_or_default(),
            size: u64::try_from(doc.int("size").ok().flatten().unwrap_or(0)).unwrap_or(0),
            digest,
            private,
            attached_to,
            uploaded_by: doc
                .link("uploaded_by")
                .ok()
                .flatten()
                .ok_or_else(|| internal("a file has no uploader"))?,
            created,
            state: text(doc, "state")
                .and_then(|s| FileState::parse(&s))
                .unwrap_or(FileState::Pending),
            quarantined: flag(doc, "quarantined"),
            key: storage_key(doc.id(), created)?,
        })
    }

    /// The ids of the records matching `filter`, oldest first.
    async fn ids(&self, filter: Expr, limit: u32) -> ServiceResult<Vec<RecordId>> {
        let meta = self
            .live
            .served()
            .ok_or_else(|| internal("the site serves no metadata"))?
            .meta
            .clone();
        let catalog = Arc::new(crate::schema::catalog(&meta).map_err(|e| internal(e.0))?);
        let entity = name(FILE)?;
        let alias = name("f")?;
        let mut query = Query::from(&entity, &alias, QueryPath::Report);
        query.select = vec![Selection {
            expr: Expr::field(&alias, &name("id")?),
            alias: name("v")?,
        }];
        query.filter = Some(filter);
        query.order_by = vec![Order {
            expr: Expr::field(&alias, &name("created_at")?),
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        }];
        query.limit = Some(limit);
        let mut tx = self
            .backend
            .begin(
                catalog,
                TxOptions::new(name("spot_cli_files")?, Isolation::ReadCommitted),
            )
            .await
            .map_err(|e| FileServiceError::store(e.to_string()))?;
        let rows = tx.query(&query).await;
        let _ = tx.rollback().await;
        let rows = rows.map_err(|e| FileServiceError::store(e.to_string()))?;
        Ok(rows
            .rows
            .into_iter()
            .flatten()
            .filter_map(|value| match value {
                Value::Id(id) => Some(id),
                _ => None,
            })
            .collect())
    }

    async fn records(&self, filter: Expr, limit: u32) -> ServiceResult<Vec<FileRecord>> {
        let mut found = Vec::new();
        for id in self.ids(filter, limit).await? {
            if let Some(doc) = self.load(id).await? {
                found.push(self.record(&doc)?);
            }
        }
        Ok(found)
    }

    fn field(&self, field: &str) -> ServiceResult<Expr> {
        Ok(Expr::field(&name("f")?, &name(field)?))
    }

    /// One page of the documents of `doctype` that name a file in any of `fields`, in id order
    /// after `after`: each with its `(field, file id)` pairs.
    async fn named_files(
        &self,
        doctype: &str,
        fields: &[String],
        after: Option<RecordId>,
        limit: u32,
    ) -> ServiceResult<Vec<(RecordId, Vec<(String, RecordId)>)>> {
        let meta = self
            .live
            .served()
            .ok_or_else(|| internal("the site serves no metadata"))?
            .meta
            .clone();
        let catalog = Arc::new(crate::schema::catalog(&meta).map_err(|e| internal(e.0))?);
        let entity = name(doctype)?;
        let alias = name("d")?;
        let mut query = Query::from(&entity, &alias, QueryPath::Report);
        let id_field = Expr::field(&alias, &name("id")?);
        let mut selection = vec![Selection {
            expr: id_field.clone(),
            alias: name("v0")?,
        }];
        let mut named = Vec::new();
        for (i, field) in fields.iter().enumerate() {
            let expr = Expr::field(&alias, &name(field)?);
            selection.push(Selection {
                expr: expr.clone(),
                alias: name(&format!("v{}", i + 1))?,
            });
            named.push(Expr::Not(Box::new(Expr::IsNull(Box::new(expr)))));
        }
        query.select = selection;
        let mut parts = vec![Expr::Or(named)];
        if let Some(after) = after {
            parts.push(id_field.clone().cmp(CmpOp::Gt, Expr::value(Value::Id(after))));
        }
        query.filter = Some(Expr::And(parts));
        query.order_by = vec![Order {
            expr: id_field,
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        }];
        query.limit = Some(limit);
        let mut tx = self
            .backend
            .begin(
                catalog,
                TxOptions::new(name("spot_cli_files")?, Isolation::ReadCommitted),
            )
            .await
            .map_err(|e| FileServiceError::store(e.to_string()))?;
        let rows = tx.query(&query).await;
        let _ = tx.rollback().await;
        let rows = rows.map_err(|e| FileServiceError::store(e.to_string()))?;
        let mut found = Vec::new();
        for row in rows.rows {
            let mut cells = row.into_iter();
            let Some(Value::Id(id)) = cells.next() else {
                continue;
            };
            let pairs = fields
                .iter()
                .zip(cells)
                .filter_map(|(field, value)| match value {
                    Value::Id(file) => Some((field.clone(), file)),
                    _ => None,
                })
                .collect();
            found.push((id, pairs));
        }
        Ok(found)
    }
}

impl FileRecordStore for DocRecords {
    fn insert(&self, record: FileRecord) -> spot_files_service::BoxFuture<'_, ServiceResult<()>> {
        Box::pin(async move {
            let engine = self.engine()?;
            let inv = system(&self.site, self.clock.as_ref())?;
            let mut doc = Document::new(name(FILE)?, record.id);
            for (field, value) in fields_of(&record) {
                doc.set(name(field)?, value);
            }
            engine
                .create(&inv, doc)
                .await
                .map(|_| ())
                .map_err(|e| FileServiceError::store(e.to_string()))
        })
    }

    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        id: RecordId,
    ) -> spot_files_service::BoxFuture<'a, ServiceResult<Option<FileRecord>>> {
        Box::pin(async move {
            if site != &self.site {
                return Ok(None);
            }
            match self.load(id).await? {
                Some(doc) => Ok(Some(self.record(&doc)?)),
                None => Ok(None),
            }
        })
    }

    fn update(&self, record: FileRecord) -> spot_files_service::BoxFuture<'_, ServiceResult<()>> {
        Box::pin(async move {
            let engine = self.engine()?;
            let inv = system(&self.site, self.clock.as_ref())?;
            let Some(current) = self.load(record.id).await? else {
                return Err(FileServiceError::NotFound);
            };
            let version: Version = current
                .version()
                .ok_or_else(|| internal("a stored file has no version"))?;
            let mut patch = DocumentPatch::default();
            for (field, value) in fields_of(&record) {
                patch.fields.insert(name(field)?, value);
            }
            engine
                .update(&inv, FILE, record.id, patch, version)
                .await
                .map(|_| ())
                .map_err(|e| FileServiceError::store(e.to_string()))
        })
    }

    fn delete<'a>(
        &'a self,
        site: &'a SiteId,
        id: RecordId,
    ) -> spot_files_service::BoxFuture<'a, ServiceResult<bool>> {
        Box::pin(async move {
            if site != &self.site {
                return Ok(false);
            }
            let engine = self.engine()?;
            let inv = system(&self.site, self.clock.as_ref())?;
            let Some(current) = self.load(id).await? else {
                return Ok(false);
            };
            let version = current
                .version()
                .ok_or_else(|| internal("a stored file has no version"))?;
            engine
                .delete(&inv, FILE, id, version)
                .await
                .map(|_| true)
                .map_err(|e| FileServiceError::store(e.to_string()))
        })
    }

    fn orphans<'a>(
        &'a self,
        site: &'a SiteId,
        older_than: time::OffsetDateTime,
        limit: usize,
    ) -> spot_files_service::BoxFuture<'a, ServiceResult<Vec<FileRecord>>> {
        Box::pin(async move {
            if site != &self.site {
                return Ok(Vec::new());
            }
            let filter = Expr::And(vec![
                self.field("state")?.eq(Expr::value(Value::Text(
                    FileState::Orphaned.as_str().into(),
                ))),
                self.field("created_at")?
                    .cmp(CmpOp::Le, Expr::value(Value::DateTime(older_than))),
            ]);
            self.records(filter, u32::try_from(limit).unwrap_or(u32::MAX))
                .await
        })
    }

    fn by_target<'a>(
        &'a self,
        site: &'a SiteId,
        target: &'a AttachTarget,
    ) -> spot_files_service::BoxFuture<'a, ServiceResult<Vec<FileRecord>>> {
        Box::pin(async move {
            if site != &self.site {
                return Ok(Vec::new());
            }
            let mut parts = vec![
                self.field("attached_doctype")?
                    .eq(Expr::value(Value::Text(target.doctype.clone()))),
                self.field("attached_id")?
                    .eq(Expr::value(Value::Text(target.id.to_string()))),
            ];
            if let Some(field) = &target.field {
                parts.push(
                    self.field("attached_field")?
                        .eq(Expr::value(Value::Text(field.clone()))),
                );
            }
            self.records(Expr::And(parts), 1000).await
        })
    }

    fn by_document<'a>(
        &'a self,
        site: &'a SiteId,
        doctype: &'a str,
        id: RecordId,
    ) -> spot_files_service::BoxFuture<'a, ServiceResult<Vec<FileRecord>>> {
        Box::pin(async move {
            if site != &self.site {
                return Ok(Vec::new());
            }
            let filter = Expr::And(vec![
                self.field("attached_doctype")?
                    .eq(Expr::value(Value::Text(doctype.to_owned()))),
                self.field("attached_id")?
                    .eq(Expr::value(Value::Text(id.to_string()))),
            ]);
            self.records(filter, 1000).await
        })
    }

    fn known_keys<'a>(
        &'a self,
        site: &'a SiteId,
    ) -> spot_files_service::BoxFuture<'a, ServiceResult<Vec<spot_files::key::FileKey>>> {
        Box::pin(async move {
            if site != &self.site {
                return Ok(Vec::new());
            }
            let mut keys = Vec::new();
            for id in self.ids(Expr::And(Vec::new()), u32::MAX).await? {
                if let Some(doc) = self.load(id).await? {
                    keys.push(self.record(&doc)?.key);
                }
            }
            Ok(keys)
        })
    }
}

/// Resolves the bearer credential with the site's sign-in service.
pub struct Callers {
    live: Arc<SiteLive>,
}

impl CallerSource for Callers {
    fn caller<'a>(
        &'a self,
        ctx: &'a FileCtx,
    ) -> spot_files_service::BoxFuture<'a, ServiceResult<Caller>> {
        Box::pin(async move {
            let Some(token) = &ctx.credential else {
                return Ok(Caller::Guest);
            };
            let Some(auth) = self.live.auth() else {
                return Ok(Caller::Guest);
            };
            let request = RequestCtx {
                site: ctx.site.clone(),
                credential: Some(Credential::new(token.clone())),
                idempotency_key: None,
                language: None,
                request_id: RequestId::generate(),
                peer: None,
            };
            Ok(match auth.authenticate_on(&request, PathClass::of_path(FILES_ROUTE)).await {
                // A caller that has not chosen a password yet may do nothing with files.
                Ok(who) if !auth.is_restricted(&who) => Caller::User(who.user_id),
                _ => Caller::Guest,
            })
        })
    }
}

/// May this caller read, or upload to, the document a file hangs off?
pub struct Access {
    live: Arc<SiteLive>,
    clock: Arc<dyn Clock>,
    site: SiteId,
}

impl Access {
    fn invocation(&self, user: RecordId) -> Invocation {
        Invocation {
            site: self.site.clone(),
            actor: Actor::User {
                id: user,
                via: None,
            },
            command_id: None,
            language: None,
            now: self.clock.now_utc(),
        }
    }

    /// The document as the caller reads it, or `None` when the caller may not.
    async fn read(&self, user: RecordId, target: &AttachTarget) -> Option<Document> {
        let engine = self.live.engine()?;
        let inv = self.invocation(user);
        let loaded = engine.load(&inv, &target.doctype, target.id).await.ok()?;
        Some(loaded.value)
    }
}

impl AttachAccess for Access {
    fn may_read<'a>(
        &'a self,
        caller: &'a Caller,
        target: &'a AttachTarget,
    ) -> spot_files_service::BoxFuture<'a, ServiceResult<bool>> {
        Box::pin(async move {
            let Some(user) = caller.user_id() else {
                return Ok(false);
            };
            let Some(doc) = self.read(user, target).await else {
                return Ok(false);
            };
            // A field the caller may not read is absent from the document they get, and a
            // file that hangs off it is as hidden as the field is.
            Ok(match &target.field {
                Some(field) => doc.fields().keys().any(|k| k.as_str() == field),
                None => true,
            })
        })
    }

    fn may_upload<'a>(
        &'a self,
        caller: &'a Caller,
        target: &'a AttachTarget,
    ) -> spot_files_service::BoxFuture<'a, ServiceResult<bool>> {
        Box::pin(async move {
            let Some(user) = caller.user_id() else {
                return Ok(false);
            };
            let Some(doc) = self.read(user, target).await else {
                return Ok(false);
            };
            let Some(authorizer) = self.live.authorizer() else {
                return Ok(false);
            };
            let inv = self.invocation(user);
            let principal = authorizer.principal_for(&inv).await;
            Ok(authorizer
                .authorize(&principal, Action::Op(Operation::Update), &doc)
                .await
                .is_ok())
        })
    }
}

/// An upload's byte stream as the file service reads it. A failure of the stream is kept, so the
/// caller gets the error that ended the body and not the reader's generic one.
struct StreamBody {
    stream: BoxByteStream,
    chunk: Bytes,
    failure: Arc<std::sync::Mutex<Option<ApiError>>>,
}

impl AsyncRead for StreamBody {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if !self.chunk.is_empty() {
                let n = self.chunk.len().min(buf.remaining());
                let part = self.chunk.split_to(n);
                buf.put_slice(&part);
                return Poll::Ready(Ok(()));
            }
            match self.stream.as_mut().poll_chunk(cx) {
                Poll::Ready(Some(Ok(bytes))) => self.chunk = bytes,
                Poll::Ready(Some(Err(error))) => {
                    *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error);
                    return Poll::Ready(Err(std::io::Error::other("the upload body failed")));
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// A download's reader as the router pulls it: 64 KiB at a time.
struct ReadStream {
    body: Body,
}

impl ByteStream for ReadStream {
    fn poll_chunk(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Bytes, ApiError>>> {
        let mut buffer = vec![0u8; 64 * 1024];
        let mut read = ReadBuf::new(&mut buffer);
        match self.body.as_mut().poll_read(cx, &mut read) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(_)) => Poll::Ready(Some(Err(ApiError::from_code(
                spot_http::api::registry::Code::FileStoreFailed,
            )))),
            Poll::Ready(Ok(())) => {
                let filled = read.filled().len();
                if filled == 0 {
                    Poll::Ready(None)
                } else {
                    buffer.truncate(filled);
                    Poll::Ready(Some(Ok(Bytes::from(buffer))))
                }
            }
        }
    }
}

fn port_error(error: &FileServiceError) -> FilePortError {
    FilePortError::new(
        error.status(),
        error.code(),
        error.to_string(),
        error.retryable(),
    )
}

/// The file service as `spot-http`'s [`FilesPort`].
pub struct Files {
    service: FilesService,
    records: Arc<DocRecords>,
    access: Arc<Access>,
    store: Arc<dyn FileStore>,
    max_bytes: u64,
    reconciled: AtomicBool,
}

impl Files {
    /// Builds the service over the provider and the record store the settings name.
    pub fn new(settings: &Settings) -> Arc<Self> {
        let store: Arc<dyn FileStore> = match settings.provider.as_str() {
            "memory" => Arc::new(MemoryFileStore::new()),
            _ => Arc::new(LocalFileStore::new(settings.root.clone())),
        };
        let records = Arc::new(DocRecords {
            live: Arc::clone(&settings.live),
            backend: settings.backend.clone(),
            clock: Arc::clone(&settings.clock),
            site: settings.site.clone(),
        });
        let access = Arc::new(Access {
            live: Arc::clone(&settings.live),
            clock: Arc::clone(&settings.clock),
            site: settings.site.clone(),
        });
        let policy = FilePolicy {
            max_upload_bytes: settings.max_bytes,
            ..FilePolicy::default()
        };
        let service = FilesService::new(
            Arc::clone(&store),
            records.clone(),
            Arc::new(Callers {
                live: Arc::clone(&settings.live),
            }),
            access.clone(),
            policy,
            Arc::clone(&settings.clock),
        );
        Arc::new(Self {
            service,
            records,
            access,
            store,
            max_bytes: settings.max_bytes,
            reconciled: AtomicBool::new(false),
        })
    }

    /// The attach step the document engine's committed phase drives: `attachments` are the
    /// `attach` field values (`field -> file id`) the document was saved with. The files named
    /// become attached to the document; any other file attached to it is orphaned.
    ///
    /// # Errors
    /// The record store's failures.
    pub async fn attach_saved(
        &self,
        site: &SiteId,
        doctype: &str,
        id: RecordId,
        attachments: &[(String, RecordId)],
    ) -> Result<(), FileServiceError> {
        self.service
            .attach_saved(site, doctype, id, attachments)
            .await
    }

    /// The byte store behind the service: mail reads its attachments from it.
    pub fn store(&self) -> Arc<dyn FileStore> {
        Arc::clone(&self.store)
    }

    fn context(ctx: &RequestCtx) -> FileCtx {
        FileCtx {
            site: ctx.site.clone(),
            credential: ctx.credential.as_ref().map(|c| c.token.clone()),
        }
    }
}

impl FileAttachHook for Files {
    fn on_document_saved<'a>(
        &'a self,
        site: &'a SiteId,
        doctype: &'a str,
        id: RecordId,
        attachments: &'a [(String, RecordId)],
    ) -> spot_files_service::BoxFuture<'a, ServiceResult<()>> {
        self.service
            .on_document_saved(site, doctype, id, attachments)
    }

    fn on_document_deleted<'a>(
        &'a self,
        site: &'a SiteId,
        doctype: &'a str,
        id: RecordId,
    ) -> spot_files_service::BoxFuture<'a, ServiceResult<()>> {
        self.service.on_document_deleted(site, doctype, id)
    }
}

impl Files {
    /// Re-applies the attach step for every document whose `attach` field names a file that is
    /// still `pending`: the repair for a crash between a commit and its in-process committed
    /// step (the host's delivery is not durable, see [`AttachDelivery`]). Runs once per
    /// process; the step is idempotent, so a node that starts while another already repaired
    /// the same documents changes nothing. Returns how many documents were repaired.
    ///
    /// # Errors
    /// The record store's failures; the host logs them and starts anyway, because the repair is
    /// best effort and the next start runs it again.
    pub async fn reconcile_attachments(&self) -> Result<usize, FileServiceError> {
        if self.reconciled.swap(true, Ordering::AcqRel) {
            return Ok(0);
        }
        let outcome = self.reconcile_pending().await;
        if outcome.is_err() {
            self.reconciled.store(false, Ordering::Release);
        }
        outcome
    }

    async fn reconcile_pending(&self) -> Result<usize, FileServiceError> {
        const PAGE: u32 = 500;
        let site = self.records.site.clone();
        let Some(served) = self.records.live.served() else {
            return Ok(0);
        };
        let mut repaired = 0;
        for def in served.meta.doctypes() {
            if def.is_child {
                continue;
            }
            let fields = attach_fields(def);
            if fields.is_empty() {
                continue;
            }
            let doctype = def.name.as_str().to_owned();
            let mut after = None;
            loop {
                let page = self
                    .records
                    .named_files(&doctype, &fields, after, PAGE)
                    .await?;
                let Some((last, _)) = page.last() else {
                    break;
                };
                after = Some(*last);
                let full = page.len() >= PAGE as usize;
                for (id, pairs) in page {
                    // The writer of the lost step is not known any more, so a pending file is
                    // re-attached only when its uploader can read the document (the same bar a
                    // save by the uploader passes); a file somebody else uploaded stays pending.
                    let mut pending = false;
                    let mut kept = Vec::new();
                    for (field, file) in pairs {
                        let Some(record) = self.records.get(&site, file).await? else {
                            continue;
                        };
                        if record.state != FileState::Pending {
                            // Already this document's through this field: keep it named, so
                            // the step does not orphan it. Any other file (attached to another
                            // document, or orphaned) is never taken over here.
                            let own = record.state == FileState::Attached
                                && record.attached_to.as_ref().is_some_and(|t| {
                                    t.doctype == doctype
                                        && t.id == id
                                        && t.field.as_deref() == Some(field.as_str())
                                });
                            if own {
                                kept.push((field, file));
                            }
                            continue;
                        }
                        let target = AttachTarget::field(&doctype, id, field.clone());
                        let uploader = Caller::User(record.uploaded_by);
                        if !record.quarantined
                            && self.access.may_read(&uploader, &target).await?
                        {
                            pending = true;
                            kept.push((field, file));
                        }
                    }
                    if pending {
                        self.service
                            .on_document_saved(&site, &doctype, id, &kept)
                            .await?;
                        repaired += 1;
                    }
                }
                if !full {
                    break;
                }
            }
        }
        if repaired > 0 {
            tracing::warn!(repaired, "re-applied lost file attach steps at start");
        }
        Ok(repaired)
    }
}

/// The names of a DocType's `attach` fields.
fn attach_fields(def: &spot_app_api::DocTypeDef) -> Vec<String> {
    def.fields
        .iter()
        .filter(|f| matches!(f.kind, FieldKind::Attach { .. }))
        .map(|f| f.name.as_str().to_owned())
        .collect()
}

/// Whether a file may be attached by a save: the decision the files service does not make for
/// itself, because its attach step is not told who saved.
pub trait AttachGuard: Send + Sync + 'static {
    /// `actor` saved document `id` of `doctype` naming `file` in `field`. `true` only when that
    /// file is the actor's to attach: unknown, quarantined and other documents' files never are.
    fn may_attach<'a>(
        &'a self,
        actor: &'a Actor,
        doctype: &'a str,
        id: RecordId,
        file: RecordId,
    ) -> spot_files_service::BoxFuture<'a, bool>;
}

impl AttachGuard for Files {
    fn may_attach<'a>(
        &'a self,
        actor: &'a Actor,
        doctype: &'a str,
        id: RecordId,
        file: RecordId,
    ) -> spot_files_service::BoxFuture<'a, bool> {
        Box::pin(async move {
            let site = &self.records.site;
            let Ok(Some(record)) = self.records.get(site, file).await else {
                return false; // unknown, or the record store failed: fail closed
            };
            if record.quarantined {
                return false;
            }
            if let Some(target) = &record.attached_to {
                // Already this document's (the value did not change), or somebody else's.
                return target.doctype == doctype && target.id == id;
            }
            match actor {
                Actor::User { id: user, .. } => record.uploaded_by == *user,
                Actor::System { .. } => true,
                _ => false,
            }
        })
    }
}

/// The hook and the guard a filled slot holds.
type Bound<'a> = (&'a Arc<dyn FileAttachHook>, &'a Arc<dyn AttachGuard>);

/// The late-bound place the host keeps the attach hook in.
///
/// The host builds the engine, and with it the delivery, before the files service exists, and
/// the engine cannot change its delivery afterwards; so the delivery holds this slot and the
/// service fills it once it is built. An empty slot (a host without the files service) means
/// the wrapper only delegates.
#[derive(Default)]
pub struct AttachSlot {
    hook: OnceLock<Arc<dyn FileAttachHook>>,
    guard: OnceLock<Arc<dyn AttachGuard>>,
}

impl AttachSlot {
    /// An empty slot.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Fills the slot with the files service; the first one stays (`false` when it was already
    /// filled).
    pub fn fill(&self, files: Arc<Files>) -> bool {
        let guard: Arc<dyn AttachGuard> = files.clone();
        let hook: Arc<dyn FileAttachHook> = files;
        let _ = self.guard.set(guard);
        self.hook.set(hook).is_ok()
    }

    fn get(&self) -> Option<Bound<'_>> {
        Some((self.hook.get()?, self.guard.get()?))
    }
}

/// The delivery that drives the attach lifecycle in the committed phase of a save or a delete.
///
/// It wraps the delivery the host already chose and delegates to it first, then, for every
/// `save_committed` step of a DocType with `attach` fields, hands the files the saved document
/// names to the hook (which attaches them and orphans whatever the document no longer names),
/// and for every `delete_committed` step orphans the document's files. A failure is logged and
/// never fails the save.
///
/// The host's delivery is the in-process one, so a crash between commit and step loses the
/// attach; the hook is idempotent and [`Files::reconcile_attachments`] repairs the loss at
/// start.
pub struct AttachDelivery {
    inner: Arc<dyn CommittedDelivery>,
    slot: Arc<AttachSlot>,
    live: Arc<SiteLive>,
}

impl AttachDelivery {
    /// Wraps `inner`; `live` supplies the metadata the `attach` fields are read from.
    pub fn new(
        inner: Arc<dyn CommittedDelivery>,
        slot: Arc<AttachSlot>,
        live: Arc<SiteLive>,
    ) -> Arc<Self> {
        Arc::new(Self { inner, slot, live })
    }

    async fn attach(&self, batch: &CommittedBatch) {
        let Some((hook, guard)) = self.slot.get() else {
            return;
        };
        for step in &batch.steps {
            self.step(hook.as_ref(), guard.as_ref(), batch, step).await;
        }
    }

    async fn step(
        &self,
        hook: &dyn FileAttachHook,
        guard: &dyn AttachGuard,
        batch: &CommittedBatch,
        step: &CommittedStep,
    ) {
        let saved = match &step.point {
            HookPoint::Lifecycle {
                phase: Phase::Committed,
                target: Target::Save,
            } => true,
            HookPoint::Lifecycle {
                phase: Phase::Committed,
                target: Target::Op(Operation::Delete),
            } => false,
            _ => return,
        };
        let doctype = step.doc.doctype().as_str();
        let Some(def) = self
            .live
            .served()
            .and_then(|served| served.meta.doctype(doctype))
        else {
            return;
        };
        let fields = attach_fields(&def);
        if fields.is_empty() {
            return;
        }
        let id = step.doc.id();
        let site = &batch.invocation.site;
        let outcome = if saved {
            let mut pairs: Vec<(String, RecordId)> = Vec::new();
            for field in fields {
                let Value::Id(file) = step.doc.get(&field) else {
                    continue;
                };
                if guard
                    .may_attach(&batch.invocation.actor, doctype, id, *file)
                    .await
                {
                    pairs.push((field, *file));
                } else {
                    tracing::warn!(
                        doctype,
                        document = %id,
                        field = %field,
                        "a saved document names a file its writer may not attach; not attached"
                    );
                }
            }
            hook.on_document_saved(site, doctype, id, &pairs).await
        } else {
            hook.on_document_deleted(site, doctype, id).await
        };
        if let Err(error) = outcome {
            tracing::warn!(
                doctype,
                document = %id,
                step = %step.point.name(),
                error = %error,
                "file attach step failed; the document was saved"
            );
        }
    }
}

impl CommittedDelivery for AttachDelivery {
    fn prepare<'a>(
        &'a self,
        outbox: &'a mut dyn OutboxSink,
        batch: &'a CommittedBatch,
    ) -> spot_app_api::BoxFuture<'a, spot_app_api::AppResult<()>> {
        self.inner.prepare(outbox, batch)
    }

    fn deliver<'a>(
        &'a self,
        steps: &'a dyn CommittedSteps,
        batch: CommittedBatch,
    ) -> spot_app_api::BoxFuture<'a, Vec<spot_app_api::Message>> {
        Box::pin(async move {
            let messages = self.inner.deliver(steps, batch.clone()).await;
            self.attach(&batch).await;
            messages
        })
    }
}

impl FilesPort for Files {
    fn max_upload_bytes(&self) -> u64 {
        self.max_bytes
    }

    fn upload<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        upload: FileUpload,
        body: BoxByteStream,
    ) -> BoxFuture<'a, Result<PortRecord, FilePortError>> {
        Box::pin(async move {
            let failure = Arc::new(std::sync::Mutex::new(None));
            let reader: Body = Box::pin(StreamBody {
                stream: body,
                chunk: Bytes::new(),
                failure: Arc::clone(&failure),
            });
            let mut request = UploadRequest::new(upload.name);
            request.content_type = upload.content_type;
            match self
                .service
                .upload(&Self::context(ctx), request, reader)
                .await
            {
                Ok(view) => Ok(PortRecord {
                    id: view.id,
                    name: view.name,
                    size: view.size,
                    content_type: view.content_type,
                    digest: view.digest,
                    private: view.private,
                }),
                Err(error) => {
                    // The body's own failure (an aborted request, a stream the router stopped)
                    // is the better answer than the store's.
                    if let Some(stream_error) =
                        failure.lock().unwrap_or_else(|p| p.into_inner()).take()
                    {
                        return Err(FilePortError::from(stream_error));
                    }
                    Err(port_error(&error))
                }
            }
        })
    }

    fn get<'a>(
        &'a self,
        ctx: &'a RequestCtx,
        id: RecordId,
        request: FileGetRequest,
    ) -> BoxFuture<'a, Result<FileDownload, FilePortError>> {
        Box::pin(async move {
            let mut asked = DownloadRequest::new();
            asked.range = request.range;
            asked.if_none_match = request.if_none_match;
            let download = self
                .service
                .download(&Self::context(ctx), id, asked)
                .await
                .map_err(|e| port_error(&e))?;
            Ok(into_download(download))
        })
    }
}

fn into_download(download: Download) -> FileDownload {
    let status = match download.status {
        DownloadStatus::Full => FileGetStatus::Full,
        DownloadStatus::Partial => FileGetStatus::Partial,
        DownloadStatus::NotModified => FileGetStatus::NotModified,
        DownloadStatus::RangeNotSatisfiable => FileGetStatus::RangeNotSatisfiable,
    };
    let mut headers = vec![
        ("ETag".to_owned(), download.etag),
        ("Accept-Ranges".to_owned(), "bytes".to_owned()),
    ];
    if matches!(
        status,
        FileGetStatus::Full | FileGetStatus::Partial | FileGetStatus::RangeNotSatisfiable
    ) {
        headers.push(("Content-Type".to_owned(), download.content_type));
        headers.push((
            "Content-Disposition".to_owned(),
            download.content_disposition,
        ));
    }
    if let Some(range) = download.content_range {
        headers.push(("Content-Range".to_owned(), range.to_header()));
    }
    FileDownload {
        status,
        headers,
        body: download
            .body
            .map(|body| Box::pin(ReadStream { body }) as BoxByteStream),
    }
}

/// The start-up check of the file provider: the folder exists and takes a write.
pub struct FilesCheck {
    provider: String,
    root: PathBuf,
    required: bool,
}

impl FilesCheck {
    /// The service for the settings' provider.
    pub fn new(settings: &Settings) -> Arc<Self> {
        Arc::new(Self {
            provider: settings.provider.clone(),
            root: settings.root.clone(),
            required: settings.required,
        })
    }
}

impl Service for FilesCheck {
    fn name(&self) -> &'static str {
        "files"
    }

    fn level(&self) -> Level {
        if self.required {
            Level::Required
        } else {
            Level::Optional
        }
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if self.provider != "local" {
                return Ok(());
            }
            tokio::fs::create_dir_all(&self.root)
                .await
                .map_err(|e| format!("the file folder cannot be created: {e}"))?;
            let probe = self.root.join(".probe");
            tokio::fs::write(&probe, b"spot")
                .await
                .map_err(|e| format!("the file folder takes no writes: {e}"))?;
            let _ = tokio::fs::remove_file(&probe).await;
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
}
