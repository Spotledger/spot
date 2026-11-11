//! Rotation: the key file lock, adding and retiring a version, and the rewrap sweep.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use spot_app_api::context::{Actor, Invocation};
use spot_core::{Clock, SiteId};
use spot_document::engine::{Engine, V7Ids};
use spot_document::secrets::{RewrapBatch, SecretCaller};
use spot_meta::{EffectiveMeta, FieldType};
use spot_postgres::PgBackend;

use super::scan::name_of;
use super::{SecretsHost, cause, codes, refuse};
use crate::{Error, Result, config};

/// The exclusive lock of one key file: a rotation holds it, so two operators (or two processes)
/// cannot each add "the next version" and strand values under the loser's key. The lock is a
/// file next to the key file, created exclusively and removed on drop. A crash leaves it behind;
/// the refusal says so and names it.
pub struct RotationLock {
    path: PathBuf,
}

impl RotationLock {
    /// Takes the lock for the key file at `kek`.
    ///
    /// # Errors
    ///
    /// `secrets_host.rotation_running` when the lock exists.
    pub fn take(kek: &Path) -> Result<RotationLock> {
        let mut path = kek.as_os_str().to_owned();
        path.push(".lock");
        let path = PathBuf::from(path);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(_) => Ok(RotationLock { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(refuse(
                codes::ROTATION_RUNNING,
                format!(
                    "another rotation holds {}; if none is running, remove that file",
                    path.display()
                ),
            )),
            Err(e) => Err(refuse(
                codes::ROTATION_RUNNING,
                format!("cannot take {} ({})", path.display(), e.kind()),
            )),
        }
    }
}

impl Drop for RotationLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Adds the next key version and makes it current. Older versions stay until retired.
///
/// # Errors
///
/// A provider failure, by code.
pub async fn add_version(host: &SecretsHost) -> Result<u32> {
    host.keyring.rotate().await.map_err(|e| {
        refuse(
            codes::KEK_FILE_UNUSABLE,
            format!("the key file {} was not changed ({})", host.path.display(), cause(&e)),
        )
    })
}

/// Destroys the key material of `version`, after checking that no value is under it.
///
/// # Errors
///
/// `secrets_host.retire_refused`.
pub async fn retire_version(host: &SecretsHost, version: u32, rows_on_it: u64) -> Result<()> {
    if rows_on_it > 0 {
        return Err(refuse(
            codes::RETIRE_REFUSED,
            format!("{rows_on_it} stored value(s) are still under key version {version}"),
        ));
    }
    host.keyring.retire(version, false).await.map_err(|e| {
        refuse(
            codes::RETIRE_REFUSED,
            format!("key version {version} was not retired ({})", cause(&e)),
        )
    })
}

/// What a rewrap sweep did. Counts only.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SweepTotals {
    /// Records examined.
    pub examined: u64,
    /// Sealed values moved onto the current key.
    pub rewrapped: u64,
    /// Blind indexes recomputed.
    pub reindexed: u64,
    /// Values that could not be moved (each is in the secret audit journal).
    pub failed: u64,
    /// Batches run.
    pub batches: u64,
}

/// An engine over the site's database with the host's vault, for the commands: the same document
/// engine the server runs, without its authorizer and participants (a rewrap is the vault's
/// operation, journalled by the engine in the batch's own transaction).
///
/// # Errors
///
/// Metadata, catalogue or database failures.
pub async fn open_engine(
    root: &Path,
    loaded: &config::Loaded,
    site: &config::Site,
    host: &SecretsHost,
    clock: Arc<dyn Clock>,
) -> Result<Engine<PgBackend>> {
    let meta = crate::services::metadata::effective(root, loaded, site)?;
    let backend = crate::schema::backend(loaded, site).await?;
    let engine = Engine::new(
        backend,
        meta,
        Arc::new(crate::platform_registry()?),
        clock,
        Arc::new(V7Ids),
    )
    .map_err(|e| Error(format!("the document engine did not start ({})", e.code)))?;
    Ok(engine.with_secrets(host.vault()))
}

/// One pass over every top-level DocType with a `secret` field: batches of `batch` records until
/// each DocType's report has no `next`. `progress` hears the DocType and the totals so far after
/// every batch.
///
/// # Errors
///
/// `secrets_host.rewrap_failed` when the engine refuses a batch (the code of its refusal is in
/// the text, never a value).
pub async fn sweep_once(
    engine: &Engine<PgBackend>,
    meta: &EffectiveMeta,
    site: &str,
    clock: &dyn Clock,
    batch: u32,
    progress: &mut (dyn FnMut(&str, &SweepTotals) + Send),
) -> Result<SweepTotals> {
    let invocation = Invocation {
        site: SiteId::new(site).map_err(|_| Error(format!("`{site}` is not a valid site name")))?,
        actor: Actor::System {
            component: name_of("secrets")?,
        },
        command_id: None,
        language: None,
        now: clock.now_utc(),
    };
    // The operator tool acts with the bootstrap's capability: there is no separate caller kind
    // for it (see the report's open questions).
    let access = engine.secret_access(SecretCaller::Seed, &invocation);
    let mut totals = SweepTotals::default();
    for def in meta.definitions() {
        if def.is_child || !def.fields.iter().any(|f| f.ty == FieldType::Secret) {
            continue;
        }
        let mut after = None;
        loop {
            let report = engine
                .rewrap_secrets(&access, &def.name, RewrapBatch { size: batch, after })
                .await
                .map_err(|e| {
                    refuse(
                        codes::REWRAP_FAILED,
                        format!("the rewrap of `{}` was refused ({})", def.name, e.code),
                    )
                })?;
            totals.batches += 1;
            totals.examined += u64::from(report.examined);
            totals.rewrapped += u64::from(report.rewrapped);
            totals.reindexed += u64::from(report.reindexed);
            totals.failed += u64::from(report.failed);
            progress(&def.name, &totals);
            match report.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
    }
    Ok(totals)
}
