//! The stored side of the model: roles with their duty rows, every assignment of every user type
//! with its scope rows, and the users' types, read inside the caller's one transaction.
//!
//! Unlike the SCIM store's loader, nothing is filtered here: an inactive assignment is kept (the
//! assembly closes its window) and an assignment of a portal or service user is kept (the portal
//! rule is applied to it by the questions, not hidden by the reader). A row that does not decode
//! is skipped and counted by the assembly, never guessed.

use std::sync::Arc;

use spot_http::api::service::BoxFuture;
use spot_postgres::PgBackend;
use spot_storage_contract::{
    Catalog, Isolation, Name, RecordId, RowLock, StorageBackend, Transaction, TxOptions, Value,
};

use super::ModelError;
use super::assemble::{RawAssignment, RawDirectory, RawRole, RawUser};
use super::events::Touched;
use crate::services::scim_store::{at, idv, int, nm, permission_of, rows, text};

/// The reader the model source calls for the stored directory.
pub trait DirectoryReader: Send + Sync {
    /// Reads roles, assignments and users in one consistent read-only transaction.
    ///
    /// # Errors
    ///
    /// [`ModelError::DirectoryUnavailable`]: nothing is returned for a part that failed.
    fn read<'a>(&'a self) -> BoxFuture<'a, Result<RawDirectory, ModelError>>;

    /// The directory again after the records `touched` names changed, keeping from `previous`
    /// what did not: the child rows (a role's duties, an assignment's scope) of every record that
    /// is not named are taken from `previous`, not read. The default reads everything, which is
    /// always correct.
    ///
    /// # Errors
    ///
    /// [`ModelError::DirectoryUnavailable`].
    fn read_delta<'a>(
        &'a self,
        previous: &'a RawDirectory,
        touched: &'a Touched,
    ) -> BoxFuture<'a, Result<RawDirectory, ModelError>> {
        let _ = (previous, touched);
        self.read()
    }
}

/// The production reader over the site's database.
pub struct PgDirectory {
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

impl PgDirectory {
    /// A reader over `backend`, with the site's catalogue.
    #[must_use]
    pub fn new(backend: PgBackend, catalog: Arc<Catalog>) -> Self {
        Self { backend, catalog }
    }
}

impl DirectoryReader for PgDirectory {
    fn read<'a>(&'a self) -> BoxFuture<'a, Result<RawDirectory, ModelError>> {
        Box::pin(async move {
            let name = Name::new("access_questions_model").map_err(|_| unavailable())?;
            // One snapshot for every table: the roles, the assignments and the users the model
            // joins are the same point in time.
            let options = TxOptions::new(name, Isolation::RepeatableRead).read_only();
            let mut tx = self
                .backend
                .begin(Arc::clone(&self.catalog), options)
                .await
                .map_err(|_| unavailable())?;
            let read = read_directory(&mut tx).await;
            let _ = tx.rollback().await;
            read
        })
    }

    fn read_delta<'a>(
        &'a self,
        previous: &'a RawDirectory,
        touched: &'a Touched,
    ) -> BoxFuture<'a, Result<RawDirectory, ModelError>> {
        Box::pin(async move {
            let name = Name::new("access_questions_model").map_err(|_| unavailable())?;
            let options = TxOptions::new(name, Isolation::RepeatableRead).read_only();
            let mut tx = self
                .backend
                .begin(Arc::clone(&self.catalog), options)
                .await
                .map_err(|_| unavailable())?;
            let read = read_directory_delta(&mut tx, previous, touched).await;
            let _ = tx.rollback().await;
            read
        })
    }
}

fn unavailable() -> ModelError {
    ModelError::DirectoryUnavailable
}

/// The DocType names of the platform records read here.
mod table {
    pub(super) const ROLE: &str = "role";
    pub(super) const ROLE_ASSIGNMENT: &str = "role_assignment";
    pub(super) const USER: &str = "user";
}

fn nonneg(v: &Value) -> Option<u32> {
    int(v).and_then(|i| u32::try_from(i).ok())
}

/// Reads the directory through `tx`.
///
/// # Errors
///
/// [`ModelError::DirectoryUnavailable`] when any table cannot be read.
pub async fn read_directory<T: Transaction + Send>(tx: &mut T) -> Result<RawDirectory, ModelError> {
    read_with(tx, None).await
}

/// [`read_directory`] that reads the child rows of the records `touched` names only, and takes the
/// others' from `previous`.
///
/// # Errors
///
/// [`ModelError::DirectoryUnavailable`] when any table cannot be read.
pub async fn read_directory_delta<T: Transaction + Send>(
    tx: &mut T,
    previous: &RawDirectory,
    touched: &Touched,
) -> Result<RawDirectory, ModelError> {
    read_with(tx, Some((previous, touched))).await
}

type Delta<'a> = Option<(&'a RawDirectory, &'a Touched)>;

async fn read_with<T: Transaction + Send>(
    tx: &mut T,
    delta: Delta<'_>,
) -> Result<RawDirectory, ModelError> {
    let roles = read_roles(tx, delta).await?;
    let assignments = read_assignments(tx, delta).await?;
    let users = rows(tx, table::USER, &["id", "user_type"])
        .await
        .map_err(|_| unavailable())?
        .iter()
        .filter_map(|r| {
            Some(RawUser {
                id: idv(&r[0])?.to_string(),
                user_type: text(&r[1])?,
            })
        })
        .collect();
    Ok(RawDirectory {
        roles,
        assignments,
        users,
    })
}

async fn child_rows<T: Transaction + Send>(
    tx: &mut T,
    entity: &Name,
    id: RecordId,
    table: &str,
) -> Result<Vec<spot_storage_contract::ChildRow>, ModelError> {
    let loaded = tx
        .load(entity, id, RowLock::None)
        .await
        .map_err(|_| unavailable())?;
    let table = Name::new(table).map_err(|_| unavailable())?;
    Ok(loaded
        .and_then(|d| d.data.children.get(&table).cloned())
        .unwrap_or_default())
}

async fn read_roles<T: Transaction + Send>(
    tx: &mut T,
    delta: Delta<'_>,
) -> Result<Vec<RawRole>, ModelError> {
    let entity = nm(table::ROLE).map_err(|_| unavailable())?;
    let stored = rows(
        tx,
        table::ROLE,
        &["id", "role_name", "origin", "is_system", "copied_from"],
    )
    .await
    .map_err(|_| unavailable())?;
    let mut out = Vec::with_capacity(stored.len());
    for r in &stored {
        let (Some(id), Some(name)) = (idv(&r[0]), text(&r[1])) else {
            continue;
        };
        // An untouched role keeps the duty rows it had; a touched or new one is read again.
        let kept = delta.and_then(|(previous, touched)| {
            let key = id.to_string();
            if touched.roles.contains(&key) {
                return None;
            }
            previous
                .roles
                .iter()
                .find(|p| p.id == key)
                .map(|p| p.duties.clone())
        });
        let duties = match kept {
            Some(duties) => duties,
            None => {
                let mut duties = Vec::new();
                for row in child_rows(tx, &entity, id, "duties").await? {
                    let field =
                        |k: &str| Name::new(k).ok().and_then(|n| row.fields.get(&n).cloned());
                    if let Some(duty) = field("duty").as_ref().and_then(text) {
                        duties.push((duty, field("duty_version").as_ref().and_then(nonneg)));
                    }
                }
                duties
            }
        };
        out.push(RawRole {
            id: id.to_string(),
            name,
            origin: text(&r[2]),
            is_system: matches!(r[3], Value::Bool(true)),
            copied_from: idv(&r[4]).map(|c| c.to_string()),
            duties,
            // The role DocType declares neither a parent table nor a kind today: a stored role
            // is a desk role without parents until it does (the assembly reads them when set).
            parents: Vec::new(),
            kind: None,
        });
    }
    Ok(out)
}

async fn read_assignments<T: Transaction + Send>(
    tx: &mut T,
    delta: Delta<'_>,
) -> Result<Vec<RawAssignment>, ModelError> {
    let entity = nm(table::ROLE_ASSIGNMENT).map_err(|_| unavailable())?;
    let stored = rows(
        tx,
        table::ROLE_ASSIGNMENT,
        &[
            "id",
            "user",
            "role",
            "source",
            "state",
            "valid_from",
            "valid_to",
            "reason",
        ],
    )
    .await
    .map_err(|_| unavailable())?;
    let mut out = Vec::with_capacity(stored.len());
    for r in &stored {
        let (Some(id), Some(user), Some(role)) = (idv(&r[0]), idv(&r[1]), idv(&r[2])) else {
            continue;
        };
        let state = text(&r[4]).unwrap_or_default();
        // The engine reads the scope rows of every assignment it counts, whatever its source, so
        // so does the model; an assignment that is not active counts nowhere and needs none.
        let kept = delta.and_then(|(previous, touched)| {
            let key = id.to_string();
            if touched.assignments.contains(&key) {
                return None;
            }
            previous
                .assignments
                .iter()
                .find(|p| p.id == key && p.state == state)
                .map(|p| p.scope.clone())
        });
        let scope = match kept {
            Some(scope) => scope,
            None if state == "active" => child_rows(tx, &entity, id, "scope")
                .await?
                .iter()
                .filter_map(permission_of)
                .collect(),
            None => Vec::new(),
        };
        out.push(RawAssignment {
            id: id.to_string(),
            user: user.to_string(),
            role_id: role.to_string(),
            source: text(&r[3]).unwrap_or_default(),
            state,
            valid_from: at(&r[5]),
            valid_to: at(&r[6]),
            reason: text(&r[7]),
            scope,
        });
    }
    Ok(out)
}

/// A fixed directory, for tests and for a site whose reader is swapped in by a harness.
pub struct FixedDirectory {
    directory: std::sync::Mutex<Result<RawDirectory, ModelError>>,
}

impl FixedDirectory {
    /// A reader that answers `directory` until [`FixedDirectory::set`] changes it.
    #[must_use]
    pub fn new(directory: RawDirectory) -> Self {
        Self {
            directory: std::sync::Mutex::new(Ok(directory)),
        }
    }

    /// Replaces the answer: the next read sees it.
    pub fn set(&self, directory: Result<RawDirectory, ModelError>) {
        *self
            .directory
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = directory;
    }
}

impl DirectoryReader for FixedDirectory {
    fn read<'a>(&'a self) -> BoxFuture<'a, Result<RawDirectory, ModelError>> {
        Box::pin(async move {
            self.directory
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        })
    }
}
