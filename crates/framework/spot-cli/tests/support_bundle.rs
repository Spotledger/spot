//! `spot support bundle|consent|help-code` (WP-PRD-13.T3): the security-focused tests the
//! brief names, driven over the registry without a process (the technique of
//! `tests/commands_registry.rs`), plus a binary-level smoke test.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use clap::ArgMatches;
use spot_core::ManualClock;
use time::macros::datetime;

/// The registry module, driven without a process (same technique as
/// `tests/commands_registry.rs`): `commands::COMMANDS` includes this task's `support`
/// registration because it is a real file under `src/commands/`.
#[path = "../src/commands/mod.rs"]
mod commands;

use commands::{CommandCtx, Exit};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Sink {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

fn ctx(root: &std::path::Path) -> (CommandCtx, Sink, Sink) {
    let (out, err) = (Sink::default(), Sink::default());
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-08 12:00:00 UTC)));
    (
        CommandCtx::new(
            root.to_path_buf(),
            clock,
            Box::new(out.clone()),
            Box::new(err.clone()),
        ),
        out,
        err,
    )
}

fn temp_root(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "spot-cli-support-bundle-test-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    std::fs::create_dir_all(dir.join("sites").join("acme")).unwrap();
    dir
}

/// Plants a canary in every planted-secret category the brief names: a bearer-ish token, a
/// password in a connection URL, an AWS key id, and an email address.
const TOKEN_CANARY: &str = "ghp_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
const URL_PASSWORD_CANARY: &str = "sekrit-admin-password";
const AWS_KEY_CANARY: &str = "AKIAABCDEFGHIJKLMNOP";
const EMAIL_CANARY: &str = "alice@example.com";

fn plant_secrets(root: &std::path::Path) {
    std::fs::write(
        root.join("common.toml"),
        format!(
            "apps = [\"platform\"]\n\n[postgres]\nadmin_url = \"postgres://admin:{URL_PASSWORD_CANARY}@db.internal:5432/postgres\"\n\n[services.mail.smtp]\nhost = \"smtp.internal\"\nport = 587\nsecurity = \"starttls\"\nfrom = \"ops@example.com\"\npassword = \"{URL_PASSWORD_CANARY}\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("sites").join("acme").join("site.toml"),
        format!(
            "site = \"acme\"\ndatabase = \"acme\"\n# token {TOKEN_CANARY}\n# aws {AWS_KEY_CANARY}\n# contact {EMAIL_CANARY}\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("sites").join("acme").join("identity-audit.log"),
        format!("login ok user={EMAIL_CANARY} key={AWS_KEY_CANARY} token={TOKEN_CANARY}\n"),
    )
    .unwrap();
}

/// Reads every entry of the ZIP at `path` as `(name, text)`.
fn read_zip(path: &std::path::Path) -> Vec<(String, String)> {
    let file = std::fs::File::open(path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut out = Vec::new();
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).unwrap();
        let name = entry.name().unwrap().into_owned();
        let mut text = String::new();
        std::io::Read::read_to_string(&mut entry, &mut text).unwrap();
        out.push((name, text));
    }
    out
}

async fn run(args: &[&str], root: &std::path::Path) -> (Exit, String, String) {
    let command = commands::augment(
        clap::Command::new("spot").subcommand_required(true),
        commands::COMMANDS,
    )
    .unwrap();
    let mut full = vec!["spot"];
    full.extend_from_slice(args);
    let matches: ArgMatches = command.try_get_matches_from(full).unwrap();
    let (mut ctx, out, err) = ctx(root);
    let exit = commands::run_registered(commands::COMMANDS, &matches, &mut ctx)
        .await
        .unwrap();
    (exit, out.text(), err.text())
}

// --- the bundle itself ------------------------------------------------------------------

#[tokio::test]
async fn planted_secrets_never_reach_the_bundle() {
    let root = temp_root("secrets");
    plant_secrets(&root);
    let out_path = root.parent().unwrap().join("bundle-secrets.zip");
    let (exit, stdout, stderr) = run(
        &["support", "bundle", "--out", out_path.to_str().unwrap()],
        &root,
    )
    .await;
    assert_eq!(exit, Exit::Ok, "stderr: {stderr}");
    assert!(stdout.contains("bytes"), "{stdout}");

    let entries = read_zip(&out_path);
    assert!(!entries.is_empty());
    let whole = entries
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    for canary in [
        TOKEN_CANARY,
        URL_PASSWORD_CANARY,
        AWS_KEY_CANARY,
        EMAIL_CANARY,
    ] {
        assert!(
            !whole.contains(canary),
            "{canary} leaked into the bundle: {whole}"
        );
    }
    // The shape survives even though the secrets do not: a reviewer can still tell what
    // kind of thing was redacted.
    assert!(whole.contains("[redacted"));
    let _ = std::fs::remove_file(&out_path);
}

#[tokio::test]
async fn the_bundle_lists_every_file_and_its_byte_count() {
    let root = temp_root("listing");
    plant_secrets(&root);
    let out_path = root.parent().unwrap().join("bundle-listing.zip");
    let (exit, stdout, _) = run(
        &["support", "bundle", "--out", out_path.to_str().unwrap()],
        &root,
    )
    .await;
    assert_eq!(exit, Exit::Ok);
    assert!(stdout.contains("versions.txt:"));
    assert!(stdout.contains("config/common.toml:"));
    assert!(stdout.contains("config/sites/acme/site.toml:"));
    assert!(stdout.contains("health/summary.txt:"));
    assert!(stdout.contains("logs/sites/acme/identity-audit.log:"));
    assert!(stdout.contains("bytes"));
    assert!(stdout.contains("wrote "));
    let _ = std::fs::remove_file(&out_path);
}

#[tokio::test]
async fn the_bundle_contains_no_database_dump_or_document_data() {
    let root = temp_root("nodb");
    plant_secrets(&root);
    let out_path = root.parent().unwrap().join("bundle-nodb.zip");
    let (exit, _, _) = run(
        &["support", "bundle", "--out", out_path.to_str().unwrap()],
        &root,
    )
    .await;
    assert_eq!(exit, Exit::Ok);
    let entries = read_zip(&out_path);
    // Only the four documented categories ever appear; nothing named like a database dump
    // or a document record.
    for (name, _) in &entries {
        assert!(
            name.starts_with("versions")
                || name.starts_with("config/")
                || name.starts_with("health/")
                || name.starts_with("logs/"),
            "unexpected bundle entry: {name}"
        );
    }
    let _ = std::fs::remove_file(&out_path);
}

#[tokio::test]
async fn an_include_list_narrows_the_categories() {
    let root = temp_root("include");
    plant_secrets(&root);
    let out_path = root.parent().unwrap().join("bundle-include.zip");
    let (exit, _, _) = run(
        &[
            "support",
            "bundle",
            "--out",
            out_path.to_str().unwrap(),
            "--include",
            "versions",
        ],
        &root,
    )
    .await;
    assert_eq!(exit, Exit::Ok);
    let entries = read_zip(&out_path);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].0, "versions.txt");
    let _ = std::fs::remove_file(&out_path);
}

#[tokio::test]
async fn an_unknown_include_category_is_a_usage_refusal() {
    let root = temp_root("bad-include");
    let out_path = root.parent().unwrap().join("bundle-bad.zip");
    let (exit, _, stderr) = run(
        &[
            "support",
            "bundle",
            "--out",
            out_path.to_str().unwrap(),
            "--include",
            "nope",
        ],
        &root,
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert!(stderr.contains("bad_include"), "{stderr}");
    assert!(!out_path.exists());
}

#[tokio::test]
async fn the_bundle_refuses_to_write_inside_the_data_directory() {
    let root = temp_root("datadir");
    let inside = root.join("sites").join("dump.zip");
    let (exit, _, stderr) = run(
        &["support", "bundle", "--out", inside.to_str().unwrap()],
        &root,
    )
    .await;
    assert_eq!(exit, Exit::Refused);
    assert!(stderr.contains("refused_data_dir"), "{stderr}");
    assert!(!inside.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn the_bundle_file_is_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_root("perm");
    plant_secrets(&root);
    let out_path = root.parent().unwrap().join("bundle-perm.zip");
    let (exit, _, _) = run(
        &["support", "bundle", "--out", out_path.to_str().unwrap()],
        &root,
    )
    .await;
    assert_eq!(exit, Exit::Ok);
    let mode = std::fs::metadata(&out_path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let _ = std::fs::remove_file(&out_path);
}

#[tokio::test]
async fn since_skips_an_old_log_file_but_keeps_other_categories() {
    let root = temp_root("since");
    plant_secrets(&root);
    // Back-date the log file well past any `--since` window this test uses.
    let log = root.join("sites").join("acme").join("identity-audit.log");
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(10 * 24 * 3600);
    let file = std::fs::File::open(&log).unwrap();
    file.set_modified(old).unwrap();

    let out_path = root.parent().unwrap().join("bundle-since.zip");
    let (exit, _, _) = run(
        &[
            "support",
            "bundle",
            "--out",
            out_path.to_str().unwrap(),
            "--since",
            "24h",
        ],
        &root,
    )
    .await;
    assert_eq!(exit, Exit::Ok);
    let entries = read_zip(&out_path);
    assert!(
        entries.iter().all(|(n, _)| !n.starts_with("logs/")),
        "an old log file was not skipped: {entries:?}"
    );
    assert!(entries.iter().any(|(n, _)| n == "versions.txt"));
    let _ = std::fs::remove_file(&out_path);
}

#[tokio::test]
async fn a_bad_since_value_is_a_usage_refusal() {
    let root = temp_root("bad-since");
    let out_path = root.parent().unwrap().join("bundle-bad-since.zip");
    let (exit, _, stderr) = run(
        &[
            "support",
            "bundle",
            "--out",
            out_path.to_str().unwrap(),
            "--since",
            "24x",
        ],
        &root,
    )
    .await;
    assert_eq!(exit, Exit::Usage);
    assert!(stderr.contains("bad_since"), "{stderr}");
}

// --- consent ------------------------------------------------------------------------------

#[tokio::test]
async fn consent_defaults_to_off_and_round_trips_on_and_off() {
    let root = temp_root("consent");
    let (exit, stdout, _) = run(&["support", "consent", "status"], &root).await;
    assert_eq!(exit, Exit::Ok);
    assert!(stdout.contains("crash reporting: ask"));
    assert!(stdout.contains("telemetry: ask"));

    let (exit, stdout, _) = run(&["support", "consent", "on"], &root).await;
    assert_eq!(exit, Exit::Ok);
    assert!(stdout.contains("now on"));
    let (exit, stdout, _) = run(&["support", "consent", "status"], &root).await;
    assert_eq!(exit, Exit::Ok);
    assert!(stdout.contains("crash reporting: on"));
    assert!(stdout.contains("telemetry: on"));

    let (exit, stdout, _) = run(&["support", "consent", "off"], &root).await;
    assert_eq!(exit, Exit::Ok);
    assert!(stdout.contains("now off"));
    let (exit, stdout, _) = run(&["support", "consent", "status"], &root).await;
    assert_eq!(exit, Exit::Ok);
    assert!(stdout.contains("crash reporting: off"));
    assert!(stdout.contains("telemetry: off"));
}

#[cfg(unix)]
#[tokio::test]
async fn the_consent_file_is_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_root("consent-perm");
    let (exit, _, _) = run(&["support", "consent", "on"], &root).await;
    assert_eq!(exit, Exit::Ok);
    let mode = std::fs::metadata(root.join("support-consent.toml"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

// --- help-code ------------------------------------------------------------------------------

#[tokio::test]
async fn help_code_explains_a_known_code_offline() {
    let root = temp_root("help-known");
    let (exit, stdout, _) = run(&["support", "help-code", "login.store_failed"], &root).await;
    assert_eq!(exit, Exit::Ok);
    assert!(stdout.contains("what to do"));
    assert!(stdout.to_lowercase().contains("backend"));
}

#[tokio::test]
async fn help_code_refuses_a_code_with_no_offline_help() {
    let root = temp_root("help-unknown");
    let (exit, _, stderr) = run(&["support", "help-code", "not.a.real.code"], &root).await;
    assert_eq!(exit, Exit::Usage);
    assert!(stderr.contains("help_missing"), "{stderr}");
}

// --- security hardening (review) -------------------------------------------------

#[tokio::test]
async fn the_bundle_refuses_to_overwrite_an_existing_file() {
    let root = temp_root("overwrite");
    plant_secrets(&root);
    let out_path = root.parent().unwrap().join("bundle-overwrite.zip");
    std::fs::write(&out_path, b"do-not-touch").unwrap();
    let (exit, _, stderr) = run(
        &["support", "bundle", "--out", out_path.to_str().unwrap()],
        &root,
    )
    .await;
    assert_ne!(exit, Exit::Ok, "an existing path must never be overwritten");
    assert!(stderr.contains("out_exists"), "{stderr}");
    assert_eq!(
        std::fs::read(&out_path).unwrap(),
        b"do-not-touch",
        "the existing file's content must be untouched"
    );
    let _ = std::fs::remove_file(&out_path);
}

#[cfg(unix)]
#[tokio::test]
async fn an_out_symlink_is_refused_and_the_target_is_untouched() {
    use std::os::unix::fs::symlink;
    let root = temp_root("out-symlink");
    plant_secrets(&root);
    let parent = root.parent().unwrap();
    let target = parent.join("bundle-symlink-target.txt");
    std::fs::write(&target, b"the real file, elsewhere").unwrap();
    let out_link = parent.join("bundle-symlink-out.zip");
    let _ = std::fs::remove_file(&out_link);
    symlink(&target, &out_link).unwrap();

    let (exit, _, _) = run(
        &["support", "bundle", "--out", out_link.to_str().unwrap()],
        &root,
    )
    .await;
    assert_ne!(
        exit,
        Exit::Ok,
        "writing through a symlinked --out must never succeed"
    );
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"the real file, elsewhere",
        "the symlink target must not be truncated or overwritten"
    );
    assert!(
        std::fs::symlink_metadata(&out_link)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false),
        "the symlink itself must survive, not be replaced by a regular file"
    );
    let _ = std::fs::remove_file(&out_link);
    let _ = std::fs::remove_file(&target);
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_log_is_never_copied_into_the_bundle() {
    use std::os::unix::fs::symlink;
    let root = temp_root("log-symlink");
    plant_secrets(&root);
    let outside_secret = "the-outside-log-must-never-be-in-the-bundle";
    let outside_dir = root.parent().unwrap().join(format!(
        "spot-cli-support-bundle-outside-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&outside_dir).unwrap();
    let outside_log = outside_dir.join("outside.log");
    std::fs::write(&outside_log, format!("{outside_secret}\n")).unwrap();
    let link = root.join("sites").join("acme").join("link.log");
    let _ = std::fs::remove_file(&link);
    symlink(&outside_log, &link).unwrap();

    let out_path = root.parent().unwrap().join("bundle-log-symlink.zip");
    let (exit, _, _) = run(
        &["support", "bundle", "--out", out_path.to_str().unwrap()],
        &root,
    )
    .await;
    assert_eq!(exit, Exit::Ok);
    let entries = read_zip(&out_path);
    assert!(
        entries.iter().all(|(name, _)| !name.contains("link.log")),
        "a symlinked log must never be walked into the bundle: {entries:?}"
    );
    let whole = entries
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !whole.contains(outside_secret),
        "the outside log's content leaked through the symlink: {whole}"
    );
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_dir_all(&outside_dir);
}

#[tokio::test]
async fn hex_and_base64_secret_forms_planted_in_config_never_leak() {
    let root = temp_root("hex-base64");
    // A 32-byte hex secret (embedded in the connection URL) and a base64-looking secret,
    // planted as the specific config fields `spot support bundle` registers as known
    // secrets (`postgres.admin_url` whole and `services.mail.smtp.password`), then
    // duplicated verbatim into a log file as free text the way an application log might
    // echo a connection failure.
    const HEX_SECRET: &str = "4e1c9f2b6a7d3c08f1e5b9a2d6c3f708a1b2c3d4e5f60718293a4b5c6d7e8f9";
    const BASE64_SECRET: &str = "QWxhZGRpbjpvcGVuU2VzYW1lMTIzNDU2Nzg5MA==";
    std::fs::write(
        root.join("common.toml"),
        format!(
            "apps = [\"platform\"]\n\n[postgres]\nadmin_url = \"postgres://admin:{HEX_SECRET}@db.internal:5432/postgres\"\n\n[services.mail.smtp]\nhost = \"smtp.internal\"\nport = 587\nsecurity = \"starttls\"\nfrom = \"ops@example.com\"\npassword = \"{BASE64_SECRET}\"\n"
        ),
    )
    .unwrap();
    std::fs::write(
        root.join("sites").join("acme").join("identity-audit.log"),
        format!(
            "connection refused using postgres://admin:{HEX_SECRET}@db.internal:5432/postgres and smtp password {BASE64_SECRET}\n"
        ),
    )
    .unwrap();

    let out_path = root.parent().unwrap().join("bundle-hex-base64.zip");
    let (exit, _, stderr) = run(
        &["support", "bundle", "--out", out_path.to_str().unwrap()],
        &root,
    )
    .await;
    assert_eq!(exit, Exit::Ok, "stderr: {stderr}");
    let entries = read_zip(&out_path);
    let whole = entries
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!whole.contains(HEX_SECRET), "hex secret leaked: {whole}");
    assert!(
        !whole.contains(BASE64_SECRET),
        "base64 secret leaked: {whole}"
    );
    assert!(whole.contains("[redacted"), "{whole}");
    let _ = std::fs::remove_file(&out_path);
}

// --- the module's own unit coverage (name parsing, normalisation, the config allow list,
// the log walk's symlink and cap behaviour, the whole-bundle caps, and the env-var secret
// plant) is in the command file itself (`src/commands/support_bundle.rs`); this file only
// drives it as a user would.

// --- log free-text redaction (config values, hex/base64 forms, IP addresses) ---------------

const JWT_SECRET_CANARY: &str = "jwt-signing-secret-Zx9Qm2LpV7";

fn hex_of(value: &str, upper: bool) -> String {
    let s: String = value.bytes().map(|b| format!("{b:02x}")).collect();
    if upper { s.to_ascii_uppercase() } else { s }
}

/// Standard padded base64, hand-rolled because this crate has no `base64` dependency.
fn b64_of(value: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in value.as_bytes().chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |a, (i, b)| a | (u32::from(*b) << (16 - 8 * i)));
        for k in 0..4 {
            if k <= chunk.len() {
                out.push(T[((n >> (18 - 6 * k)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

async fn bundle_text(label: &str, log_line: &str) -> (String, Vec<(String, String)>) {
    let root = temp_root(label);
    std::fs::write(
        root.join("common.toml"),
        format!("apps = [\"platform\"]\n\n[security]\njwt_secret = \"{JWT_SECRET_CANARY}\"\n"),
    )
    .unwrap();
    std::fs::write(
        root.join("sites").join("acme").join("app.log"),
        format!("{log_line}\n"),
    )
    .unwrap();
    let out_path = root.parent().unwrap().join(format!("bundle-{label}.zip"));
    let (exit, _, stderr) = run(
        &["support", "bundle", "--out", out_path.to_str().unwrap()],
        &root,
    )
    .await;
    assert_eq!(exit, Exit::Ok, "stderr: {stderr}");
    let entries = read_zip(&out_path);
    let whole = entries
        .iter()
        .map(|(_, t)| t.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let _ = std::fs::remove_file(&out_path);
    (whole, entries)
}

#[tokio::test]
async fn a_config_secret_repeated_in_a_log_line_is_redacted() {
    let (whole, entries) = bundle_text(
        "cfglog",
        &format!("boot ok signing with {JWT_SECRET_CANARY} done"),
    )
    .await;
    assert!(!whole.contains(JWT_SECRET_CANARY), "{whole}");
    // positive control: the log file is in the bundle and the redactor fired on the value.
    let log = entries
        .iter()
        .find(|(n, _)| n.ends_with("app.log"))
        .unwrap();
    assert!(log.1.contains("[redacted:"), "{}", log.1);
    assert!(log.1.contains("boot ok signing with"), "{}", log.1);
}

#[tokio::test]
async fn hex_and_base64_forms_of_a_config_secret_in_a_log_are_redacted() {
    let line = format!(
        "a={} b={} c={}",
        hex_of(JWT_SECRET_CANARY, false),
        hex_of(JWT_SECRET_CANARY, true),
        b64_of(JWT_SECRET_CANARY)
    );
    let (whole, _) = bundle_text("encoded", &line).await;
    assert!(
        !whole.contains(&hex_of(JWT_SECRET_CANARY, false)),
        "{whole}"
    );
    assert!(!whole.contains(&hex_of(JWT_SECRET_CANARY, true)), "{whole}");
    assert!(!whole.contains(&b64_of(JWT_SECRET_CANARY)), "{whole}");
}

#[tokio::test]
async fn ip_addresses_in_a_log_are_redacted_but_loopback_stays() {
    let (whole, _) = bundle_text(
        "ips",
        "from 203.0.113.77:51234 via 2001:db8::7334 local 127.0.0.1 ::1 0.0.0.0 mod std::io v1.2.3",
    )
    .await;
    assert!(!whole.contains("203.0.113.77"), "{whole}");
    assert!(!whole.contains("2001:db8::7334"), "{whole}");
    assert!(whole.contains("[redacted:ip]:51234"), "{whole}");
    assert!(whole.contains("127.0.0.1"), "{whole}");
    assert!(whole.contains("::1"), "{whole}");
    assert!(whole.contains("0.0.0.0"), "{whole}");
    assert!(whole.contains("std::io"), "{whole}");
    assert!(whole.contains("v1.2.3"), "{whole}");
}
