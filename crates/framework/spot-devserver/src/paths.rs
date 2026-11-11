//! Cross-platform, UTF-8 path handling for the dev server.
//!
//! Everything here is `camino`/`Utf8PathBuf` or `std::path`, never `OsStr` juggling. The rules
//! the watcher relies on:
//!
//! - [`is_ignored`]: `target/`, `node_modules/` and `.git/` (and friends) are never watched.
//! - [`is_atomic_temp`]: an editor's write-then-rename temp file is not a change; only the
//!   rename's destination is.
//! - [`path_key`]: the debounce key. It folds case and normalises `\` to `/`, so the same file
//!   seen as `Views/x.toml` on one event and `views/x.toml` on the next debounces **once** —
//!   matching Windows' case-insensitive filesystem and inotify's two spellings for one inode.
//! - [`absolute`]: long-path safe on Windows: an existing path is canonicalised (which yields
//!   the `\\?\` prefix), a missing one is normalised lexically without touching the disk.

use camino::{Utf8Component, Utf8Path, Utf8PathBuf};

use crate::error::{DevError, DevResult};

/// Directory names never watched, wherever they appear in a path.
pub const IGNORED_DIRS: &[&str] = &["target", "node_modules", ".git", ".svn", ".hg", ".spot"];

/// Whether `path` names anything under an ignored directory.
#[must_use]
pub fn is_ignored(path: &Utf8Path) -> bool {
    path.components().any(|component| match component {
        Utf8Component::Normal(name) => IGNORED_DIRS.contains(&name),
        _ => false,
    })
}

/// Whether `path` looks like the temporary file an editor writes before it renames onto the real
/// one (vim's `4913`/`.swp`, emacs' `#file#`/`file~`, VS Code's `file.tmp-1234`, Kate's
/// `.kate-swp`). Such a path is never a change in its own right.
#[must_use]
pub fn is_atomic_temp(path: &Utf8Path) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    if name == "4913" {
        return true;
    }
    if name.len() > 1 && name.starts_with('#') && name.ends_with('#') {
        return true;
    }
    if name.ends_with('~') {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    for suffix in [".swp", ".swx", ".tmp", ".bak", ".kate-swp"] {
        if lower.ends_with(suffix) {
            return true;
        }
    }
    lower.contains(".tmp.") || lower.contains(".tmp-")
}

/// The debounce key of `path`: lower-case, with `\` folded to `/`.
#[must_use]
pub fn path_key(path: &Utf8Path) -> String {
    let mut key = String::with_capacity(path.as_str().len());
    for character in path.as_str().chars() {
        key.push(match character {
            '\\' => '/',
            other => other.to_ascii_lowercase(),
        });
    }
    key
}

/// `path` relative to `root` when it is under it, else `path` unchanged.
#[must_use]
pub fn relative_to(root: &Utf8Path, path: &Utf8Path) -> Utf8PathBuf {
    match path.strip_prefix(root) {
        Ok(relative) => relative.to_owned(),
        Err(_) => path.to_owned(),
    }
}

/// The absolute form of `path`, resolved against `root` when it is relative.
///
/// An existing path is canonicalised, which on Windows also produces the `\\?\` long-path form;
/// a path that does not exist yet is normalised lexically (no disk access, no `..` left in it).
///
/// # Errors
///
/// [`DevError::Config`] when `path` is empty.
pub fn absolute(root: &Utf8Path, path: &Utf8Path) -> DevResult<Utf8PathBuf> {
    if path.as_str().is_empty() {
        return Err(DevError::Config {
            message: "a watched path is empty".into(),
        });
    }
    let joined = if path.is_absolute() {
        path.to_owned()
    } else {
        root.join(path)
    };
    match joined.canonicalize_utf8() {
        Ok(canonical) => Ok(canonical),
        Err(_) => Ok(lexical(&joined)),
    }
}

/// Removes `.` and resolves `..` lexically, preserving a leading root or prefix.
fn lexical(path: &Utf8Path) -> Utf8PathBuf {
    let mut out = Utf8PathBuf::new();
    for component in path.components() {
        match component {
            Utf8Component::Prefix(prefix) => out.push(prefix.as_str()),
            Utf8Component::RootDir => out.push("/"),
            Utf8Component::CurDir => {}
            Utf8Component::ParentDir => {
                out.pop();
            }
            Utf8Component::Normal(name) => out.push(name),
        }
    }
    out
}

/// Every file under `dir` whose extension is `extension` (with or without the leading dot),
/// sorted by path and skipping [`is_ignored`] directories. A missing `dir` is an empty list.
///
/// # Errors
///
/// [`DevError::Io`] when the directory cannot be read.
pub fn walk_files(dir: &Utf8Path, extension: &str) -> DevResult<Vec<Utf8PathBuf>> {
    let extension = extension.trim_start_matches('.');
    let mut out = Vec::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    walk_into(dir, extension, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_into(dir: &Utf8Path, extension: &str, out: &mut Vec<Utf8PathBuf>) -> DevResult<()> {
    let entries = std::fs::read_dir(dir).map_err(|error| DevError::io(dir, &error))?;
    for entry in entries {
        let entry = entry.map_err(|error| DevError::io(dir, &error))?;
        let Some(path) = Utf8PathBuf::from_path_buf(entry.path()).ok() else {
            continue;
        };
        if is_ignored(&path) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| DevError::io(&path, &error))?;
        if file_type.is_dir() {
            // `file_type` does not follow symlinks, so a link back up the tree cannot loop.
            walk_into(&path, extension, out)?;
        } else if path.extension().is_some_and(|found| found == extension) {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn ignored_directories_anywhere_in_the_path() {
        assert!(is_ignored(Utf8Path::new("/app/target/debug/x.rs")));
        assert!(is_ignored(Utf8Path::new("node_modules/pkg/index.ts")));
        assert!(is_ignored(Utf8Path::new("a/.git/HEAD")));
        assert!(!is_ignored(Utf8Path::new("app/ts/main.ts")));
    }

    #[test]
    fn editor_temp_files_are_recognised() {
        for name in [
            "4913",
            "#main.ts#",
            "main.ts~",
            ".main.ts.swp",
            "main.ts.tmp-4482",
            ".main.ts.kate-swp",
        ] {
            assert!(is_atomic_temp(Utf8Path::new(name)), "{name}");
        }
        assert!(!is_atomic_temp(Utf8Path::new("main.ts")));
        assert!(!is_atomic_temp(Utf8Path::new("main.wasm")));
    }

    #[test]
    fn path_keys_fold_case_and_separators() {
        assert_eq!(
            path_key(Utf8Path::new("Views\\Form.toml")),
            path_key(Utf8Path::new("views/form.toml"))
        );
    }

    #[test]
    fn absolute_resolves_relative_and_removes_dot_dot() {
        let root = Utf8Path::new("/scratch/dev");
        let resolved = absolute(root, Utf8Path::new("meta/../meta/widget.toml")).unwrap();
        assert_eq!(resolved, Utf8Path::new("/scratch/dev/meta/widget.toml"));
    }

    #[test]
    fn walk_files_finds_extensions_and_skips_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8Path::from_path(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("meta")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join("meta/a.toml"), "name = \"a\"").unwrap();
        std::fs::write(root.join("meta/b.tres"), "x").unwrap();
        std::fs::write(root.join("target/c.toml"), "name = \"c\"").unwrap();
        let found = walk_files(root, "toml").unwrap();
        assert_eq!(found, vec![root.join("meta/a.toml")]);
    }
}
