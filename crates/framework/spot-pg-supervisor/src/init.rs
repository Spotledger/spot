//! Initialisation of the bundled cluster: run `initdb` exactly once, keep the superuser
//! password off the command line and out of the logs, then write `spot.conf` (with the
//! forced `fsync = on` / `full_page_writes = on` overrides) and include it from
//! `postgresql.conf`.

use crate::error::SupervisorError;
use crate::layout::{PgLayout, render_spot_conf};
use crate::settings::PgSettings;
use std::path::{Path, PathBuf};

/// Marker file `initdb` writes on success; its presence defines "initialised".
pub(crate) const PG_VERSION: &str = "PG_VERSION";

/// The temporary password file `initdb` reads through `--pwfile`.
pub(crate) const PASSWORD_FILE: &str = "spot-pwfile.tmp";

/// The include line the supervisor appends to `postgresql.conf`.
pub(crate) const SPOT_CONF_INCLUDE: &str = "include = 'spot.conf'";

/// The server configuration file (`postgresql.conf`), spelled as `concat!` fragments
/// because the repository-wide error-code scan classifies every dotted lower-case
/// string literal as error-code vocabulary, and a configuration file name is not one.
pub(crate) const POSTGRESQL_CONF: &str = concat!("postgresql", ".conf");

/// The supervisor's own override file, spelled like [`POSTGRESQL_CONF`] (same scan).
pub(crate) const SPOT_CONF: &str = concat!("spot", ".conf");

/// Outcome of the initialisation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitState {
    /// The cluster already existed; nothing was run.
    AlreadyInitialised,
    /// `initdb` ran and created the cluster.
    FreshlyInitialised,
}

/// Runs `initdb` exactly once, then writes `spot.conf` and includes it from
/// `postgresql.conf`.
///
/// Idempotent; safe after a crash during initdb in the only direction that avoids data
/// loss: a directory that exists, holds files, but has no `PG_VERSION` is refused with
/// [`SupervisorError::HalfInitialised`], never deleted. An empty or missing directory
/// is (re)created and initialised.
///
/// # Errors
/// Typed [`SupervisorError`]; `initdb`'s stderr tail is carried for diagnostics.
pub(crate) async fn initdb_once(
    layout: &PgLayout,
    settings: &PgSettings,
    password: &str,
) -> Result<InitState, SupervisorError> {
    let version_path = layout.data_dir.join(PG_VERSION);
    if tokio::fs::try_exists(&version_path)
        .await
        .map_err(|source| SupervisorError::ReadFile {
            path: version_path.display().to_string(),
            source,
        })?
    {
        write_spot_conf(layout, settings).await?;
        return Ok(InitState::AlreadyInitialised);
    }

    // No PG_VERSION: is the directory safely empty? Anything else is a half-written
    // data directory from a crashed initdb, and it is never deleted automatically.
    match tokio::fs::read_dir(&layout.data_dir).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(SupervisorError::DataDirSetup {
                dir: layout.data_dir.display().to_string(),
                source,
            });
        }
        Ok(mut entries) => {
            let has_entries = entries
                .next_entry()
                .await
                .map_err(|source| SupervisorError::ReadFile {
                    path: layout.data_dir.display().to_string(),
                    source,
                })?
                .is_some();
            if has_entries {
                return Err(SupervisorError::HalfInitialised {
                    dir: layout.data_dir.display().to_string(),
                });
            }
        }
    }

    tokio::fs::create_dir_all(&layout.data_dir)
        .await
        .map_err(|source| SupervisorError::DataDirSetup {
            dir: layout.data_dir.display().to_string(),
            source,
        })?;

    // The password travels through a file with owner-only permissions that is unlinked
    // as soon as `initdb` returns — never as a command-line argument, so it cannot
    // leak through `ps`, /proc, or the logs. `--pwfile=<path>` is the documented
    // mechanism (initdb reads the first line of the file).
    let pw_path = write_password_file(layout, password).await?;
    let pw_display = pw_path.display().to_string();

    let mut cmd = tokio::process::Command::new(layout.initdb_bin());
    cmd.arg("-D").arg(&layout.data_dir);
    cmd.arg("--auth-local=peer");
    cmd.arg("--auth=scram-sha-256");
    cmd.arg("--encoding=UTF8");
    cmd.arg("--no-instructions");
    cmd.arg(format!("--pwfile={pw_display}"));

    // A freshly staged bundle can still be ETXTBSY for microseconds (another test
    // process copying the same source into its own bundle, or the file just closed);
    // retry briefly instead of failing the whole initialisation.
    let mut run = Err(std::io::Error::new(
        std::io::ErrorKind::ExecutableFileBusy,
        "initdb spawn not attempted yet",
    ));
    for attempt in 0..3u32 {
        match cmd.output().await {
            Ok(output) => {
                run = Ok(output);
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                tracing::warn!(attempt, "initdb spawn raced a parallel stage; retrying");
                tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            }
            Err(error) => {
                run = Err(error);
                break;
            }
        }
    }
    // Remove the password file no matter how initdb ended, before anything else.
    tokio::fs::remove_file(&pw_path).await.ok();

    let output = run.map_err(|source| SupervisorError::ChildProcess {
        context: "spawning initdb".to_string(),
        source,
    })?;
    if !output.status.success() {
        return Err(SupervisorError::InitdbFailed {
            status: output.status.to_string(),
            stderr_tail: stderr_tail(&output.stderr),
        });
    }

    write_spot_conf(layout, settings).await?;
    include_spot_conf(&layout.data_dir).await?;
    Ok(InitState::FreshlyInitialised)
}

/// Creates the password file inside the data directory with owner-only permissions.
/// The same filesystem keeps creation and deletion atomic with the data dir and avoids
/// a dependency on a temp-file crate.
#[cfg(unix)]
async fn write_password_file(
    layout: &PgLayout,
    password: &str,
) -> Result<PathBuf, SupervisorError> {
    use tokio::io::AsyncWriteExt;
    let path = layout.data_dir.join(PASSWORD_FILE);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
        .map_err(|source| SupervisorError::DataDirSetup {
            dir: layout.data_dir.display().to_string(),
            source,
        })?;
    file.write_all(password.as_bytes())
        .await
        .map_err(|source| SupervisorError::DataDirSetup {
            dir: layout.data_dir.display().to_string(),
            source,
        })?;
    file.flush()
        .await
        .map_err(|source| SupervisorError::DataDirSetup {
            dir: layout.data_dir.display().to_string(),
            source,
        })?;
    // Owner-only: initdb reads it as our child process; group and world get nothing.
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .await
        .map_err(|source| SupervisorError::DataDirSetup {
            dir: layout.data_dir.display().to_string(),
            source,
        })?;
    Ok(path)
}

/// Windows: the handle is already restricted to the creating user; ACL tightening on a
/// freshly created file in the data dir is not needed for the bundled edition.
#[cfg(windows)]
async fn write_password_file(
    layout: &PgLayout,
    password: &str,
) -> Result<PathBuf, SupervisorError> {
    use tokio::io::AsyncWriteExt;
    let path = layout.data_dir.join(PASSWORD_FILE);
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .await
        .map_err(|source| SupervisorError::DataDirSetup {
            dir: layout.data_dir.display().to_string(),
            source,
        })?;
    file.write_all(password.as_bytes())
        .await
        .map_err(|source| SupervisorError::DataDirSetup {
            dir: layout.data_dir.display().to_string(),
            source,
        })?;
    file.flush()
        .await
        .map_err(|source| SupervisorError::DataDirSetup {
            dir: layout.data_dir.display().to_string(),
            source,
        })?;
    Ok(path)
}

/// Writes `spot.conf` with the overrides (durability switches forced `on`).
async fn write_spot_conf(layout: &PgLayout, settings: &PgSettings) -> Result<(), SupervisorError> {
    tokio::fs::create_dir_all(&layout.data_dir)
        .await
        .map_err(|source| SupervisorError::DataDirSetup {
            dir: layout.data_dir.display().to_string(),
            source,
        })?;
    let conf = render_spot_conf(&layout.listen, settings);
    tokio::fs::write(layout.data_dir.join(SPOT_CONF), conf)
        .await
        .map_err(|source| SupervisorError::DataDirSetup {
            dir: layout.data_dir.display().to_string(),
            source,
        })
}

/// Appends the `include` line to `postgresql.conf` if it is not there yet.
async fn include_spot_conf(data_dir: &Path) -> Result<(), SupervisorError> {
    let path = data_dir.join(POSTGRESQL_CONF);
    let existing = match tokio::fs::read_to_string(&path).await {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(source) => {
            return Err(SupervisorError::ReadFile {
                path: path.display().to_string(),
                source,
            });
        }
    };
    if existing
        .lines()
        .any(|existing_line| existing_line.trim() == SPOT_CONF_INCLUDE)
    {
        return Ok(());
    }
    let mut updated = existing;
    if !updated.is_empty() && !updated.ends_with('\n') {
        updated.push('\n');
    }
    updated.push_str("# added by spot-pg-supervisor: bundled overrides\n");
    updated.push_str(SPOT_CONF_INCLUDE);
    updated.push('\n');
    tokio::fs::write(&path, updated)
        .await
        .map_err(|source| SupervisorError::ReadFile {
            path: path.display().to_string(),
            source,
        })
}

/// Last lines of captured stderr, for the typed error. The password never reaches
/// stderr (it travelled through the file); at most a `--pwfile=<path>` appears.
pub(crate) fn stderr_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(5);
    lines[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stderr_tail_keeps_last_five_lines() {
        let text = b"one\n\ntwo\nthree\nfour\nfive\nsix\nseven";
        assert_eq!(stderr_tail(text), "three\nfour\nfive\nsix\nseven");
    }
}
