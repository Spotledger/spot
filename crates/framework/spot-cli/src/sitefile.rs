//! Reading and writing `site.toml` safely: exclusive create, then atomic replace under a lock.
use crate::config::{self, Site};
use crate::{Error, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

fn open(path: &Path, truncate: bool) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true);
    if truncate {
        options.create(true).truncate(true);
    } else {
        options.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

fn render(site: &Site) -> Result<String> {
    toml::to_string(site).map_err(|e| Error(e.to_string()))
}

/// Writes a new site file; fails when it already exists.
pub fn create(path: &Path, site: &Site) -> Result<()> {
    let mut file = open(path, false)?;
    file.write_all(render(site)?.as_bytes())?;
    file.sync_all()?;
    Ok(())
}

struct Lock(PathBuf);
impl Lock {
    fn acquire(dir: &Path) -> Result<Self> {
        let path = dir.join(concat!("site.toml", ".lock"));
        match open(&path, false) {
            Ok(_) => Ok(Self(path)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(Error(format!(
                "another spot command is changing this site ({} exists); if none is running, \
                 remove the file",
                path.display()
            ))),
            Err(e) => Err(e.into()),
        }
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Re-reads the validated site, lets `change` edit it, and replaces the file atomically with
/// mode 0600. `change` runs under an exclusive lock, so two commands cannot lose each other's
/// edit. Nothing is written when `change` fails.
pub fn update<R>(
    root: &Path,
    name: &str,
    change: impl FnOnce(&mut Site) -> Result<R>,
) -> Result<R> {
    let dir = root.join("sites").join(name);
    let _lock = Lock::acquire(&dir)?;
    let mut site = read(root, name)?;
    let result = change(&mut site)?;
    let target = dir.join("site.toml");
    let temporary = dir.join("site.toml.new");
    let _ = std::fs::remove_file(&temporary);
    let mut file = open(&temporary, false)?;
    file.write_all(render(&site)?.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, &target)?;
    Ok(result)
}

/// The validated site `name`.
pub fn read(root: &Path, name: &str) -> Result<Site> {
    config::load(root, false)?
        .sites
        .into_iter()
        .find(|s| s.site == name)
        .ok_or_else(|| Error(format!("no site `{name}`")))
}

/// The site to act on: the named one, or the only one configured.
pub fn pick(root: &Path, requested: Option<&str>) -> Result<Site> {
    let loaded = config::load(root, false)?;
    match requested {
        Some(name) => loaded
            .sites
            .into_iter()
            .find(|s| s.site == name)
            .ok_or_else(|| Error(format!("no site `{name}`"))),
        None => {
            let mut sites = loaded.sites;
            if sites.len() == 1 {
                Ok(sites.remove(0))
            } else {
                Err(Error(format!(
                    "{} sites are configured; name one with --site",
                    sites.len()
                )))
            }
        }
    }
}
