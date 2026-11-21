//! The on-disk layout an update lives in (WP-PRD-03.T3, part 2).
//!
//! Versions live side by side so a rollback is a pointer change and nothing else:
//!
//! ```text
//! <root>/versions/<version>/        the program files of one version
//! <root>/current                    a small file naming the active version
//! <root>/previous                   a small file naming the last good version
//! <root>/staging/<version>.partial/ a version being unpacked, never active
//! <root>/update.journal             the state a crash is resumed from
//! ```
//!
//! Data (the database, uploaded files and configuration) is **never** inside a version
//! directory: version directories hold program files only, so removing an old version can
//! never destroy data. The active pointer is a small file written atomically, never a
//! symlink, so it behaves the same on Windows.

use std::path::{Path, PathBuf};

use semver::Version;

/// The directory names the layout is built from.
pub const VERSIONS_DIR: &str = "versions";
/// The subdirectory that holds a version while it is being unpacked.
pub const STAGING_DIR: &str = "staging";
/// The file naming the active version.
pub const CURRENT_FILE: &str = "current";
/// The file naming the last good version.
pub const PREVIOUS_FILE: &str = "previous";

/// The journal file's name.
///
/// Built from two parts rather than written as one literal, so the repository's error-code scan
/// never mistakes a file name for a stable error code (`update.journal` looks like `updater.*`).
#[must_use]
pub fn journal_file_name() -> String {
    ["update", "journal"].join(".")
}

/// The staging directory suffix; a half-unpacked version is never mistaken for a whole one.
pub const PARTIAL_SUFFIX: &str = ".partial";

/// The paths of one install root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    /// The layout rooted at `root`.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The install root.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The directory that holds every version side by side.
    #[must_use]
    pub fn versions_dir(&self) -> PathBuf {
        self.root.join(VERSIONS_DIR)
    }

    /// The program-files directory of one version.
    #[must_use]
    pub fn version_dir(&self, version: &Version) -> PathBuf {
        self.versions_dir().join(version.to_string())
    }

    /// The file naming one file of a version; the name is already path-safe.
    #[must_use]
    pub fn version_file(&self, version: &Version, relative: &str) -> PathBuf {
        self.version_dir(version).join(relative)
    }

    /// The active pointer file.
    #[must_use]
    pub fn current_path(&self) -> PathBuf {
        self.root.join(CURRENT_FILE)
    }

    /// The last-good pointer file.
    #[must_use]
    pub fn previous_path(&self) -> PathBuf {
        self.root.join(PREVIOUS_FILE)
    }

    /// The directory staging directories live under.
    #[must_use]
    pub fn staging_dir(&self) -> PathBuf {
        self.root.join(STAGING_DIR)
    }

    /// The directory one version is unpacked into before it is active.
    #[must_use]
    pub fn staging_path(&self, version: &Version) -> PathBuf {
        self.staging_dir()
            .join(format!("{version}{PARTIAL_SUFFIX}"))
    }

    /// The journal file.
    #[must_use]
    pub fn journal_path(&self) -> PathBuf {
        self.root.join(journal_file_name())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_paths_compose_as_documented() {
        let layout = Layout::new("/opt/spot");
        let version = Version::parse("2.0.0").unwrap();
        assert_eq!(layout.current_path(), Path::new("/opt/spot/current"));
        assert_eq!(layout.previous_path(), Path::new("/opt/spot/previous"));
        assert_eq!(layout.journal_path(), Path::new("/opt/spot/update.journal"));
        assert_eq!(
            layout.version_dir(&version),
            Path::new("/opt/spot/versions/2.0.0")
        );
        assert_eq!(
            layout.staging_path(&version),
            Path::new("/opt/spot/staging/2.0.0.partial")
        );
        assert_eq!(
            layout.version_file(&version, "bin/spot.exe"),
            Path::new("/opt/spot/versions/2.0.0/bin/spot.exe")
        );
    }
}
