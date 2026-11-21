//! The sandbox registry and the destroyer (WP-ALM-05.T6).
//!
//! A host keeps one record per sandbox at `<root>/sites/<name>/sandbox.toml`. That file is the
//! registry: its record, never a name pattern, says what is a sandbox, which site it was cloned
//! from, which database it lives in and when it expires. `spot sandbox` (the command) and the
//! hosted reaper (WP-ALM-05.T5) both read and destroy through this module, so they cannot drift
//! apart.
//!
//! - [`read_record`] reads one record strictly; [`list_records`] reads every record of a root
//!   strictly (the command: a record that does not parse is an error the operator sees).
//! - [`list_entries`] reads every record of a root leniently into a [`Listing`] (the reaper: a
//!   record it cannot read is left alone, and a `keep` flag is honoured).
//! - [`destroy_sandbox`] drops a sandbox's database through a [`DatabaseAdmin`] port, then
//!   removes its folder (and with it the record). It is **idempotent**: a half-destroyed
//!   sandbox is finished, not an error. It refuses anything that is not a plain sandbox
//!   database (`sandbox_` prefix, plain lower-case name) or a site name that is not a plain
//!   folder name.
//!
//! The database server is a port ([`DatabaseAdmin`]) so this crate stays free of a database
//! driver: the host supplies the PostgreSQL implementation. Nothing here reads a clock or a
//! random source, and no error carries a path, a database name or a connection string: the
//! destroyer's errors are fixed reason codes.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;

use serde::{Deserialize, Serialize};

/// The file that marks a site folder as a sandbox.
pub const RECORD_FILE: &str = "sandbox.toml";

/// A creation that never finished is reaped after this many seconds.
pub const STALE_CREATING: i64 = 3_600;

/// The prefix every sandbox database carries.
pub const DATABASE_PREFIX: &str = "sandbox_";

/// A sandbox's record, as `spot sandbox create` writes it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The site the sandbox was cloned from.
    pub source: String,
    /// The sandbox's database.
    pub database: String,
    /// When it was created, Unix seconds.
    pub created_at: i64,
    /// When it expires, Unix seconds.
    pub expires_at: i64,
    /// `creating` or `active`.
    pub state: String,
    /// The banner the interface shows on every page of a sandbox.
    pub banner: bool,
}

/// Why a record could not be read. Carries no path and no content.
#[derive(Debug)]
pub enum ReadError {
    /// The file exists and does not parse.
    Unparsable,
    /// The file exists and could not be read.
    Io(std::io::Error),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unparsable => f.write_str("a sandbox record does not parse"),
            Self::Io(e) => write!(f, "a sandbox record could not be read: {}", e.kind()),
        }
    }
}

impl std::error::Error for ReadError {}

/// Reads a sandbox record; `None` when the folder holds none.
///
/// # Errors
/// [`ReadError::Unparsable`] when the file exists and does not parse, [`ReadError::Io`] when
/// it cannot be read.
pub fn read_record(dir: &Path) -> Result<Option<Record>, ReadError> {
    match std::fs::read_to_string(dir.join(RECORD_FILE)) {
        Ok(text) => toml::from_str(&text)
            .map(Some)
            .map_err(|_| ReadError::Unparsable),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(ReadError::Io(e)),
    }
}

/// Every sandbox record under `<root>/sites`, strictly read, sorted by site name. A root with
/// no `sites` folder holds none.
///
/// # Errors
/// The first [`ReadError`] met: a record that does not parse is never skipped here.
pub fn list_records(root: &Path) -> Result<Vec<(String, PathBuf, Record)>, ReadError> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(root.join("sites")) else {
        return Ok(found);
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if let (Some(name), Some(record)) = (
            entry.file_name().to_str().map(str::to_owned),
            read_record(&dir)?,
        ) {
            found.push((name, dir, record));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// One sandbox of the registry, as the reaper sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The sandbox's site name.
    pub name: String,
    /// The site it was cloned from.
    pub source: String,
    /// Its database.
    pub database: String,
    /// When it was created, Unix seconds.
    pub created_at: i64,
    /// When it expires, Unix seconds; 0 or less means "no expiry".
    pub expires_at: i64,
    /// `creating` or `active`.
    pub state: String,
    /// Marked to be kept: never reaped.
    pub keep: bool,
}

impl Entry {
    /// The entry of the sandbox named `name` with record `record` (never marked `keep`: the
    /// strict record has no such flag).
    #[must_use]
    pub fn from_record(name: &str, record: &Record) -> Self {
        Self {
            name: name.to_owned(),
            source: record.source.clone(),
            database: record.database.clone(),
            created_at: record.created_at,
            expires_at: record.expires_at,
            state: record.state.clone(),
            keep: false,
        }
    }
}

/// What the registry holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// Every sandbox of the root, of every source (the safety rules look at all of them).
    pub entries: Vec<Entry>,
    /// The databases of the root's real (non-sandbox) sites: never destroyed.
    pub real_databases: BTreeSet<String>,
}

/// A record as read leniently: a record the reaper cannot read is left alone, and a `keep`
/// flag (written by hand or by a later version) is honoured.
#[derive(Deserialize)]
struct LenientRecord {
    source: String,
    database: String,
    created_at: i64,
    #[serde(default)]
    expires_at: i64,
    #[serde(default)]
    state: String,
    #[serde(default)]
    keep: bool,
}

/// Lists the sandboxes under `<root>/sites/*/sandbox.toml` leniently, sorted by name: a record
/// that does not parse or cannot be read is not a registry row and is left alone.
/// `real_databases` are the databases of the real sites of the configuration.
#[must_use]
pub fn list_entries(root: &Path, real_databases: &BTreeSet<String>) -> Listing {
    let mut entries = Vec::new();
    if let Ok(dirs) = std::fs::read_dir(root.join("sites")) {
        for dir in dirs.flatten() {
            let Some(name) = dir.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Ok(text) = std::fs::read_to_string(dir.path().join(RECORD_FILE)) else {
                continue;
            };
            let Ok(record) = toml::from_str::<LenientRecord>(&text) else {
                continue;
            };
            entries.push(Entry {
                name,
                source: record.source,
                database: record.database,
                created_at: record.created_at,
                expires_at: record.expires_at,
                state: record.state,
                keep: record.keep,
            });
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Listing {
        entries,
        real_databases: real_databases.clone(),
    }
}

/// What destroying a sandbox found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destroyed {
    /// The database or the folder was there and is now gone.
    Gone,
    /// Neither was there: someone finished the job already.
    AlreadyGone,
}

/// The boxed future of a [`DatabaseAdmin`] call. An error is a reason code (`[a-z_]+`).
pub type AdminFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, &'static str>> + Send + 'a>>;

/// The database server's maintenance side: the one thing the destroyer needs from it.
pub trait DatabaseAdmin: Send + Sync {
    /// Drops `database` (ending its connections first) when it exists; `true` when it existed.
    /// `database` has already been checked to be a plain sandbox database name. An error is a
    /// reason code such as `database_unreachable` or `drop_failed`.
    fn drop_database_if_exists<'a>(&'a self, database: &'a str) -> AdminFuture<'a, bool>;
}

/// Whether `name` is a plain storage name: lower-case letters, digits and underscores, 1 to 63
/// bytes. Such a name needs no quoting beyond the double quotes.
#[must_use]
pub fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 63
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Whether `name` is a valid site name, so a plain folder name: a lower-case DNS host name of 1
/// to 253 bytes, dot-separated labels of 1 to 63 bytes of `a-z`, `0-9` and `-`, none starting
/// or ending with `-`.
///
/// This mirrors `spot_core::SiteId::new` rule for rule (this crate does not depend on
/// `spot-core`); a test in `spot-cli` (`tests/sandbox_shared.rs`) keeps the two in step.
#[must_use]
pub fn is_plain_site_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !label.starts_with('-')
                && !label.ends_with('-')
        })
}

/// Destroys one sandbox, idempotently: drops its database through `admin`, then removes its
/// site folder under `<root>/sites` (the record is inside, so the registry row goes last).
///
/// # Errors
/// A reason code: `invalid_database` (not a plain name), `not_a_sandbox_database` (no
/// `sandbox_` prefix), `invalid_site` (not a plain folder name), the code `admin` returned, or
/// `folder_not_removed`. A code never holds a name, a path or a connection string.
pub async fn destroy_sandbox(
    root: &Path,
    admin: &dyn DatabaseAdmin,
    entry: &Entry,
) -> Result<Destroyed, &'static str> {
    // The record names the database; refuse anything that is not a plain sandbox database and
    // any site name that is not a plain folder name.
    if !is_plain_name(&entry.database) {
        return Err("invalid_database");
    }
    if !entry.database.starts_with(DATABASE_PREFIX) {
        return Err("not_a_sandbox_database");
    }
    if !is_plain_site_name(&entry.name) {
        return Err("invalid_site");
    }
    let existed = admin.drop_database_if_exists(&entry.database).await?;
    let dir = root.join("sites").join(&entry.name);
    let folder = dir.exists();
    if folder {
        std::fs::remove_dir_all(&dir).map_err(|_| "folder_not_removed")?;
    }
    Ok(if existed || folder {
        Destroyed::Gone
    } else {
        Destroyed::AlreadyGone
    })
}
