//! The records every site starts with: the `Administrator` user, the system roles, and the
//! Administrator's assignment to `Tenant Administrator` (ADR-070 sections 5 and 6).
//!
//! [`seed`] takes no password and writes no secret. The Administrator is created `invited`,
//! with no credential: the one-time recovery path that lets `spot new-site` print a first
//! sign-in secret exactly once is issued by the sign-in service through the secrets service,
//! not stored here. Every record has a fixed id from `bootstrap.toml`, so running [`seed`]
//! again finds them and writes nothing.
//!
//! # Transaction
//!
//! [`seed`] works on the [`DocumentStore`] of a transaction the caller opened. It writes
//! nothing outside it and does not commit; on any error the caller rolls the transaction back,
//! so a failed seed leaves no partial set. That is what "in one transaction" means here.
//!
//! # Journal capture (WP-JRN-02.T6)
//!
//! The seed writes rows at the storage level, not through the document engine, so it does not get the
//! engine's `doc_state` event for free. [`seed_with_capture`] and [`seed_system_rows_with_capture`]
//! take a [`StateCapture`]: right after each row is **inserted** the seed asks it for the `doc_state`
//! event of that insert and enqueues it in the same transaction, so a row never commits without its
//! state and a state never commits without its row. A row that already exists is not touched and
//! gets no event, so a second seed adds no event and no version. [`seed`] and [`seed_system_rows`]
//! capture nothing (they are [`NoCapture`]): a caller that wants the journal complete uses the
//! `_with_capture` form. This crate builds no event itself (it reads no clock and draws no id): the
//! host supplies the time, the ids and the metadata version through its [`StateCapture`].

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use spot_meta::{Definition, FieldType};
use spot_storage_contract::{
    DocumentData, DocumentStore, ErrorKind, Fields, Name, Outbox, OutboxEvent, RecordId, RowLock,
    StorageError, Value, Version,
};

use crate::{definitions, doctype};

/// The embedded `bootstrap.toml`.
const BOOTSTRAP: &str = include_str!("../../../../apps/platform/bootstrap.toml");

/// What varies per site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedParams {
    /// The Administrator's language, for example `en`.
    pub language: String,
    /// The Administrator's time zone, for example `UTC`.
    pub time_zone: String,
    /// The Administrator's e-mail address, when the site has one to give.
    pub email: Option<String>,
}

impl Default for SeedParams {
    fn default() -> Self {
        Self {
            language: "en".to_owned(),
            time_zone: "UTC".to_owned(),
            email: None,
        }
    }
}

/// What a [`seed`] run did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedOutcome {
    /// The Administrator's id: the same on every site and every run.
    pub administrator: RecordId,
    /// How many records this run wrote; 0 when everything was already there.
    pub written: usize,
}

/// Builds the journal event of a row the seed inserted. Implemented by the host, which owns the
/// clock, the id source and the metadata version the event needs.
pub trait StateCapture: Send {
    /// The `doc_state` insert event of `doc`, written at `version`, or `None` to capture nothing.
    ///
    /// # Errors
    ///
    /// [`SeedError`] when the event cannot be built; the seed fails and the caller rolls back.
    fn insert_event(
        &mut self,
        doc: &DocumentData,
        version: Version,
    ) -> Result<Option<OutboxEvent>, SeedError>;
}

/// Captures nothing: what [`seed`] and [`seed_system_rows`] use.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoCapture;

impl StateCapture for NoCapture {
    fn insert_event(
        &mut self,
        _doc: &DocumentData,
        _version: Version,
    ) -> Result<Option<OutboxEvent>, SeedError> {
        Ok(None)
    }
}

/// Why a [`seed`] run failed.
#[derive(Debug)]
pub enum SeedError {
    /// A DocType the seed writes is not in the catalogue of the transaction: the platform app
    /// has not been installed on this store yet.
    DoctypeMissing {
        /// The DocType.
        doctype: String,
        /// What storage said.
        detail: String,
    },
    /// The embedded definitions or bootstrap data are malformed (a test keeps that from
    /// shipping).
    Data(String),
    /// Storage failed for another reason.
    Storage(StorageError),
}

impl fmt::Display for SeedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DoctypeMissing { doctype, detail } => write!(
                f,
                "the platform DocType `{doctype}` is not installed on this store: {detail}"
            ),
            Self::Data(detail) => write!(f, "the platform bootstrap data is malformed: {detail}"),
            Self::Storage(e) => write!(f, "storage failed while seeding the platform app: {e}"),
        }
    }
}

impl std::error::Error for SeedError {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BootstrapFile {
    administrator: AdministratorRow,
    roles: Vec<RoleRow>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdministratorRow {
    id: String,
    user_name: String,
    full_name: String,
    user_type: String,
    state: String,
    role: String,
    assignment_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleRow {
    id: String,
    name: String,
    description: String,
}

/// Parses the hyphenated text of a UUIDv7 into a [`RecordId`].
fn record_id(text: &str) -> Result<RecordId, SeedError> {
    let bad = || SeedError::Data(format!("`{text}` is not a UUIDv7"));
    let hex: Vec<u8> = text.bytes().filter(|b| *b != b'-').collect();
    if hex.len() != 32 {
        return Err(bad());
    }
    let mut bytes = [0u8; 16];
    for (slot, pair) in bytes.iter_mut().zip(hex.chunks(2)) {
        let digit = |b: u8| char::from(b).to_digit(16).ok_or_else(bad);
        let (hi, lo) = match pair {
            [h, l] => (digit(*h)?, digit(*l)?),
            _ => return Err(bad()),
        };
        *slot = u8::try_from(hi * 16 + lo).map_err(|_| bad())?;
    }
    RecordId::from_bytes(bytes).map_err(|_| bad())
}

fn name(text: &str) -> Result<Name, SeedError> {
    Name::new(text).map_err(|e| SeedError::Data(format!("`{text}`: {e}")))
}

/// Every stored field of a DocType set to `Null`: a write must name exactly the entity's
/// declared fields. A child table has no column; a secret has none either, but its engine-managed
/// `<field>__sealed` (and, for a lookup field, `<field>__bidx`) columns are declared fields, so
/// they are written as `Null`: the seed sets no secret (WP-SEC-03.T3).
fn blank(defs: &[Definition], doctype: &str) -> Result<Fields, SeedError> {
    let def = defs
        .iter()
        .find(|d| d.name == doctype)
        .ok_or_else(|| SeedError::Data(format!("no definition for `{doctype}`")))?;
    let mut fields = BTreeMap::new();
    for f in &def.fields {
        if f.ty.is_table() {
            continue;
        }
        if f.ty == FieldType::Secret {
            fields.insert(name(&spot_meta::sealed_column(&f.name))?, Value::Null);
            if f.lookup_index() {
                fields.insert(name(&spot_meta::blind_index_column(&f.name))?, Value::Null);
            }
            continue;
        }
        fields.insert(name(&f.name)?, Value::Null);
    }
    Ok(fields)
}

fn set(fields: &mut Fields, field: &str, value: impl Into<Value>) -> Result<(), SeedError> {
    let key = name(field)?;
    if !fields.contains_key(&key) {
        return Err(SeedError::Data(format!("no stored field `{field}`")));
    }
    fields.insert(key, value.into());
    Ok(())
}

fn storage_error(doctype: &str, e: StorageError) -> SeedError {
    match e.kind {
        ErrorKind::UnknownEntity | ErrorKind::UnknownField => SeedError::DoctypeMissing {
            doctype: doctype.to_owned(),
            detail: e.to_string(),
        },
        _ => SeedError::Storage(e),
    }
}

/// Writes `doc` unless a document with its id exists, and enqueues its captured event in the same
/// transaction. Returns whether it wrote.
async fn ensure(
    store: &mut (impl DocumentStore + Outbox),
    capture: &mut dyn StateCapture,
    doc: DocumentData,
) -> Result<bool, SeedError> {
    let entity = doc.entity.as_str().to_owned();
    let found = store
        .load(&doc.entity, doc.id, RowLock::None)
        .await
        .map_err(|e| storage_error(&entity, e))?;
    if found.is_some() {
        return Ok(false);
    }
    let version = store
        .insert(&doc)
        .await
        .map_err(|e| storage_error(&entity, e))?;
    if let Some(event) = capture.insert_event(&doc, version)? {
        store
            .enqueue(&event)
            .await
            .map_err(|e| storage_error(&entity, e))?;
    }
    Ok(true)
}

fn document(entity: &str, id: RecordId, fields: Fields) -> Result<DocumentData, SeedError> {
    Ok(DocumentData {
        entity: name(entity)?,
        id,
        fields,
        children: BTreeMap::new(),
    })
}

/// What [`seed_system_rows`] did: the names of the rows it added (never a count alone).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SystemRowsOutcome {
    /// The system roles this run wrote, in `bootstrap.toml` order. Empty when everything was there.
    pub roles_added: Vec<String>,
}

impl SystemRowsOutcome {
    /// How many rows this run wrote.
    #[must_use]
    pub fn written(&self) -> usize {
        self.roles_added.len()
    }
}

fn read_bootstrap() -> Result<BootstrapFile, SeedError> {
    toml::from_str(BOOTSTRAP).map_err(|e| SeedError::Data(e.to_string()))
}

/// Writes the system roles, and any other non-identity row `bootstrap.toml` declares (today: roles
/// only), that are missing. Each is written by its fixed id through [`ensure`], so a row with that id
/// is left exactly as it is (an edited description survives) and a second run writes nothing.
///
/// This is what `spot migrate --apply` runs on an existing site, so a role a later task adds to
/// `bootstrap.toml` reaches sites created before it. It **never** writes the `Administrator`, a
/// `User`, a role assignment or any credential: those belong to [`seed`] at site creation.
///
/// # Errors
///
/// [`SeedError`]; the caller rolls its transaction back.
pub async fn seed_system_rows(
    store: &mut (impl DocumentStore + Outbox),
    params: &SeedParams,
) -> Result<SystemRowsOutcome, SeedError> {
    seed_system_rows_with_capture(store, params, &mut NoCapture).await
}

/// [`seed_system_rows`] that also enqueues, in the same transaction, the journal event of every
/// role it inserts (see the module docs).
///
/// # Errors
///
/// [`SeedError`]; the caller rolls its transaction back.
pub async fn seed_system_rows_with_capture(
    store: &mut (impl DocumentStore + Outbox),
    _params: &SeedParams,
    capture: &mut dyn StateCapture,
) -> Result<SystemRowsOutcome, SeedError> {
    let file = read_bootstrap()?;
    let defs = definitions().map_err(|e| SeedError::Data(e.to_string()))?;
    let mut outcome = SystemRowsOutcome::default();
    for row in &file.roles {
        let id = record_id(&row.id)?;
        let mut f = blank(&defs, doctype::ROLE)?;
        set(&mut f, "role_name", row.name.as_str())?;
        set(&mut f, "description", row.description.as_str())?;
        set(&mut f, "is_system", true)?;
        set(&mut f, "origin", "reference")?;
        if ensure(store, capture, document(doctype::ROLE, id, f)?).await? {
            outcome.roles_added.push(row.name.clone());
        }
    }
    Ok(outcome)
}

/// Creates the `Administrator` user, the system roles and the Administrator's assignment,
/// whatever is missing, in the caller's transaction (see the module docs).
///
/// It is [`seed_system_rows`] followed by the Administrator and its assignment, so creation and
/// migration cannot drift. Idempotent: a record with the expected id is left exactly as it is, so a
/// second run writes nothing and a site's later edits survive. Fails with
/// [`SeedError::DoctypeMissing`] when the platform DocTypes are not in the transaction's catalogue.
///
/// # Errors
///
/// [`SeedError`]; the caller rolls its transaction back.
pub async fn seed(
    store: &mut (impl DocumentStore + Outbox),
    params: &SeedParams,
) -> Result<SeedOutcome, SeedError> {
    seed_with_capture(store, params, &mut NoCapture).await
}

/// [`seed`] that also enqueues, in the same transaction, the journal event of every row it inserts
/// (the roles, the Administrator and the Administrator's assignment; see the module docs).
///
/// # Errors
///
/// [`SeedError`]; the caller rolls its transaction back.
pub async fn seed_with_capture(
    store: &mut (impl DocumentStore + Outbox),
    params: &SeedParams,
    capture: &mut dyn StateCapture,
) -> Result<SeedOutcome, SeedError> {
    let file = read_bootstrap()?;
    let defs = definitions().map_err(|e| SeedError::Data(e.to_string()))?;
    let mut written = seed_system_rows_with_capture(store, params, capture)
        .await?
        .written();

    let mut role_ids = BTreeMap::new();
    for row in &file.roles {
        role_ids.insert(row.name.clone(), record_id(&row.id)?);
    }

    let admin = &file.administrator;
    let admin_id = record_id(&admin.id)?;
    let mut f = blank(&defs, doctype::USER)?;
    set(&mut f, "user_name", admin.user_name.as_str())?;
    set(&mut f, "full_name", admin.full_name.as_str())?;
    set(&mut f, "user_type", admin.user_type.as_str())?;
    set(&mut f, "state", admin.state.as_str())?;
    set(&mut f, "language", params.language.as_str())?;
    set(&mut f, "time_zone", params.time_zone.as_str())?;
    set(&mut f, "failed_attempts", 0i64)?;
    if let Some(email) = &params.email {
        set(&mut f, "email", email.as_str())?;
    }
    written += usize::from(ensure(store, capture, document(doctype::USER, admin_id, f)?).await?);

    let role_id = *role_ids
        .get(&admin.role)
        .ok_or_else(|| SeedError::Data(format!("the Administrator's role `{}`", admin.role)))?;
    let mut f = blank(&defs, doctype::ROLE_ASSIGNMENT)?;
    set(&mut f, "user", admin_id)?;
    set(&mut f, "role", role_id)?;
    set(&mut f, "source", "manual")?;
    set(&mut f, "state", "active")?;
    set(&mut f, "reason", "Created with the site.")?;
    let assignment = record_id(&admin.assignment_id)?;
    written += usize::from(
        ensure(
            store,
            capture,
            document(doctype::ROLE_ASSIGNMENT, assignment, f)?,
        )
        .await?,
    );

    Ok(SeedOutcome {
        administrator: admin_id,
        written,
    })
}
