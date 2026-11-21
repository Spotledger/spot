//! The promotion intent log: one JSON file per promotion under `promotions/` in the host root.
//!
//! `spot-promotion`'s `Promotion` has no durable form yet, so the CLI keeps what the operator
//! decided (the request, the approvals with their identities and times) and **replays it through
//! the service when `apply` runs**: request, plan, each recorded approval, rehearsal on a clone,
//! apply, verify. The approvals and the audit records are written to the sites' real stores at
//! that point, once. The file holds names, roles, times and digests; never a key or a payload.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::super::bundle::write_private;

/// The directory under the host root.
pub const DIR: &str = "promotions";

/// One recorded approval: who, in which roles, acting as which, why, when (Unix microseconds).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalIntent {
    /// The approver's subject.
    pub approver: String,
    /// The roles the approver holds.
    pub roles: Vec<String>,
    /// The role acted in.
    pub role: String,
    /// The reason.
    pub comment: String,
    /// When.
    pub at: i64,
}

/// A promotion as the operator left it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    /// The promotion id.
    pub id: String,
    /// The source site.
    pub from_site: String,
    /// The target site.
    pub to_site: String,
    /// Who asked.
    pub requester: String,
    /// When (Unix microseconds).
    pub requested_at: i64,
    /// The bundle's digest, hex.
    pub bundle_digest: String,
    /// The approvals recorded so far, in order.
    pub approvals: Vec<ApprovalIntent>,
    /// `requested`, `applied`, `failed` or `aborted`.
    pub state: String,
    /// A line for the operator: why it is in this state.
    pub note: String,
}

/// Whether `id` can be a file stem and an audit run id.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn dir(root: &Path) -> PathBuf {
    root.join(DIR)
}

/// Where the record of `id` lives.
#[must_use]
pub fn record_path(root: &Path, id: &str) -> PathBuf {
    dir(root).join(format!("{id}.json"))
}

/// Where the kept hand-over file of `id` lives.
#[must_use]
pub fn handover_path(root: &Path, id: &str) -> PathBuf {
    dir(root).join(format!("{id}.handover"))
}

/// Reads a record; `None` when there is none.
///
/// # Errors
/// [`spot_cli::Error`] for an invalid id, an unreadable file or one that does not parse.
pub fn read(root: &Path, id: &str) -> spot_cli::Result<Option<Record>> {
    if !valid_id(id) {
        return Err(spot_cli::Error("the promotion id is not valid".to_owned()));
    }
    match std::fs::read(record_path(root, id)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|_| spot_cli::Error("the promotion record does not parse".to_owned())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Writes a record (`create` refuses to replace one).
///
/// # Errors
/// [`spot_cli::Error`] when the file cannot be written.
pub fn write(root: &Path, record: &Record, create: bool) -> spot_cli::Result<()> {
    std::fs::create_dir_all(dir(root))?;
    let bytes = serde_json::to_vec_pretty(record)
        .map_err(|_| spot_cli::Error("the promotion record cannot be encoded".to_owned()))?;
    write_private(&record_path(root, &record.id), &bytes, !create)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_file_stems() {
        assert!(valid_id("promo-abc_1"));
        assert!(!valid_id(""));
        assert!(!valid_id("../x"));
        assert!(!valid_id("a b"));
    }

    #[test]
    fn a_record_round_trips() {
        let dir = std::env::temp_dir().join(format!("spot-promote-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let record = Record {
            id: "p1".into(),
            from_site: "a".into(),
            to_site: "b".into(),
            requester: "alice".into(),
            requested_at: 5,
            bundle_digest: "00".into(),
            approvals: vec![ApprovalIntent {
                approver: "bob".into(),
                roles: vec!["owner".into()],
                role: "owner".into(),
                comment: "ok".into(),
                at: 6,
            }],
            state: "requested".into(),
            note: String::new(),
        };
        write(&dir, &record, true).unwrap();
        assert!(
            write(&dir, &record, true).is_err(),
            "never replaced silently"
        );
        assert_eq!(read(&dir, "p1").unwrap(), Some(record));
        assert_eq!(read(&dir, "nope").unwrap(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
