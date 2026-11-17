//! Who runs the command, the confirm token, the restore lock and the debug-build fault seam.

use serde::{Deserialize, Serialize};
use spot_auth::{OpaqueToken, TokenDigest};
use std::path::{Path, PathBuf};

use super::{TOKEN_TTL_SECS, private_file, state_dir};

/// The real user id of this process (never `USER`, which the caller controls).
#[cfg(unix)]
pub(super) fn os_uid() -> Option<u32> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find_map(|l| l.strip_prefix("Uid:"))?;
    line.split_whitespace().next()?.parse().ok()
}

/// Who runs the command, as a stable text: `uid:N` on Unix, the account name elsewhere.
pub(super) fn os_user() -> String {
    #[cfg(unix)]
    {
        os_uid().map_or_else(|| "unknown".to_owned(), |uid| format!("uid:{uid}"))
    }
    #[cfg(not(unix))]
    {
        std::env::var("USERNAME")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "unknown".to_owned())
    }
}

/// Refuses a restore run as root when the host root belongs to someone else.
pub(super) fn root_check(root: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let Some(uid) = os_uid() else {
            return Err(
                "restore.user_unknown: the invoking OS user cannot be determined".to_owned(),
            );
        };
        let owner = std::fs::metadata(root)
            .map(|m| m.uid())
            .map_err(|_| "restore.user_unknown: the host root cannot be read".to_owned())?;
        if uid == 0 && owner != 0 {
            return Err("restore.root_refused: run restore as the service user that owns the host root, not as root".to_owned());
        }
    }
    let _ = root;
    Ok(())
}

// Confirm tokens

#[derive(Serialize, Deserialize)]
pub(super) struct TokenFile {
    digest: String,
    backup: String,
    target: String,
    os_user: String,
    expires_at: i64,
}

pub(super) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(super) fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() & 1 == 1 || !text.is_ascii() {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

pub(super) fn token_path(root: &Path, site: &str) -> std::io::Result<PathBuf> {
    Ok(state_dir(root, site)?.join("restore-confirm.toml"))
}

/// Issues a token, replacing any earlier one of the site. Only its digest is stored.
pub(super) fn issue_token(
    root: &Path,
    site: &str,
    backup: &str,
    target: &str,
    now: i64,
) -> std::io::Result<String> {
    let token = OpaqueToken::generate();
    let file = TokenFile {
        digest: hex(token.digest().as_bytes()),
        backup: backup.to_owned(),
        target: target.to_owned(),
        os_user: os_user(),
        expires_at: now.saturating_add(TOKEN_TTL_SECS),
    };
    let path = token_path(root, site)?;
    let text = toml::to_string(&file).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("new");
    let _ = std::fs::remove_file(&tmp);
    let mut out = private_file(&tmp)?;
    std::io::Write::write_all(&mut out, text.as_bytes())?;
    out.sync_all()?;
    std::fs::rename(&tmp, &path)?;
    Ok(token.as_str().to_owned())
}

/// Checks `presented` against the stored token without using it up.
pub(super) fn check_token(
    path: &Path,
    presented: &str,
    backup: &str,
    target: &str,
    now: i64,
) -> Result<(), String> {
    let invalid = |why: &str| {
        format!("restore.confirm_invalid: {why}; run `spot restore preflight` for a new token")
    };
    let text = std::fs::read_to_string(path)
        .map_err(|_| invalid("there is no confirm token for this site"))?;
    let file: TokenFile =
        toml::from_str(&text).map_err(|_| invalid("the stored confirm token is unreadable"))?;
    let parsed = OpaqueToken::parse(presented).map_err(|_| invalid("the token is malformed"))?;
    let stored = unhex(&file.digest).and_then(|b| TokenDigest::from_bytes(&b).ok());
    if !stored.is_some_and(|d| d.matches_token(&parsed)) {
        return Err(invalid("the token does not match"));
    }
    if now > file.expires_at {
        return Err(invalid("the token has expired"));
    }
    if file.backup != backup || file.target != target {
        return Err(invalid("the token was issued for another backup or target"));
    }
    if file.os_user != os_user() {
        return Err(invalid("the token was issued to another OS user"));
    }
    Ok(())
}

/// Uses the token up: the rename is atomic, so of two racing applies only one wins.
pub(super) fn spend_token(path: &Path) -> Result<(), String> {
    let used = path.with_extension(format!("used-{}", std::process::id()));
    std::fs::rename(path, &used)
        .map_err(|_| "restore.confirm_invalid: the token was already used".to_owned())?;
    let _ = std::fs::remove_file(&used);
    Ok(())
}

/// Test seam: in a debug build the process dies at the named point, as a power cut would.
pub(super) fn fault(point: &str) {
    let armed =
        cfg!(debug_assertions) && std::env::var("SPOT_RESTORE_FAULT").is_ok_and(|v| v == point);
    if armed {
        std::process::abort();
    }
}

/// Records that the host root's process holds the restore, so a second one is refused and a dead
/// one can be recovered from.
pub(super) struct StateLock(PathBuf);

pub(super) enum LockState {
    Busy,
    Stale,
    Failed,
}

impl StateLock {
    pub(super) fn acquire(dir: &Path) -> Result<StateLock, LockState> {
        let path = dir.join("restore.lock");
        match private_file(&path) {
            Ok(mut file) => {
                let _ =
                    std::io::Write::write_all(&mut file, std::process::id().to_string().as_bytes());
                Ok(StateLock(path))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let pid = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|t| t.trim().parse::<u32>().ok());
                let alive = pid
                    .is_none_or(|p| cfg!(not(unix)) || Path::new(&format!("/proc/{p}")).exists());
                Err(if alive {
                    LockState::Busy
                } else {
                    LockState::Stale
                })
            }
            Err(_) => Err(LockState::Failed),
        }
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
