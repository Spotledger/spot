//! A platform app listed in a host's `common.toml` loads from disk the same permission model
//! the embedded app gives (WP-INT-01.T6): every `*grants.toml` and the webhook groups.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
use spot_cli::{appdir, apps, config};
use spot_storage_contract::RecordId;
use std::path::{Path, PathBuf};

struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn platform_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../apps/platform")
}

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &to.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

/// A host root; `catalogued` lists a copy of `apps/platform` in `common.toml`.
fn host(catalogued: bool) -> Root {
    let root = Root(std::env::temp_dir().join(format!("spot_cli_platform_{}", RecordId::new_v7())));
    std::fs::create_dir_all(root.0.join("sites/demo")).unwrap();
    let list = if catalogued {
        copy(&platform_dir(), &root.0.join("apps/platform"));
        "['apps/platform']"
    } else {
        // The host needs a non-empty list; the sample app is not installed on the site.
        copy(
            &platform_dir().join("../sample"),
            &root.0.join("apps/sample"),
        );
        "['apps/sample']"
    };
    std::fs::write(
        root.0.join("common.toml"),
        format!(
            "apps = {list}\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\nadmin_url = 'postgres://spot@localhost/postgres'\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.0.join("sites/demo/site.toml"),
        "site = 'demo'\ndatabase = 'demo'\napps = ['platform']\n",
    )
    .unwrap();
    root
}

fn model(root: &Root) -> Result<spot_permissions::Model, String> {
    let loaded = config::load(&root.0, false).unwrap();
    apps::site_model(&root.0, &loaded, &loaded.sites[0]).map_err(|e| e.to_string())
}

fn debug_each<T: std::fmt::Debug>(items: &[T]) -> Vec<String> {
    items.iter().map(|i| format!("{i:?}")).collect()
}

#[test]
fn the_catalogue_loaded_model_equals_the_embedded_one() {
    let disk = model(&host(true)).unwrap();
    let embedded = model(&host(false)).unwrap();
    assert_eq!(debug_each(&disk.grants), debug_each(&embedded.grants));
    assert_eq!(
        debug_each(&disk.field_grants),
        debug_each(&embedded.field_grants)
    );
    assert_eq!(debug_each(&disk.groups), debug_each(&embedded.groups));
    assert_eq!(debug_each(&disk.shares), debug_each(&embedded.shares));
    // The webhook grants and both webhook groups are there, and `secret` is traced.
    assert!(
        debug_each(&disk.grants)
            .iter()
            .any(|g| g.contains("webhook\"") || g.contains("webhook_delivery"))
    );
    for name in ["secret", "config"] {
        assert!(
            disk.groups.iter().any(|g| g.name.as_str() == name),
            "group {name} missing"
        );
    }
    assert!(
        disk.groups
            .iter()
            .any(|g| g.name.as_str() == "secret" && g.trace)
    );
}

#[test]
fn every_grants_file_is_loaded_in_name_order_and_other_files_are_not() {
    let dir = Root(std::env::temp_dir().join(format!("spot_cli_perm_{}", RecordId::new_v7())));
    std::fs::create_dir_all(&dir.0).unwrap();
    for (file, text) in [
        ("grants.toml", "# base"),
        ("zeta_grants.toml", "# zeta"),
        ("alpha_grants.toml", "# alpha"),
        ("field_grants.toml", "# fields"),
        ("groups.toml", "# groups"),
        ("notes.toml", "# notes"),
        ("grants.toml.bak", "# bak"),
    ] {
        std::fs::write(dir.0.join(file), text).unwrap();
    }
    let texts = appdir::read_permission_texts(&dir.0);
    assert_eq!(texts.grants, "# base\n# alpha\n# zeta");
    assert_eq!(texts.field_grants, "# fields");
    assert_eq!(texts.groups, "# groups");
    assert!(texts.shares.is_empty());
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_directory_is_not_followed() {
    let outside = Root(std::env::temp_dir().join(format!("spot_cli_out_{}", RecordId::new_v7())));
    std::fs::create_dir_all(&outside.0).unwrap();
    std::fs::write(outside.0.join("secret.toml"), "# outside").unwrap();
    let dir = Root(std::env::temp_dir().join(format!("spot_cli_perm_{}", RecordId::new_v7())));
    std::fs::create_dir_all(&dir.0).unwrap();
    std::fs::write(dir.0.join("grants.toml"), "# base").unwrap();
    std::os::unix::fs::symlink(
        outside.0.join("secret.toml"),
        dir.0.join("evil_grants.toml"),
    )
    .unwrap();
    let texts = appdir::read_permission_texts(&dir.0);
    assert!(!texts.grants.contains("outside"), "{}", texts.grants);
}

#[test]
fn an_extra_grants_file_added_to_the_catalogue_directory_is_loaded() {
    let root = host(true);
    let before = model(&root).unwrap();
    let dir = root.0.join("apps/platform/permissions");
    // A copy of the webhook grants under another name is read: the loader sees the duplicates.
    std::fs::copy(
        dir.join("webhook_grants.toml"),
        dir.join("extra_grants.toml"),
    )
    .unwrap();
    let error = model(&root).expect_err("the extra file is loaded");
    assert!(error.contains("duplicate grant"), "{error}");
    std::fs::remove_file(dir.join("extra_grants.toml")).unwrap();
    // A file that does not end in `grants.toml` is never read, even when it is not valid TOML.
    std::fs::write(dir.join("unrelated.toml"), "this is not toml [[").unwrap();
    let after = model(&root).unwrap();
    assert_eq!(debug_each(&after.grants), debug_each(&before.grants));
}
