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

use std::collections::BTreeMap;
use std::fmt;

use serde::Deserialize;
use spot_meta::{Definition, FieldType};
use spot_storage_contract::{
    DocumentData, DocumentStore, ErrorKind, Fields, Name, RecordId, RowLock, StorageError, Value,
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

/// Writes `doc` unless a document with its id exists. Returns whether it wrote.
async fn ensure(store: &mut impl DocumentStore, doc: DocumentData) -> Result<bool, SeedError> {
    let entity = doc.entity.as_str().to_owned();
    let found = store
        .load(&doc.entity, doc.id, RowLock::None)
        .await
        .map_err(|e| storage_error(&entity, e))?;
    if found.is_some() {
        return Ok(false);
    }
    store
        .insert(&doc)
        .await
        .map_err(|e| storage_error(&entity, e))?;
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

/// Creates the `Administrator` user, the system roles and the Administrator's assignment,
/// whatever is missing, in the caller's transaction (see the module docs).
///
/// Idempotent: a record with the expected id is left exactly as it is, so a second run writes
/// nothing and a site's later edits survive. Fails with [`SeedError::DoctypeMissing`] when the
/// platform DocTypes are not in the transaction's catalogue.
///
/// # Errors
///
/// [`SeedError`]; the caller rolls its transaction back.
pub async fn seed(
    store: &mut impl DocumentStore,
    params: &SeedParams,
) -> Result<SeedOutcome, SeedError> {
    let file: BootstrapFile =
        toml::from_str(BOOTSTRAP).map_err(|e| SeedError::Data(e.to_string()))?;
    let defs = definitions().map_err(|e| SeedError::Data(e.to_string()))?;
    let mut written = 0usize;

    let mut role_ids = BTreeMap::new();
    for row in &file.roles {
        let id = record_id(&row.id)?;
        role_ids.insert(row.name.clone(), id);
        let mut f = blank(&defs, doctype::ROLE)?;
        set(&mut f, "role_name", row.name.as_str())?;
        set(&mut f, "description", row.description.as_str())?;
        set(&mut f, "is_system", true)?;
        set(&mut f, "origin", "reference")?;
        written += usize::from(ensure(store, document(doctype::ROLE, id, f)?).await?);
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
    written += usize::from(ensure(store, document(doctype::USER, admin_id, f)?).await?);

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
    written +=
        usize::from(ensure(store, document(doctype::ROLE_ASSIGNMENT, assignment, f)?).await?);

    Ok(SeedOutcome {
        administrator: admin_id,
        written,
    })
}
