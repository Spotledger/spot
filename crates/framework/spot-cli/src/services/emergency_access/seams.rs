//! The parts emergency access calls out to, each behind a small trait so the flow is tested on
//! fakes and the site wires the real ones:
//!
//! - [`Assignments`]: the assignment records. [`PgAssignments`] writes the time-boxed
//!   `role_assignment` (source `emergency`) and its identity-topic event in **one transaction**, so
//!   there is no assignment without its event and no event without its assignment, and the record
//!   id is derived from the grant id, so a retry after a crash writes nothing twice.
//! - [`Recording`]: the archive of an ended session. [`BreakGlassRecording`] hands the hash-chained
//!   frames to the break-glass archive (sealed, written once, digest on the audit chain); with no
//!   archive wired the status of the grant says `unavailable`.
//! - [`EmergencyLauncher`]: starts the approval Case on the hosted process engine, keyed by the
//!   grant id, the same way the grant approval does.
//! - [`Ids`]: new request ids (injected: nothing here reads entropy).

use std::collections::BTreeMap;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use spot_bpm_model::ProcessVersion;
use spot_core::Clock;
use spot_events_contract::payload::{EventMeta, PayloadDocument};
use spot_events_contract::{Actor, Topic};
use spot_http::api::service::BoxFuture;
use spot_iam_governance::approval::LaunchReceipt;
use spot_iam_governance::{ChangeActor, ChangeEvent, GovError, GrantRequestId, ProcessLauncher};
use spot_iam_roles::{Assignment, Source};
use spot_iam_scim::parse_record_id;
use spot_platform_app::doctype;
use spot_postgres::PgBackend;
use spot_storage_contract::{
    Catalog, DocumentStore, Isolation, NodeRef, Outbox, RecordId, RowLock, StorageBackend,
    Transaction, TxOptions, Value,
};
use time::OffsetDateTime;

use super::model::{CODE_UNAVAILABLE, CODE_WRITE_FAILED};
use crate::services::break_glass::{BreakGlassPort, Window};
use crate::services::process::ProcessHost;
use crate::services::scim_store::{assignment_fields, blank, doc, find_id, idv, nm, rows, text};

/// A fixed failure code, never a message.
pub type Code = &'static str;

type Tx = <PgBackend as StorageBackend>::Tx;

/// What an activation writes.
#[derive(Clone, Debug)]
pub struct Activation {
    /// The grant id.
    pub grant: String,
    /// The user (a record id).
    pub user: String,
    /// The elevated role's name.
    pub role: String,
    /// The window start, epoch seconds.
    pub valid_from: i64,
    /// The window end, epoch seconds.
    pub valid_to: i64,
    /// The reason.
    pub reason: String,
    /// The change the write records on the identity topic.
    pub event: ChangeEvent,
}

/// The assignment records of the site.
pub trait Assignments: Send + Sync {
    /// The assignment record of `grant`, when it was written.
    fn find<'a>(&'a self, grant: &'a str) -> BoxFuture<'a, Result<Option<String>, Code>>;
    /// Writes the time-boxed assignment and its event, once: a second call for the same grant
    /// writes nothing and answers the same record.
    fn grant<'a>(&'a self, activation: &'a Activation) -> BoxFuture<'a, Result<String, Code>>;
    /// Ends the assignment of `grant` at `at` (the window closes at the earlier of `at` and its
    /// own end) and records `event`, once. Ending an assignment that is already over is `Ok`.
    fn end<'a>(
        &'a self,
        grant: &'a str,
        at: i64,
        event: &'a ChangeEvent,
    ) -> BoxFuture<'a, Result<(), Code>>;
    /// The names of the roles `user` holds through an assignment active at `at` (the approver
    /// check of a `role:<name>` approver).
    fn roles_of<'a>(&'a self, user: &'a str, at: i64) -> BoxFuture<'a, Result<Vec<String>, Code>>;
}

/// The record id an emergency grant's assignment gets: the first 16 bytes of the SHA-256 of the
/// site and the grant id, with the version-7 and variant bits set (the storage contract refuses
/// any other version). The same grant always has the same record.
#[must_use]
pub fn assignment_record(site: &str, grant: &str) -> Option<RecordId> {
    let digest = Sha256::digest(format!("emergency_access:{site}:{grant}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(digest.get(..16)?);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    RecordId::from_bytes(bytes).ok()
}

fn instant(at: i64) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp(at).ok()
}

/// The production assignment records, over the site's database.
pub struct PgAssignments {
    site: String,
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl PgAssignments {
    /// Records of `site` over `backend`.
    #[must_use]
    pub fn new(site: &str, backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self {
            site: site.to_owned(),
            backend,
            catalog,
        }
    }

    async fn begin(&self, label: &str, read_only: bool) -> Result<Tx, Code> {
        let name = nm(label).map_err(|_| CODE_UNAVAILABLE)?;
        let options = TxOptions::new(name, Isolation::Serializable);
        let options = if read_only {
            options.read_only()
        } else {
            options
        };
        self.backend
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|_| CODE_UNAVAILABLE)
    }

    async fn finish<R>(tx: Tx, result: Result<R, Code>) -> Result<R, Code> {
        match result {
            Ok(value) => {
                tx.commit().await.map_err(|_| CODE_WRITE_FAILED)?;
                Ok(value)
            }
            Err(code) => {
                let _ = tx.rollback().await;
                Err(code)
            }
        }
    }

    /// The identity-topic outbox event of `event`, about `user`.
    fn outbox(
        event: &ChangeEvent,
        user: RecordId,
        now: OffsetDateTime,
    ) -> Result<spot_storage_contract::OutboxEvent, Code> {
        let id = RecordId::new_v7();
        let actor = match &event.actor {
            ChangeActor::User { id, .. } => {
                parse_record_id(id).map(|id| Actor::User { id, via: None })
            }
            ChangeActor::System { .. } => None,
        };
        let actor = match actor {
            Some(actor) => actor,
            None => Actor::System {
                component: nm("emergency_access").map_err(|_| CODE_WRITE_FAILED)?,
            },
        };
        let meta = EventMeta {
            occurred_at: now,
            actor,
            command_id: None,
            causation_id: None,
            correlation_id: id,
            trace_id: None,
        };
        let topic = Topic::any(event.topic()).map_err(|_| CODE_WRITE_FAILED)?;
        let data = event.to_identity_data().map_err(|_| CODE_WRITE_FAILED)?;
        let subject = NodeRef {
            entity: nm(doctype::USER).map_err(|_| CODE_WRITE_FAILED)?,
            id: user,
        };
        PayloadDocument { meta, data }
            .to_outbox_event(id, &topic, subject, None, 1)
            .map_err(|_| CODE_WRITE_FAILED)
    }
}

impl Assignments for PgAssignments {
    fn find<'a>(&'a self, grant: &'a str) -> BoxFuture<'a, Result<Option<String>, Code>> {
        Box::pin(async move {
            let id = assignment_record(&self.site, grant).ok_or(CODE_UNAVAILABLE)?;
            let mut tx = self.begin("emergency_assignment_find", true).await?;
            let entity = nm(doctype::ROLE_ASSIGNMENT).map_err(|_| CODE_UNAVAILABLE);
            let found = match entity {
                Ok(entity) => tx
                    .load(&entity, id, RowLock::None)
                    .await
                    .map(|d| d.map(|_| id.to_string()))
                    .map_err(|_| CODE_UNAVAILABLE),
                Err(code) => Err(code),
            };
            Self::finish(tx, found).await
        })
    }

    fn grant<'a>(&'a self, activation: &'a Activation) -> BoxFuture<'a, Result<String, Code>> {
        Box::pin(async move {
            let id = assignment_record(&self.site, &activation.grant).ok_or(CODE_WRITE_FAILED)?;
            let user = parse_record_id(&activation.user).ok_or(CODE_WRITE_FAILED)?;
            let (from, to) = (
                instant(activation.valid_from).ok_or(CODE_WRITE_FAILED)?,
                instant(activation.valid_to).ok_or(CODE_WRITE_FAILED)?,
            );
            let defs = spot_platform_app::definitions().map_err(|_| CODE_UNAVAILABLE)?;
            let mut tx = self.begin("emergency_assignment_grant", false).await?;
            let result = async {
                let entity = nm(doctype::ROLE_ASSIGNMENT).map_err(|_| CODE_UNAVAILABLE)?;
                if tx
                    .load(&entity, id, RowLock::None)
                    .await
                    .map_err(|_| CODE_UNAVAILABLE)?
                    .is_some()
                {
                    return Ok(id.to_string());
                }
                let role = find_id(&mut tx, doctype::ROLE, "role_name", &activation.role)
                    .await
                    .map_err(|_| CODE_UNAVAILABLE)?
                    .ok_or(CODE_WRITE_FAILED)?;
                let mut assignment =
                    Assignment::new(&id.to_string(), &user.to_string(), &activation.role);
                assignment.source = Source::Emergency;
                assignment.valid_from = Some(from);
                assignment.valid_to = Some(to);
                assignment.reason = Some(activation.reason.clone());
                let roles = BTreeMap::from([(activation.role.clone(), role)]);
                let mut fields =
                    blank(&defs, doctype::ROLE_ASSIGNMENT).map_err(|_| CODE_WRITE_FAILED)?;
                assignment_fields(&mut fields, &assignment, &roles, from)
                    .map_err(|_| CODE_WRITE_FAILED)?;
                let data =
                    doc(doctype::ROLE_ASSIGNMENT, id, fields).map_err(|_| CODE_WRITE_FAILED)?;
                tx.insert(&data).await.map_err(|_| CODE_WRITE_FAILED)?;
                let event = Self::outbox(&activation.event, user, from)?;
                tx.enqueue(&event).await.map_err(|_| CODE_WRITE_FAILED)?;
                Ok(id.to_string())
            }
            .await;
            Self::finish(tx, result).await
        })
    }

    fn end<'a>(
        &'a self,
        grant: &'a str,
        at: i64,
        event: &'a ChangeEvent,
    ) -> BoxFuture<'a, Result<(), Code>> {
        Box::pin(async move {
            let id = assignment_record(&self.site, grant).ok_or(CODE_WRITE_FAILED)?;
            let when = instant(at).ok_or(CODE_WRITE_FAILED)?;
            let mut tx = self.begin("emergency_assignment_end", false).await?;
            let result = async {
                let entity = nm(doctype::ROLE_ASSIGNMENT).map_err(|_| CODE_UNAVAILABLE)?;
                let Some(loaded) = tx
                    .load(
                        &entity,
                        id,
                        RowLock::Update(spot_storage_contract::Wait::Block),
                    )
                    .await
                    .map_err(|_| CODE_UNAVAILABLE)?
                else {
                    return Ok(());
                };
                let mut data = loaded.data;
                let state = data
                    .fields
                    .get(&nm("state").map_err(|_| CODE_UNAVAILABLE)?)
                    .and_then(text);
                if state.as_deref() == Some("ended") {
                    return Ok(());
                }
                let current_end = data
                    .fields
                    .get(&nm("valid_to").map_err(|_| CODE_UNAVAILABLE)?)
                    .and_then(|v| match v {
                        Value::DateTime(t) => Some(*t),
                        _ => None,
                    });
                let closes = current_end.map_or(when, |end| end.min(when));
                data.fields
                    .insert(nm("state").map_err(|_| CODE_UNAVAILABLE)?, "ended".into());
                data.fields.insert(
                    nm("valid_to").map_err(|_| CODE_UNAVAILABLE)?,
                    Value::DateTime(closes),
                );
                tx.update(&data, loaded.version)
                    .await
                    .map_err(|_| CODE_WRITE_FAILED)?;
                let user = data
                    .fields
                    .get(&nm("user").map_err(|_| CODE_UNAVAILABLE)?)
                    .and_then(idv)
                    .ok_or(CODE_WRITE_FAILED)?;
                let outbox = Self::outbox(event, user, when)?;
                tx.enqueue(&outbox).await.map_err(|_| CODE_WRITE_FAILED)?;
                Ok(())
            }
            .await;
            Self::finish(tx, result).await
        })
    }

    fn roles_of<'a>(&'a self, user: &'a str, at: i64) -> BoxFuture<'a, Result<Vec<String>, Code>> {
        Box::pin(async move {
            let user = parse_record_id(user).ok_or(CODE_UNAVAILABLE)?;
            let now = instant(at).ok_or(CODE_UNAVAILABLE)?;
            let mut tx = self.begin("emergency_assignment_roles", true).await?;
            let result = async {
                let roles = rows(&mut tx, doctype::ROLE, &["id", "role_name"])
                    .await
                    .map_err(|_| CODE_UNAVAILABLE)?;
                let held = rows(
                    &mut tx,
                    doctype::ROLE_ASSIGNMENT,
                    &["user", "role", "state", "valid_from", "valid_to"],
                )
                .await
                .map_err(|_| CODE_UNAVAILABLE)?;
                let window = |v: &Value| match v {
                    Value::DateTime(t) => Some(*t),
                    _ => None,
                };
                let mut names = Vec::new();
                for row in &held {
                    let active = text(&row[2]).as_deref() == Some("active")
                        && idv(&row[0]) == Some(user)
                        && window(&row[3]).is_none_or(|t| t <= now)
                        && window(&row[4]).is_none_or(|t| t > now);
                    if !active {
                        continue;
                    }
                    if let Some(role) = idv(&row[1])
                        && let Some(name) = roles
                            .iter()
                            .find(|r| idv(&r[0]) == Some(role))
                            .and_then(|r| text(&r[1]))
                    {
                        names.push(name);
                    }
                }
                Ok(names)
            }
            .await;
            Self::finish(tx, result).await
        })
    }
}

/// The archive of an ended session's recording.
pub trait Recording: Send + Sync {
    /// Archives the frames `bytes` of `grant` (the session ran from `starts_at` to `ends_at`);
    /// the head digest, or the code of why it could not.
    fn archive<'a>(
        &'a self,
        grant: &'a str,
        user: &'a str,
        starts_at: i64,
        ends_at: i64,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<String, Code>>;
}

/// No archive is wired: archiving always says so.
pub struct NoRecording;

impl Recording for NoRecording {
    fn archive<'a>(
        &'a self,
        _grant: &'a str,
        _user: &'a str,
        _starts_at: i64,
        _ends_at: i64,
        _bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<String, Code>> {
        Box::pin(async { Err(super::model::CODE_RECORDING_UNAVAILABLE) })
    }
}

/// The break-glass archive of the site (`WP-ADM-11.T2`): a sealed, write-once recording whose head
/// digest goes to the audit chain.
pub struct BreakGlassRecording {
    port: Arc<dyn BreakGlassPort>,
    tenant: String,
}

impl BreakGlassRecording {
    /// An archive for `tenant` over `port`.
    #[must_use]
    pub fn new(port: Arc<dyn BreakGlassPort>, tenant: &str) -> Self {
        Self {
            port,
            tenant: tenant.to_owned(),
        }
    }
}

impl Recording for BreakGlassRecording {
    fn archive<'a>(
        &'a self,
        grant: &'a str,
        user: &'a str,
        starts_at: i64,
        ends_at: i64,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<String, Code>> {
        Box::pin(async move {
            let session = format!("emergency-{grant}");
            self.port
                .register(Window {
                    tenant: self.tenant.clone(),
                    session: session.clone(),
                    requester: user.to_owned(),
                    starts_at,
                    expires_at: ends_at,
                })
                .await
                .map_err(|_| super::model::CODE_RECORDING_UNAVAILABLE)?;
            self.port
                .archive_recording(&session, bytes)
                .await
                .map_err(|_| super::model::CODE_RECORDING_UNAVAILABLE)
        })
    }
}

/// New request ids.
pub trait Ids: Send + Sync {
    /// A fresh id: lower-case letters, digits and `-`, at most 64 characters.
    fn next(&self) -> String;
}

/// Ids from the storage contract's record ids (time-ordered, random).
pub struct RecordIds;

impl Ids for RecordIds {
    fn next(&self) -> String {
        format!("em-{}", RecordId::new_v7().to_string().replace('-', ""))
    }
}

/// Runs a synchronous call that may block without stalling the runtime: off the worker on a
/// multi-thread runtime, in place otherwise.
pub fn run_blocking<T>(call: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(call)
        }
        _ => call(),
    }
}

/// Starts approval Cases on the hosted engine, keyed by the grant id (a launch retried after a
/// crash resumes the same Case and never starts a second one).
pub struct EmergencyLauncher {
    host: Arc<ProcessHost>,
    clock: Arc<dyn Clock>,
}

impl EmergencyLauncher {
    /// A launcher over the process host.
    #[must_use]
    pub fn new(host: Arc<ProcessHost>, clock: Arc<dyn Clock>) -> Self {
        Self { host, clock }
    }
}

impl ProcessLauncher for EmergencyLauncher {
    fn launch(
        &self,
        process: &ProcessVersion,
        request: &GrantRequestId,
    ) -> Result<LaunchReceipt, GovError> {
        let failed = |detail: &str| GovError::ApprovalProcess {
            detail: detail.to_owned(),
        };
        let engine = self
            .host
            .engine()
            .map_err(|_| failed("the process engine is not running"))?;
        engine
            .publish(process)
            .map_err(|_| failed("the approval process did not compile"))?;
        let host = Arc::clone(&self.host);
        let now = self.clock.now_utc();
        let key = format!("emergency-approval:{}", request.as_str());
        let name = process.name.clone();
        let input = serde_json::json!({"request": request.as_str()});
        // The trait is synchronous; the caller runs it under `run_blocking`.
        let case = tokio::runtime::Handle::current()
            .block_on(async move { host.start_run(&name, None, input, None, &key, now).await })
            .map_err(|_| failed("the approval case could not be started"))?;
        Ok(LaunchReceipt {
            case_id: case.to_string(),
            process_hash: process.content_hash().to_string(),
        })
    }
}
