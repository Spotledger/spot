//! WP-PRD-04.T9: where `spot backup` finds `pg_dump`: config, then `SPOT_PG_BIN`, then `PATH`.
//! The resolver reads no global state, so every case injects the environment and `PATH`. No
//! database. The fake `pg_dump` is an empty file: the resolver only looks, it never runs it.
#![cfg(unix)]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

/// The command registry, as `tests/backup.rs` includes it.
#[path = "../src/commands/mod.rs"]
mod commands;

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use commands::backup::{PgBinSource, resolve_pg_bin_dir};

/// A temporary directory with a fake `pg_dump` in it, removed on drop.
struct Tools(PathBuf);

impl Tools {
    fn new(tag: &str, mode: u32) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "spot_pg_bin_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("pg_dump"), b"").unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode)).unwrap();
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Tools {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn no_env(_: &str) -> Option<OsString> {
    None
}

fn env_of(value: &Path) -> impl Fn(&str) -> Option<OsString> + '_ {
    move |name| (name == "SPOT_PG_BIN").then(|| value.as_os_str().to_owned())
}

fn path_of(dirs: &[&Path]) -> OsString {
    std::env::join_paths(dirs.iter().map(|d| d.as_os_str())).unwrap()
}

#[test]
fn the_config_wins_over_the_environment_and_path() {
    let (a, b, c) = (
        Tools::new("cfg", 0o755),
        Tools::new("env", 0o755),
        Tools::new("path", 0o755),
    );
    let path = path_of(&[c.path()]);
    let (dir, source) =
        resolve_pg_bin_dir(Some(a.path()), &env_of(b.path()), Some(path.as_os_str())).unwrap();
    assert_eq!((dir.as_path(), source), (a.path(), PgBinSource::Config));
}

#[test]
fn the_environment_wins_over_path() {
    let (b, c) = (Tools::new("env2", 0o755), Tools::new("path2", 0o755));
    let path = path_of(&[c.path()]);
    let (dir, source) =
        resolve_pg_bin_dir(None, &env_of(b.path()), Some(path.as_os_str())).unwrap();
    assert_eq!(
        (dir.as_path(), source),
        (b.path(), PgBinSource::Environment)
    );
    assert_eq!(source.label(), "SPOT_PG_BIN");
}

#[test]
fn path_is_searched_in_order_and_relative_and_empty_entries_are_skipped() {
    let tools = Tools::new("path3", 0o755);
    let path = OsString::from(format!(":relative/dir:{}", tools.path().display()));
    let (dir, source) = resolve_pg_bin_dir(None, &no_env, Some(path.as_os_str())).unwrap();
    assert_eq!((dir.as_path(), source), (tools.path(), PgBinSource::Path));
}

#[test]
fn an_invalid_config_value_does_not_fall_through() {
    let good = Tools::new("good", 0o755);
    let empty = std::env::temp_dir().join(format!("spot_pg_bin_empty_{}", std::process::id()));
    std::fs::create_dir_all(&empty).unwrap();
    let path = path_of(&[good.path()]);
    let error = resolve_pg_bin_dir(
        Some(empty.as_path()),
        &env_of(good.path()),
        Some(path.as_os_str()),
    )
    .unwrap_err();
    assert!(
        error.contains("holds no pg_dump") && error.contains("(config)"),
        "{error}"
    );
    let missing = empty.join("nowhere");
    let error =
        resolve_pg_bin_dir(Some(missing.as_path()), &no_env, Some(path.as_os_str())).unwrap_err();
    assert!(error.contains("not a directory"), "{error}");
    let _ = std::fs::remove_dir_all(&empty);
}

#[test]
fn an_unusable_environment_value_does_not_fall_through_to_path() {
    let good = Tools::new("good2", 0o755);
    let bad = std::env::temp_dir().join("spot_pg_bin_does_not_exist");
    let path = path_of(&[good.path()]);
    let error = resolve_pg_bin_dir(None, &env_of(&bad), Some(path.as_os_str())).unwrap_err();
    assert!(error.contains("(SPOT_PG_BIN)"), "{error}");
}

#[test]
fn nothing_anywhere_lists_the_searched_directories() {
    let a = std::env::temp_dir().join("spot_pg_bin_a_none");
    let b = std::env::temp_dir().join("spot_pg_bin_b_none");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let path = path_of(&[a.as_path(), b.as_path()]);
    let error = resolve_pg_bin_dir(None, &no_env, Some(path.as_os_str())).unwrap_err();
    assert!(
        error.contains(&a.display().to_string()) && error.contains(&b.display().to_string()),
        "{error}"
    );
    assert!(!error.contains("password"), "{error}");
    let error = resolve_pg_bin_dir(None, &no_env, None).unwrap_err();
    assert!(error.contains("no absolute PATH entry"), "{error}");
    let _ = std::fs::remove_dir_all(&a);
    let _ = std::fs::remove_dir_all(&b);
}

#[test]
fn a_world_writable_directory_is_refused() {
    let tools = Tools::new("ww", 0o777);
    let error = resolve_pg_bin_dir(Some(tools.path()), &no_env, None).unwrap_err();
    assert!(
        error.contains("backup.target_unsafe") && error.contains("world-writable"),
        "{error}"
    );
    // The same rule when the directory came from PATH or the environment.
    let path = path_of(&[tools.path()]);
    assert!(
        resolve_pg_bin_dir(None, &no_env, Some(path.as_os_str()))
            .unwrap_err()
            .contains("world-writable")
    );
    assert!(
        resolve_pg_bin_dir(None, &env_of(tools.path()), None)
            .unwrap_err()
            .contains("world-writable")
    );
}

#[test]
fn a_symlink_to_a_world_writable_directory_is_refused() {
    let tools = Tools::new("ww2", 0o777);
    let link = std::env::temp_dir().join(format!("spot_pg_bin_link_{}", std::process::id()));
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(tools.path(), &link).unwrap();
    let error = resolve_pg_bin_dir(Some(link.as_path()), &no_env, None).unwrap_err();
    let _ = std::fs::remove_file(&link);
    assert!(error.contains("world-writable"), "{error}");
}

/// The usual distribution layout: `pg_dump` in the tools directory is a link to a wrapper that
/// lives elsewhere (Debian and Ubuntu: `/usr/bin/pg_dump -> ../share/postgresql-common/pg_wrapper`,
/// the link itself mode 0777). The link's own mode is not the tool's: the file it leads to is.
fn linked_tools(tag: &str, target_mode: u32) -> (Tools, PathBuf) {
    let tools = Tools::new(tag, 0o755);
    let wrapper_dir =
        std::env::temp_dir().join(format!("spot_pg_bin_wrapper_{tag}_{}", std::process::id()));
    std::fs::create_dir_all(&wrapper_dir).unwrap();
    let wrapper = wrapper_dir.join("pg_wrapper");
    std::fs::write(&wrapper, b"").unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(target_mode)).unwrap();
    std::fs::remove_file(tools.path().join("pg_dump")).unwrap();
    std::os::unix::fs::symlink(&wrapper, tools.path().join("pg_dump")).unwrap();
    (tools, wrapper_dir)
}

#[test]
fn a_pg_dump_link_to_a_safe_file_is_accepted() {
    let (tools, wrapper_dir) = linked_tools("lnk_ok", 0o755);
    let path = path_of(&[tools.path()]);
    let found = resolve_pg_bin_dir(None, &no_env, Some(path.as_os_str()));
    let configured = resolve_pg_bin_dir(Some(tools.path()), &no_env, None);
    let _ = std::fs::remove_dir_all(&wrapper_dir);
    assert!(found.is_ok(), "{found:?}");
    assert!(configured.is_ok(), "{configured:?}");
}

#[test]
fn a_pg_dump_link_to_a_world_writable_file_is_refused_and_named() {
    let (tools, wrapper_dir) = linked_tools("lnk_ww", 0o777);
    let error = resolve_pg_bin_dir(Some(tools.path()), &no_env, None).unwrap_err();
    let _ = std::fs::remove_dir_all(&wrapper_dir);
    assert!(
        error.contains("backup.target_unsafe") && error.contains("world-writable"),
        "{error}"
    );
    // The message names the file that failed and its mode.
    assert!(
        error.contains("pg_wrapper") && error.contains("0777"),
        "{error}"
    );
}
