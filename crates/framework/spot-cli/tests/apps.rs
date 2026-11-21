//! App directory validation, the site file's install list and credentials (no database).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
use spot_cli::appdir;
use spot_cli::config::{self, Site};
use spot_storage_contract::RecordId;
use std::path::{Path, PathBuf};

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn sample() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../apps/sample")
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
fn scratch() -> Dir {
    let dir = Dir(std::env::temp_dir().join(format!("spot_cli_apps_{}", RecordId::new_v7())));
    copy(&sample(), &dir.0);
    dir
}

#[test]
fn the_shipped_apps_validate() {
    let app = appdir::load(&sample()).unwrap();
    assert_eq!(
        (app.name.as_str(), app.version.as_str()),
        ("sample", "1.0.0")
    );
    assert_eq!(app.definitions.len(), 6);
    let platform = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../apps/platform");
    assert_eq!(appdir::load(&platform).unwrap().name, "platform");
}

#[test]
fn every_problem_in_an_app_directory_is_reported_together() {
    let dir = scratch();
    std::fs::write(dir.0.join("doctypes/task.toml"), "name = [").unwrap();
    std::fs::write(dir.0.join("doctypes/label.toml"), "nonsense").unwrap();
    std::fs::write(
        dir.0.join("permissions/grants.toml"),
        "[[grants]]\nbroken = true\n",
    )
    .unwrap();
    std::fs::write(
        dir.0.join("app.toml"),
        "[app]\nname = \"sample\"\nversion = \"nope\"\n",
    )
    .unwrap();
    let error = appdir::load(&dir.0).err().unwrap().to_string();
    for expected in ["task.toml", "label.toml", "app.toml", "permissions"] {
        assert!(error.contains(expected), "{expected} missing from {error}");
    }
}

#[test]
fn a_controller_must_be_one_this_binary_provides() {
    let dir = scratch();
    let manifest = std::fs::read_to_string(dir.0.join("app.toml")).unwrap();
    for (reference, expected) in [
        ("native:nothing", "not built into this binary"),
        ("wasm:run", "cannot run in this host"),
        ("garbage", "controller"),
    ] {
        std::fs::write(
            dir.0.join("app.toml"),
            manifest.replace("native:sample", reference),
        )
        .unwrap();
        let error = appdir::load(&dir.0).err().unwrap().to_string();
        assert!(error.contains(expected), "{reference}: {error}");
    }
    std::fs::write(dir.0.join("app.toml"), &manifest).unwrap();
    assert!(appdir::load(&dir.0).is_ok());
}

#[test]
fn a_directory_without_a_manifest_or_doctypes_is_refused() {
    let dir = Dir(std::env::temp_dir().join(format!("spot_cli_apps_{}", RecordId::new_v7())));
    std::fs::create_dir_all(&dir.0).unwrap();
    let error = appdir::load(&dir.0).err().unwrap().to_string();
    assert!(
        error.contains("missing app.toml") && error.contains("missing doctypes"),
        "{error}"
    );
}

struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn root_with_site(site: &str) -> Root {
    let root = Root(std::env::temp_dir().join(format!("spot_cli_site_{}", RecordId::new_v7())));
    std::fs::create_dir_all(root.0.join("app/doctypes")).unwrap();
    std::fs::create_dir_all(root.0.join("sites/demo")).unwrap();
    std::fs::write(
        root.0.join("common.toml"),
        "apps = ['app']\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\nadmin_url = 'postgres://spot@localhost/postgres'\n",
    )
    .unwrap();
    std::fs::write(root.0.join("sites/demo/site.toml"), site).unwrap();
    root
}

#[test]
fn the_install_list_must_start_with_the_platform_and_hold_valid_unique_names() {
    for (apps, expected) in [
        ("['sample']", "must start with `platform`"),
        ("['platform', 'platform']", "invalid or duplicate app"),
        ("['platform', 'Bad Name']", "invalid or duplicate app"),
    ] {
        let root = root_with_site(&format!(
            "site = 'demo'\ndatabase = 'demo'\napps = {apps}\n"
        ));
        let error = config::load(&root.0, false).err().unwrap().to_string();
        assert!(error.contains(expected), "{apps}: {error}");
    }
    let root = root_with_site("site = 'demo'\ndatabase = 'demo'\napps = ['platform', 'sample']\n");
    assert_eq!(
        config::load(&root.0, false).unwrap().sites[0].apps,
        ["platform", "sample"]
    );
}

#[test]
fn password_verifiers_are_validated_without_echoing_them() {
    let root = root_with_site(
        "site = 'demo'\ndatabase = 'demo'\n[[credentials]]\nuser = 'not-an-id'\npassword_hash = 'plain-text-secret'\n",
    );
    let error = config::load(&root.0, false).err().unwrap().to_string();
    assert!(error.contains("password verifier"), "{error}");
    assert!(!error.contains("plain-text-secret"));
    let root = root_with_site("site = 'demo'\ndatabase = 'demo'\n[[credentials]]\nuser = 'x'\n");
    let error = config::load(&root.0, false).err().unwrap().to_string();
    assert!(error.contains("missing required field"), "{error}");
}

#[test]
fn a_site_without_apps_or_credentials_serialises_as_before() {
    let site = Site::named("demo", "demo", "en", Vec::new());
    let text = toml::to_string(&site).unwrap();
    assert!(
        !text.contains("apps") && !text.contains("credentials"),
        "{text}"
    );
}
