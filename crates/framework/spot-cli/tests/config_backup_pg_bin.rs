//! WP-PRD-04.T9: the `[backup] pg_bin_dir` key. No database: only the lexical rules of the
//! loader. Run-time checks (the directory exists, is not world-writable) belong to the command.
#![allow(missing_docs, clippy::unwrap_used)]
use spot_cli::config::load;
use std::path::PathBuf;

/// A drive prefix on Windows, where `/opt/pg/bin` is not absolute.
const ROOT: &str = if cfg!(windows) { "C:" } else { "" };

fn load_with(tag: &str, extra: &str) -> Result<spot_cli::config::Loaded, String> {
    let root = std::env::temp_dir().join(format!(
        "spot_cli_pg_bin_{tag}_{}_{}",
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

fn refused(tag: &str, extra: &str) -> String {
    match load_with(tag, extra) {
        Ok(_) => panic!("expected {tag} to be refused"),
        Err(e) => e,
    }
}

#[test]
fn an_absent_key_is_none() {
    let loaded = load_with("absent", "[backup]\nverify = 'quick'").unwrap();
    assert_eq!(loaded.common.backup.pg_bin_dir, None);
}

#[test]
fn an_absolute_directory_parses() {
    let loaded = load_with("ok", &format!("[backup]\npg_bin_dir = '{ROOT}/opt/pg/bin'")).unwrap();
    assert_eq!(
        loaded.common.backup.pg_bin_dir,
        Some(PathBuf::from(format!("{ROOT}/opt/pg/bin")))
    );
}

#[test]
fn a_relative_a_dotdot_and_an_empty_value_are_refused_naming_the_key() {
    for (tag, value) in [("rel", "pg/bin"), ("dots", "/opt/../pg/bin"), ("empty", "")] {
        let value = if value.starts_with('/') {
            format!("{ROOT}{value}")
        } else {
            value.to_owned()
        };
        let message = refused(tag, &format!("[backup]\npg_bin_dir = '{value}'"));
        assert!(message.contains("backup.pg_bin_dir"), "{tag}: {message}");
    }
}

#[test]
fn an_unknown_key_is_still_refused() {
    let message = refused("unknown", "[backup]\npg_bin = '/opt/pg/bin'");
    assert!(message.contains("pg_bin"), "{message}");
}
