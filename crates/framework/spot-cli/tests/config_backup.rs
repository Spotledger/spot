#![allow(missing_docs, clippy::unwrap_used)]
use spot_cli::config::{BackupConfigError, BackupTarget, load};
use std::path::PathBuf;

/// A drive prefix on Windows, where `/a` is not absolute.
const ROOT: &str = if cfg!(windows) { "C:" } else { "" };

/// A root whose `common.toml` is the minimal valid one plus `extra`; returns the load outcome.
fn load_with(tag: &str, extra: &str) -> Result<spot_cli::config::Loaded, String> {
    let root = std::env::temp_dir().join(format!(
        "spot_cli_backup_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("apps/books/doctypes")).unwrap();
    std::fs::write(
        root.join("common.toml"),
        format!(
            "apps = [{:?}]\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\nadmin_url = 'postgres://u:p@127.0.0.1:15432/postgres'\n{extra}\n",
            root.join("apps/books").display().to_string()
        ),
    )
    .unwrap();
    let out = load(&root, false).map_err(|e| e.to_string());
    std::fs::remove_dir_all(root).unwrap();
    out
}

/// The message of a refused `[backup]` table.
fn refused(tag: &str, extra: &str) -> String {
    match load_with(tag, extra) {
        Ok(_) => panic!("expected {tag} to be refused"),
        Err(e) => e,
    }
}

fn folder(name: &str, path: &str) -> String {
    format!("[backup.targets.{name}]\nkind = 'folder'\npath = '{ROOT}{path}'\n")
}

#[test]
fn without_a_backup_table_there_are_no_targets() {
    let loaded = load_with("absent", "").unwrap();
    assert!(loaded.common.backup.targets.is_empty());
    assert_eq!(loaded.common.backup.verify, "quick");
    assert_eq!(loaded.common.backup.retention.keep_daily, 7);
    assert_eq!(loaded.common.backup.retention.keep_weekly, 4);
    assert_eq!(loaded.common.backup.retention.keep_monthly, 12);
    assert_eq!(loaded.common.backup.retention.keep_last, 1);
}

#[test]
fn a_full_valid_table_parses() {
    let extra = format!(
        "[backup]\nverify = 'full'\ndefault_target = 'usb'\n\
         [backup.retention]\nkeep_daily = 3\nkeep_weekly = 2\nkeep_monthly = 6\nkeep_last = 5\n\
         {}{}\
         [backup.targets.offsite]\nkind = 'cloud'\nendpoint = 'https://s3.example.com/bucket'\nkey_id = 'key-1'\n",
        folder("usb", "/mnt/usb/backups"),
        folder("local", "/var/backups/spot"),
    );
    let loaded = load_with("full", &extra).unwrap();
    let backup = &loaded.common.backup;
    assert_eq!(backup.targets.len(), 3);
    assert_eq!(backup.verify, "full");
    assert_eq!(backup.retention.keep_last, 5);
    assert_eq!(backup.default_target.as_deref(), Some("usb"));
    assert!(matches!(
        backup.targets.get("offsite"),
        Some(BackupTarget::Cloud { key_id, .. }) if key_id == "key-1"
    ));
    let (name, target) = backup.target(None).unwrap();
    assert_eq!(name, "usb");
    assert_eq!(
        target,
        &BackupTarget::Folder {
            path: PathBuf::from(format!("{ROOT}/mnt/usb/backups"))
        }
    );
}

#[test]
fn bad_target_names_are_refused() {
    let long = "a".repeat(33);
    for (i, name) in ["Upper", "1abc", "has space", "x.y", long.as_str()]
        .iter()
        .enumerate()
    {
        let extra =
            format!("[backup.targets.\"{name}\"]\nkind = 'folder'\npath = '{ROOT}/data{i}'\n");
        let error = refused(&format!("name{i}"), &extra);
        assert!(error.contains("name must match"), "{name}: {error}");
    }
    let extra = folder("ok_name-1", "/data");
    assert!(load_with("namegood", &extra).is_ok());
}

#[test]
fn folder_paths_must_be_absolute_and_free_of_dot_dot() {
    let error = refused("empty", "[backup.targets.a]\nkind = 'folder'\npath = ''\n");
    assert!(
        error.contains("backup.targets.a.path: must not be empty"),
        "{error}"
    );
    let error = refused(
        "relative",
        "[backup.targets.a]\nkind = 'folder'\npath = 'backups/here'\n",
    );
    assert!(error.contains("must be an absolute path"), "{error}");
    let error = refused("dotdot", &folder("a", "/data/../etc"));
    assert!(error.contains("'..' component"), "{error}");
}

#[test]
fn two_targets_may_not_share_a_path() {
    let extra = format!(
        "{}{}{}",
        folder("one", "/a/b"),
        folder("two", "/a/b/"),
        folder("three", "/a/./b")
    );
    let error = refused("dup", &extra);
    assert!(
        error.contains("backup.targets.three.path: names the same path"),
        "{error}"
    );
    assert!(
        error.contains("backup.targets.two.path: names the same path"),
        "{error}"
    );
    assert!(!error.contains("backup.targets.one.path"), "{error}");
}

#[test]
fn the_default_target_must_exist() {
    let extra = format!("[backup]\ndefault_target = 'gone'\n{}", folder("a", "/x"));
    let error = refused("default", &extra);
    assert!(error.contains("backup.default_target"), "{error}");
}

#[test]
fn cloud_endpoints_must_be_plain_https() {
    let cloud = |endpoint: &str| {
        format!("[backup.targets.c]\nkind = 'cloud'\nendpoint = '{endpoint}'\nkey_id = 'k'\n")
    };
    for (i, bad) in [
        "http://s3.example.com",
        "ftp://s3.example.com",
        "s3.example.com",
        "https://",
        "https:///path",
        "https://s3.example.com/b?x=1",
        "https://s3.example.com/b#frag",
    ]
    .iter()
    .enumerate()
    {
        let error = refused(&format!("ep{i}"), &cloud(bad));
        assert!(
            error.contains("backup.targets.c.endpoint"),
            "{bad}: {error}"
        );
    }
    let error = refused("userinfo", &cloud("https://admin:hunter2@s3.example.com/b"));
    assert!(error.contains("must not contain userinfo"), "{error}");
    assert!(!error.contains("hunter2"), "{error}");
    assert!(!error.contains("admin"), "{error}");
    assert!(load_with("epok", &cloud("https://s3.example.com:9000/b")).is_ok());
}

#[test]
fn cloud_needs_exactly_its_fields() {
    let error = refused(
        "cloudextra",
        "[backup.targets.c]\nkind = 'cloud'\nendpoint = 'https://h'\nkey_id = 'k'\npath = '/x'\n",
    );
    assert!(error.contains("backup"), "{error}");
    let error = refused(
        "cloudmissing",
        "[backup.targets.c]\nkind = 'cloud'\nendpoint = 'https://h'\n",
    );
    assert!(error.contains("backup"), "{error}");
}

#[test]
fn bounds_are_enforced() {
    let error = refused("keep0", "[backup.retention]\nkeep_last = 0\n");
    assert!(error.contains("keep_last: must be at least 1"), "{error}");
    let error = refused("big", "[backup.retention]\nkeep_daily = 10001\n");
    assert!(
        error.contains("backup.retention.keep_daily: must be at most 10000"),
        "{error}"
    );
    assert!(load_with("edge", "[backup.retention]\nkeep_daily = 10000\n").is_ok());
    let error = refused("verify", "[backup]\nverify = 'thorough'\n");
    assert!(
        error.contains("backup.verify: must be quick or full"),
        "{error}"
    );
    let many: String = (0..33)
        .map(|i| folder(&format!("t{i}"), &format!("/d/{i}")))
        .collect();
    let error = refused("many", &many);
    assert!(error.contains("at most 32 targets"), "{error}");
    let ok: String = (0..32)
        .map(|i| folder(&format!("t{i}"), &format!("/d/{i}")))
        .collect();
    assert!(load_with("max", &ok).is_ok());
}

#[test]
fn unknown_keys_and_kinds_are_refused() {
    let error = refused("topkey", "[backup]\ncolour = 'red'\n");
    assert!(error.contains("backup"), "{error}");
    let error = refused("retkey", "[backup.retention]\nkeep_yearly = 1\n");
    assert!(error.contains("backup"), "{error}");
    let error = refused("targetkey", &format!("{}extra = 1\n", folder("a", "/x")));
    assert!(error.contains("backup"), "{error}");
    let error = refused("kind", "[backup.targets.a]\nkind = 'tape'\npath = '/x'\n");
    assert!(error.contains("backup"), "{error}");
}

#[test]
fn target_helper_has_four_outcomes() {
    let one = load_with("one", &folder("only", "/x"))
        .unwrap()
        .common
        .backup;
    assert_eq!(one.target(None).unwrap().0, "only");
    assert_eq!(one.target(Some("only")).unwrap().0, "only");
    assert_eq!(
        one.target(Some("nope")),
        Err(BackupConfigError::Unknown("nope".into()))
    );

    let none = load_with("none", "").unwrap().common.backup;
    assert_eq!(none.target(None), Err(BackupConfigError::NoTargets));

    let two = format!("{}{}", folder("a", "/x"), folder("b", "/y"));
    let two = load_with("two", &two).unwrap().common.backup;
    assert_eq!(two.target(None), Err(BackupConfigError::Ambiguous));
    assert_eq!(two.target(Some("b")).unwrap().0, "b");

    let with_default = format!(
        "[backup]\ndefault_target = 'b'\n{}{}",
        folder("a", "/x"),
        folder("b", "/y")
    );
    let with_default = load_with("dflt", &with_default).unwrap().common.backup;
    assert_eq!(with_default.target(None).unwrap().0, "b");
    assert_eq!(with_default.target(Some("a")).unwrap().0, "a");
}

#[test]
fn unknown_keys_inside_backup_give_the_decode_diagnostic() {
    // The decode pass names the field and the table, not the serde "invalid schema" fallback.
    let error = refused("dtop", "[backup]\ncolour = 'red'\n");
    assert!(error.contains(".backup: unknown field colour"), "{error}");
    let error = refused("dret", "[backup.retention]\nkeep_yearly = 1\n");
    assert!(
        error.contains(".backup.retention: unknown field keep_yearly"),
        "{error}"
    );
    let error = refused("dtgt", &format!("{}extra = 1\n", folder("a", "/x")));
    assert!(
        error.contains(".backup.targets.a: unknown field extra"),
        "{error}"
    );
    let error = refused("dtype", "[backup]\nverify = 3\n");
    assert!(error.contains(".backup.verify: invalid string"), "{error}");
}
