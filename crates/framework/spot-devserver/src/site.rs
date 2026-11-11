//! The scratch development site: create, seed, reset and snapshot/restore.
//!
//! The build/lifecycle of a real site belongs to `spot-cli` (`new_site`) and
//! `spot-pg-supervisor` (the bundled PostgreSQL child process). Those crates depend on the
//! runtime, and `spot-cli` adds the `spot dev` command **by depending on this crate**, so this
//! crate cannot depend on them without a cycle. The boundary is therefore the [`SiteControl`]
//! seam: the standalone server ships [`ScratchSiteControl`], a filesystem-only site under a
//! scratch root that is enough to watch, reset and snapshot; the CLI's `spot dev` adapter
//! implements the same trait by calling `spot_cli::new_site` and running the
//! `spot_pg_supervisor::Supervisor` for the database.
//!
//! `reset` throws away the mutable data and re-seeds from fixtures; `snapshot`/`restore` copy
//! the whole scratch site, so a developer can return to a known state without a rebuild.

use std::sync::{Mutex, MutexGuard};

use camino::{Utf8Path, Utf8PathBuf};

use crate::error::{DevError, DevResult};

/// One site operation, kept for the status page.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SiteOp {
    /// The operation (`create`, `seed`, `reset`, `snapshot`, `restore`).
    pub op: String,
    /// Whether it succeeded.
    pub ok: bool,
    /// One sentence for the developer.
    pub detail: String,
}

/// A validated scratch site layout under `root`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevLayout {
    /// The scratch root every dev site lives under.
    pub root: Utf8PathBuf,
    /// The site name (`[a-z0-9-]`, like a site directory name).
    pub name: String,
}

impl DevLayout {
    /// Validates `name` and builds the layout.
    ///
    /// # Errors
    ///
    /// [`DevError::Config`] when `name` is empty or contains anything but lower-case letters,
    /// digits and hyphens.
    pub fn new(root: impl Into<Utf8PathBuf>, name: impl Into<String>) -> DevResult<Self> {
        let name = name.into();
        if !is_safe_name(&name) {
            return Err(DevError::Config {
                message: format!(
                    "the site name `{name}` is not a site name: use lower-case letters, digits and hyphens"
                ),
            });
        }
        Ok(Self {
            root: root.into(),
            name,
        })
    }

    /// The site's directory (`<root>/sites/<name>`).
    #[must_use]
    pub fn site_dir(&self) -> Utf8PathBuf {
        self.root.join("sites").join(&self.name)
    }

    /// The mutable data directory (`<site>/data`).
    #[must_use]
    pub fn data_dir(&self) -> Utf8PathBuf {
        self.site_dir().join("data")
    }

    /// The seeded fixtures directory (`<site>/fixtures`).
    #[must_use]
    pub fn fixtures_dir(&self) -> Utf8PathBuf {
        self.site_dir().join("fixtures")
    }

    /// The snapshots root (`<root>/snapshots/<name>`).
    #[must_use]
    pub fn snapshots_root(&self) -> Utf8PathBuf {
        self.root.join("snapshots").join(&self.name)
    }
}

/// A created development site.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DevSite {
    /// The site name.
    pub name: String,
    /// The site's directory.
    pub dir: Utf8PathBuf,
    /// The mutable data directory.
    pub data_dir: Utf8PathBuf,
    /// The seeded fixtures directory.
    pub fixtures_dir: Utf8PathBuf,
}

/// Provisions, resets and snapshots a development site.
pub trait SiteControl: Send + Sync {
    /// Creates the site if it does not exist and returns its layout.
    ///
    /// # Errors
    ///
    /// [`DevError::Site`] when a directory cannot be created.
    fn create(&self) -> DevResult<DevSite>;

    /// Throws away mutable data and re-seeds the fixtures.
    ///
    /// # Errors
    ///
    /// [`DevError::Site`] when a directory cannot be replaced.
    fn reset(&self) -> DevResult<()>;

    /// Copies `fixtures` into the site's fixtures directory, returning how many files it seeded.
    ///
    /// # Errors
    ///
    /// [`DevError::Site`] when the tree cannot be copied.
    fn seed_fixtures(&self, fixtures: &Utf8Path) -> DevResult<usize>;

    /// Copies the whole site under `label` and returns the snapshot directory.
    ///
    /// # Errors
    ///
    /// [`DevError::Site`] when the label is unsafe or the copy fails.
    fn snapshot(&self, label: &str) -> DevResult<Utf8PathBuf>;

    /// Restores the site from the snapshot `label`.
    ///
    /// # Errors
    ///
    /// [`DevError::Site`] when the label is unsafe, the snapshot is missing or the copy fails.
    fn restore(&self, label: &str) -> DevResult<()>;

    /// The operations run so far, oldest first.
    fn operations(&self) -> Vec<SiteOp>;
}

/// The filesystem-only [`SiteControl`] the standalone server ships.
pub struct ScratchSiteControl {
    layout: DevLayout,
    source_fixtures: Option<Utf8PathBuf>,
    operations: Mutex<Vec<SiteOp>>,
}

impl ScratchSiteControl {
    /// A site at `layout`, re-seeded from `source_fixtures` on `reset` when set.
    #[must_use]
    pub fn new(layout: DevLayout, source_fixtures: Option<Utf8PathBuf>) -> Self {
        Self {
            layout,
            source_fixtures,
            operations: Mutex::new(Vec::new()),
        }
    }

    /// The site's layout.
    #[must_use]
    pub fn layout(&self) -> &DevLayout {
        &self.layout
    }

    fn record(&self, op: &str, ok: bool, detail: impl Into<String>) {
        self.lock().push(SiteOp {
            op: op.to_owned(),
            ok,
            detail: detail.into(),
        });
    }

    fn lock(&self) -> MutexGuard<'_, Vec<SiteOp>> {
        self.operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SiteControl for ScratchSiteControl {
    fn create(&self) -> DevResult<DevSite> {
        for dir in [
            self.layout.site_dir(),
            self.layout.data_dir(),
            self.layout.fixtures_dir(),
        ] {
            if let Err(error) = std::fs::create_dir_all(&dir) {
                return Err(DevError::Site {
                    message: format!("cannot create `{dir}`: {error}"),
                });
            }
        }
        let site = DevSite {
            name: self.layout.name.clone(),
            dir: self.layout.site_dir(),
            data_dir: self.layout.data_dir(),
            fixtures_dir: self.layout.fixtures_dir(),
        };
        self.record("create", true, format!("site `{}` ready", site.name));
        Ok(site)
    }

    fn reset(&self) -> DevResult<()> {
        let data = self.layout.data_dir();
        remove_dir(&data)?;
        std::fs::create_dir_all(&data).map_err(|error| DevError::Site {
            message: format!("cannot create `{data}`: {error}"),
        })?;
        let fixtures = self.layout.fixtures_dir();
        remove_dir(&fixtures)?;
        std::fs::create_dir_all(&fixtures).map_err(|error| DevError::Site {
            message: format!("cannot create `{fixtures}`: {error}"),
        })?;
        if let Some(source) = &self.source_fixtures {
            let seeded = self.seed_fixtures(source)?;
            self.record(
                "reset",
                true,
                format!("reset; {seeded} fixture file(s) reseeded"),
            );
        } else {
            self.record("reset", true, "reset; no fixture source configured");
        }
        Ok(())
    }

    fn seed_fixtures(&self, fixtures: &Utf8Path) -> DevResult<usize> {
        let target = self.layout.fixtures_dir();
        remove_dir(&target)?;
        std::fs::create_dir_all(&target).map_err(|error| DevError::Site {
            message: format!("cannot create `{target}`: {error}"),
        })?;
        let count = copy_tree(fixtures, &target)?;
        self.record("seed", true, format!("seeded {count} fixture file(s)"));
        Ok(count)
    }

    fn snapshot(&self, label: &str) -> DevResult<Utf8PathBuf> {
        if !is_safe_name(label) {
            return Err(DevError::Site {
                message: format!("the snapshot label `{label}` is not a safe name"),
            });
        }
        let target = self.layout.snapshots_root().join(label);
        remove_dir(&target)?;
        let count = copy_tree(&self.layout.site_dir(), &target)?;
        self.record(
            "snapshot",
            true,
            format!("snapshot `{label}` holds {count} file(s)"),
        );
        Ok(target)
    }

    fn restore(&self, label: &str) -> DevResult<()> {
        if !is_safe_name(label) {
            return Err(DevError::Site {
                message: format!("the snapshot label `{label}` is not a safe name"),
            });
        }
        let source = self.layout.snapshots_root().join(label);
        if !source.is_dir() {
            return Err(DevError::Site {
                message: format!("the snapshot `{label}` does not exist at `{source}`"),
            });
        }
        let site = self.layout.site_dir();
        remove_dir(&site)?;
        let count = copy_tree(&source, &site)?;
        self.record(
            "restore",
            true,
            format!("restored `{label}` ({count} file(s))"),
        );
        Ok(())
    }

    fn operations(&self) -> Vec<SiteOp> {
        self.lock().clone()
    }
}

/// Whether `name` is a safe site or snapshot directory name.
#[must_use]
pub fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|character| {
            character.is_ascii_lowercase() || character.is_ascii_digit() || character == '-'
        })
        && !name.starts_with('-')
        && !name.ends_with('-')
}

/// Removes `dir` when it exists.
fn remove_dir(dir: &Utf8Path) -> DevResult<()> {
    if !dir.exists() {
        return Ok(());
    }
    std::fs::remove_dir_all(dir).map_err(|error| DevError::Site {
        message: format!("cannot remove `{dir}`: {error}"),
    })
}

/// Copies the tree at `from` into `to`, creating `to`, and returns the file count.
fn copy_tree(from: &Utf8Path, to: &Utf8Path) -> DevResult<usize> {
    std::fs::create_dir_all(to).map_err(|error| DevError::Site {
        message: format!("cannot create `{to}`: {error}"),
    })?;
    let mut files = 0;
    let entries = std::fs::read_dir(from).map_err(|error| DevError::Site {
        message: format!("cannot read `{from}`: {error}"),
    })?;
    for entry in entries {
        let entry = entry.map_err(|error| DevError::Site {
            message: format!("cannot read `{from}`: {error}"),
        })?;
        let Some(path) = Utf8PathBuf::from_path_buf(entry.path()).ok() else {
            continue;
        };
        let file_type = entry.file_type().map_err(|error| DevError::Site {
            message: format!("cannot read `{path}`: {error}"),
        })?;
        if file_type.is_symlink() {
            // A link could loop or reach outside the site tree; a snapshot never follows one.
            continue;
        }
        let name = path.file_name().unwrap_or_default();
        let target = to.join(name);
        if file_type.is_dir() {
            files += copy_tree(&path, &target)?;
        } else {
            std::fs::copy(&path, &target).map_err(|error| DevError::Site {
                message: format!("cannot copy `{path}` to `{target}`: {error}"),
            })?;
            files += 1;
        }
    }
    Ok(files)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_names_are_refused() {
        assert!(is_safe_name("acme-dev"));
        assert!(!is_safe_name(""));
        assert!(!is_safe_name("Acme"));
        assert!(!is_safe_name("a/b"));
        assert!(!is_safe_name("-x"));
    }

    #[test]
    fn layout_directories_are_under_the_root() {
        let layout = DevLayout::new("/scratch/dev", "acme").unwrap();
        assert_eq!(layout.site_dir(), Utf8Path::new("/scratch/dev/sites/acme"));
        assert_eq!(
            layout.snapshots_root(),
            Utf8Path::new("/scratch/dev/snapshots/acme")
        );
    }
}
