#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

#[test]
fn include_defaults_to_every_category_and_rejects_unknown_ones() {
    let all = parse_include(None).expect("default");
    assert_eq!(all.len(), 4);
    assert!(parse_include(Some("logs,config")).expect("subset").contains("logs"));
    assert!(parse_include(Some("bogus")).is_err());
    assert!(parse_include(Some("")).is_err());
}

#[test]
fn since_parses_the_documented_suffixes() {
    assert_eq!(parse_since("45s").expect("s"), 45);
    assert_eq!(parse_since("30m").expect("m"), 1_800);
    assert_eq!(parse_since("24h").expect("h"), 86_400);
    assert_eq!(parse_since("7d").expect("d"), 604_800);
    assert!(parse_since("24x").is_err());
    assert!(parse_since("h").is_err());
}

/// The panic the review found: `split_at` on a byte offset that is not a char boundary.
/// `--since é` (and any other non-ASCII suffix) must be a clean refusal, never a panic.
#[test]
fn since_rejects_non_ascii_input_without_panicking() {
    assert!(parse_since("é").is_err());
    assert!(parse_since("24é").is_err());
    assert!(parse_since("日").is_err());
    assert!(parse_since("").is_err());
}

#[test]
fn the_data_directory_refusal_covers_the_whole_sites_tree() {
    let root = std::env::temp_dir().join("spot-support-bundle-datadir-test");
    let _ = std::fs::create_dir_all(root.join("sites").join("acme"));
    assert!(parent_inside_data_dir(&root.join("sites").join("acme"), &root));
    assert!(parent_inside_data_dir(&root.join("sites"), &root));
    assert!(!parent_inside_data_dir(&root, &root));
}

#[test]
fn normalise_resolves_dot_dot_without_touching_disk() {
    let p = normalise(Path::new("/a/b/../c/./d"));
    assert_eq!(p, PathBuf::from("/a/c/d"));
}

#[test]
fn config_summary_shows_only_allow_listed_keys() {
    let value: toml::Value = toml::from_str(
        "apps = [\"platform\"]\n\n[postgres]\nadmin_url = \"postgres://admin:sekrit@db.internal:5432/x\"\n\n[extra]\nowner = \"Bob Smith\"\n",
    )
    .expect("toml");
    let redactor = Redactor::from_policy(&RedactionPolicy::default()).expect("redactor");
    let summary = summarise_config(&value, &redactor);
    assert!(summary.contains("apps = [platform]"), "{summary}");
    assert!(!summary.contains("sekrit"), "{summary}");
    assert!(!summary.contains("db.internal"), "{summary}");
    assert!(!summary.contains("Bob Smith"), "{summary}");
    assert!(summary.contains("postgres.admin_url = [redacted:string]"), "{summary}");
    assert!(summary.contains("extra.owner = [redacted:string]"), "{summary}");
}

#[test]
fn walk_logs_stops_at_the_given_cap() {
    let root = std::env::temp_dir().join(format!(
        "spot-support-bundle-walk-cap-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(&root).expect("dir");
    for i in 0..5 {
        std::fs::write(root.join(format!("{i}.log")), b"x").expect("write");
    }
    let mut found = Vec::new();
    walk_logs(&root, 0, &mut found, 2);
    assert_eq!(found.len(), 2, "the cap must stop the walk early: {found:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn walk_logs_never_follows_a_symlinked_log_or_directory() {
    use std::os::unix::fs::symlink;
    let base = std::env::temp_dir().join(format!(
        "spot-support-bundle-walk-symlink-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    let outside = base.join("outside");
    let root = base.join("root");
    std::fs::create_dir_all(&outside).expect("outside dir");
    std::fs::create_dir_all(&root).expect("root dir");
    std::fs::write(outside.join("secret.log"), b"outside secret").expect("secret log");
    symlink(outside.join("secret.log"), root.join("link.log")).expect("symlink log");
    symlink(&outside, root.join("link_dir")).expect("symlink dir");

    let mut found = Vec::new();
    walk_logs(&root, 0, &mut found, MAX_LOG_FILES);
    assert!(found.is_empty(), "a symlinked log or directory was walked: {found:?}");
    let _ = std::fs::remove_dir_all(&base);
}

#[test]
fn cap_entries_with_limits_caps_both_file_count_and_total_bytes() {
    let entries = vec![
        Entry { name: "a".into(), bytes: vec![0; 10] },
        Entry { name: "b".into(), bytes: vec![0; 10] },
        Entry { name: "c".into(), bytes: vec![0; 10] },
    ];
    let (kept, files_capped, _) = cap_entries_with_limits(entries, 2, 1_000);
    assert_eq!(kept.len(), 2, "the file-count cap must apply");
    assert!(files_capped);

    let entries = vec![
        Entry { name: "a".into(), bytes: vec![0; 10] },
        Entry { name: "b".into(), bytes: vec![0; 10] },
        Entry { name: "c".into(), bytes: vec![0; 10] },
    ];
    let (kept, _, bytes_capped) = cap_entries_with_limits(entries, 100, 15);
    let total: usize = kept.iter().map(|e| e.bytes.len()).sum();
    assert!(total <= 15, "the byte cap must apply: {total}");
    assert!(bytes_capped);
}

#[test]
fn cap_entries_with_limits_is_a_no_op_under_both_limits() {
    let entries = vec![Entry { name: "a".into(), bytes: vec![0; 5] }];
    let (kept, files_capped, bytes_capped) = cap_entries_with_limits(entries, 10, 1_000);
    assert_eq!(kept.len(), 1);
    assert!(!files_capped);
    assert!(!bytes_capped);
}

/// The "env-var secret plant" case: a secret-shaped environment-variable name gets its
/// value registered and redacted, without mutating the real process environment (see
/// [`register_secrets_from_env_like`]'s doc comment for why).
#[test]
fn a_secret_shaped_env_var_name_is_registered_and_redacted() {
    let policy = RedactionPolicy::default();
    let mut redactor = Redactor::from_policy(&policy).expect("redactor");
    let vars = vec![
        ("SPOT_TEST_SECRET_TOKEN".to_owned(), "env-planted-secret-0123456789abcdef".to_owned()),
        ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
    ];
    register_secrets_from_env_like(&mut redactor, vars.into_iter());
    let text = "an upstream service echoed back token=env-planted-secret-0123456789abcdef and PATH=/usr/bin:/bin";
    let redacted = redactor.redact_str(text);
    assert!(
        !redacted.contains("env-planted-secret-0123456789abcdef"),
        "{redacted}"
    );
    assert!(redacted.contains("[redacted"), "{redacted}");
    assert!(
        redacted.contains("/usr/bin:/bin"),
        "an unrelated variable (PATH) must not be registered as a secret: {redacted}"
    );
}

/// The fail-closed path of [`redact_text_checked`]: ordinary text redacts to `Some`, while
/// text whose first-pass result is not stable under a second pass (here a registered
/// secret equal to its own label, so the marker re-contains the secret) yields `None`,
/// which makes the caller exclude the file instead of emitting it.
#[test]
fn redact_text_checked_fails_closed_when_the_second_pass_still_changes_the_text() {
    let policy = RedactionPolicy::default();
    let mut redactor = Redactor::from_policy(&policy).expect("redactor");
    assert_eq!(
        redact_text_checked(&redactor, "nothing sensitive here").as_deref(),
        Some(b"nothing sensitive here".as_slice())
    );
    redactor
        .register_secret("selfreferent", &spot_secrets::SecretString::new("selfreferent"))
        .expect("register");
    assert!(
        redact_text_checked(&redactor, "value selfreferent leaked").is_none(),
        "an unstable redaction must be excluded, not emitted"
    );
}
