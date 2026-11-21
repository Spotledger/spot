//! WP-DX-01.T3: the developer commands of the `spot` binary (`app new|check|pack|verify|inspect`,
//! `doctype add`, `controller add`, `script add`). They run as processes in a temporary working
//! directory, so what is checked is what a developer gets: exit codes, output and files. The
//! end-to-end case needs PostgreSQL (`SPOT_TEST_PG_URL`) and is skipped with a printed reason
//! when it is unset.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use serde_json::Value as Json;
use spot_storage_contract::RecordId;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A scratch working directory, removed on drop.
struct Dir(PathBuf);
impl Dir {
    fn new() -> Dir {
        let path = std::env::temp_dir().join(format!("spot_cli_devcmd_{}", RecordId::new_v7()));
        std::fs::create_dir_all(&path).unwrap();
        Dir(path)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `spot ARGS` with `dir` as the working directory and no configuration root at all.
fn spot(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spot"))
        .args(args)
        .current_dir(dir)
        .env_remove("SPOT_ROOT")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn code(output: &Output) -> i32 {
    output.status.code().unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Both streams, for "nothing secret anywhere".
fn everything(output: &Output) -> String {
    format!("{}{}", stdout(output), stderr(output))
}

#[track_caller]
fn expect_exit(output: &Output, wanted: i32) {
    assert_eq!(
        code(output),
        wanted,
        "stdout: {}\nstderr: {}",
        stdout(output),
        stderr(output)
    );
}

/// Every regular file under `dir`, as sorted relative paths.
fn files_under(dir: &Path) -> Vec<String> {
    fn walk(base: &Path, here: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(here).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                walk(base, &path, out);
            } else {
                out.push(
                    path.strip_prefix(base)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// A directory with a generated app `demo` (native controller and script included).
fn generated() -> Dir {
    let dir = Dir::new();
    let out = spot(
        dir.path(),
        &["app", "new", "demo", "--with-controller", "--with-script"],
    );
    expect_exit(&out, 0);
    dir
}

// --- the scaffolding commands ------------------------------------------------------------------

#[test]
fn help_of_spot_lists_the_developer_commands() {
    let dir = Dir::new();
    let out = spot(dir.path(), &["--help"]);
    expect_exit(&out, 0);
    let help = stdout(&out);
    for name in ["app", "doctype", "controller", "script"] {
        assert!(
            help.lines().any(|l| l.trim_start().starts_with(name)),
            "{name} is listed: {help}"
        );
    }
    let app = spot(dir.path(), &["app", "--help"]);
    expect_exit(&app, 0);
    let text = stdout(&app);
    for name in ["new", "check", "pack", "verify", "inspect"] {
        assert!(text.contains(name), "app {name} is listed: {text}");
    }
}

#[test]
fn app_doctype_controller_and_script_write_their_files_and_name_the_next_command() {
    let dir = Dir::new();
    let created = spot(dir.path(), &["app", "new", "demo", "--with-controller"]);
    expect_exit(&created, 0);
    let text = stdout(&created);
    assert!(text.contains("wrote "), "{text}");
    assert!(text.contains("spot app check app"), "{text}");
    for expected in [
        "app/app.toml",
        "app/app.wasm",
        "app/doctypes/note.toml",
        "app/permissions/grants.toml",
    ] {
        assert!(
            dir.path().join(expected).is_file(),
            "{expected} exists after app new"
        );
    }

    let doctype = spot(
        dir.path(),
        &["doctype", "add", "demo", "Sales Invoice", "--force"],
    );
    expect_exit(&doctype, 0);
    assert!(dir.path().join("app/doctypes/sales_invoice.toml").is_file());

    let controller = spot(
        dir.path(),
        &["controller", "add", "demo", "sales_invoice", "--force"],
    );
    expect_exit(&controller, 0);
    assert!(dir.path().join("src/sales_invoice_controller.rs").is_file());

    let script = spot(dir.path(), &["script", "add", "demo", "greet", "--force"]);
    expect_exit(&script, 0);
    assert!(dir.path().join("app/scripts/greet.ts").is_file());

    // The generated tree is clean.
    let check = spot(dir.path(), &["app", "check", "app"]);
    expect_exit(&check, 0);
    assert!(stdout(&check).contains("ok"), "{}", stdout(&check));
}

#[test]
fn a_dry_run_writes_nothing_and_json_output_is_the_scaffolders() {
    let dir = Dir::new();
    let out = spot(dir.path(), &["app", "new", "demo", "--dry-run", "--json"]);
    expect_exit(&out, 0);
    let value: Json = serde_json::from_str(stdout(&out).trim()).unwrap();
    assert!(value.is_object() || value.is_array(), "{value}");
    assert!(
        files_under(dir.path()).is_empty(),
        "a dry run writes nothing"
    );

    let wrote = spot(dir.path(), &["app", "new", "demo", "--json"]);
    expect_exit(&wrote, 0);
    let value: Json = serde_json::from_str(stdout(&wrote).trim()).unwrap();
    assert!(value["files"].is_array(), "{value}");
    assert!(value["bytes"].is_number(), "{value}");
    assert!(
        value["next"]
            .as_str()
            .unwrap()
            .starts_with("spot app check"),
        "{value}"
    );
}

// --- check, pack, inspect, verify -------------------------------------------------------------------

#[test]
fn check_is_zero_on_a_generated_tree_and_two_with_the_one_problem_named() {
    let dir = generated();
    let ok = spot(dir.path(), &["app", "check", "app"]);
    expect_exit(&ok, 0);
    let json = spot(dir.path(), &["app", "check", "app", "--json"]);
    expect_exit(&json, 0);
    assert!(serde_json::from_str::<Json>(stdout(&json).trim()).is_ok());

    std::fs::remove_file(dir.path().join("app/app.toml")).unwrap();
    let bad = spot(dir.path(), &["app", "check", "app"]);
    expect_exit(&bad, 2);
    let text = stderr(&bad);
    assert!(text.contains("pack.manifest_missing"), "{text}");
    assert!(text.contains("1 problem:"), "exactly one problem: {text}");
}

#[test]
fn pack_is_deterministic_and_inspect_shows_the_manifest_identity() {
    let dir = generated();
    let first = spot(dir.path(), &["app", "pack", "app", "--out", "one.bundle"]);
    expect_exit(&first, 0);
    let second = spot(dir.path(), &["app", "pack", "app", "--out", "two.bundle"]);
    expect_exit(&second, 0);
    assert_eq!(
        std::fs::read(dir.path().join("one.bundle")).unwrap(),
        std::fs::read(dir.path().join("two.bundle")).unwrap(),
        "packing twice gives equal bytes"
    );
    assert!(
        stdout(&first).contains("packed one.bundle"),
        "{}",
        stdout(&first)
    );

    let inspect = spot(dir.path(), &["app", "inspect", "one.bundle"]);
    expect_exit(&inspect, 0);
    let text = stdout(&inspect);
    assert!(text.contains("app: demo 0.1.0"), "{text}");
    assert!(text.contains("digest: "), "{text}");
    let json = spot(dir.path(), &["app", "inspect", "one.bundle", "--json"]);
    expect_exit(&json, 0);
    let value: Json = serde_json::from_str(stdout(&json).trim()).unwrap();
    assert_eq!(value["app_id"], "demo");
}

#[test]
fn pack_refuses_to_replace_a_bundle_without_force() {
    let dir = generated();
    expect_exit(
        &spot(dir.path(), &["app", "pack", "app", "--out", "one.bundle"]),
        0,
    );
    let again = spot(dir.path(), &["app", "pack", "app", "--out", "one.bundle"]);
    expect_exit(&again, 2);
    assert!(stderr(&again).contains("--force"), "{}", stderr(&again));
    expect_exit(
        &spot(
            dir.path(),
            &["app", "pack", "app", "--out", "one.bundle", "--force"],
        ),
        0,
    );
}

#[test]
fn verify_refuses_an_unsigned_bundle_without_the_explicit_development_switch() {
    let dir = generated();
    expect_exit(
        &spot(dir.path(), &["app", "pack", "app", "--out", "one.bundle"]),
        0,
    );
    let refused = spot(dir.path(), &["app", "verify", "one.bundle"]);
    expect_exit(&refused, 2);
    let text = stderr(&refused);
    assert!(text.contains("no signature"), "{text}");
    assert!(text.contains("--allow-unsigned-development"), "{text}");

    let development = spot(
        dir.path(),
        &[
            "app",
            "verify",
            "one.bundle",
            "--allow-unsigned-development",
        ],
    );
    expect_exit(&development, 0);
    let text = stdout(&development);
    assert!(text.contains("UNSIGNED"), "{text}");
    assert!(text.contains("demo"), "{text}");
}

// --- the exit-code table, one test per row -------------------------------------------------------

#[test]
fn exit_row_done_is_zero() {
    let dir = Dir::new();
    expect_exit(&spot(dir.path(), &["app", "new", "demo"]), 0);
}

#[test]
fn exit_row_a_wrong_argument_at_run_time_is_one() {
    let dir = generated();
    // `check` on something that is not a directory (pack's own "usage").
    let out = spot(dir.path(), &["app", "check", "no-such-dir"]);
    expect_exit(&out, 1);
    assert!(
        stderr(&out).contains("is not a directory"),
        "{}",
        stderr(&out)
    );
    // `verify` with a mode that does not exist.
    expect_exit(
        &spot(
            dir.path(),
            &["app", "verify", "one.bundle", "--mode", "sideways"],
        ),
        1,
    );
}

#[test]
fn exit_row_a_refusal_is_two() {
    let dir = Dir::new();
    // A name the scaffolder refuses (scaffold 3).
    expect_exit(&spot(dir.path(), &["app", "new", "Not A Name"]), 2);
    // A check with problems (pack 1).
    std::fs::create_dir_all(dir.path().join("empty")).unwrap();
    expect_exit(&spot(dir.path(), &["app", "check", "empty"]), 2);
    // A doctype for an app that is not there (scaffold 3, `scaffold.not_an_app`).
    expect_exit(&spot(dir.path(), &["doctype", "add", "demo", "Thing"]), 2);
}

#[test]
fn exit_row_a_file_that_cannot_be_read_or_written_is_three() {
    let dir = Dir::new();
    // pack's "cannot read" (its exit 2 for I/O).
    expect_exit(&spot(dir.path(), &["app", "inspect", "missing.bundle"]), 3);
    // scaffold.io: the output directory is a regular file.
    std::fs::write(dir.path().join("plain"), "x").unwrap();
    let out = spot(dir.path(), &["app", "new", "demo", "--out", "plain"]);
    expect_exit(&out, 3);
}

#[test]
fn clap_parse_errors_keep_clap_exit_code_two() {
    let dir = Dir::new();
    expect_exit(&spot(dir.path(), &["app", "new"]), 2);
    expect_exit(&spot(dir.path(), &["app", "pack"]), 2);
}

// --- hostile arguments ------------------------------------------------------------------------------

#[test]
fn hostile_arguments_are_refused_and_nothing_is_written() {
    // A name with `../`.
    let dir = Dir::new();
    let out = spot(dir.path(), &["app", "new", "../escape"]);
    expect_exit(&out, 2);
    assert!(files_under(dir.path()).is_empty());

    // An absolute --out.
    let elsewhere = Dir::new();
    let absolute = elsewhere.path().to_string_lossy().into_owned();
    let out = spot(dir.path(), &["app", "new", "demo", "--out", &absolute]);
    expect_exit(&out, 2);
    assert!(files_under(elsewhere.path()).is_empty());
    assert!(files_under(dir.path()).is_empty());

    // `..` in --out.
    let out = spot(dir.path(), &["app", "new", "demo", "--out", "../up"]);
    expect_exit(&out, 2);

    // An existing file without --force: it is kept and nothing else appears.
    std::fs::write(dir.path().join("README.md"), "mine").unwrap();
    let out = spot(dir.path(), &["app", "new", "demo"]);
    expect_exit(&out, 2);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("README.md")).unwrap(),
        "mine"
    );
    assert_eq!(files_under(dir.path()), ["README.md"]);

    // A pack --out that is absolute or climbs.
    let app = generated();
    let out = spot(
        app.path(),
        &["app", "pack", "app", "--out", &format!("{absolute}/b")],
    );
    expect_exit(&out, 2);
    assert!(files_under(elsewhere.path()).is_empty());
    expect_exit(
        &spot(app.path(), &["app", "pack", "app", "--out", "../b"]),
        2,
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_output_directory_is_refused_and_nothing_is_written() {
    let dir = Dir::new();
    let target = Dir::new();
    std::os::unix::fs::symlink(target.path(), dir.path().join("linked")).unwrap();
    let out = spot(dir.path(), &["app", "new", "demo", "--out", "linked"]);
    expect_exit(&out, 2);
    assert!(
        stderr(&out).contains("symlink"),
        "the refusal names the symlink: {}",
        stderr(&out)
    );
    assert!(files_under(target.path()).is_empty());

    // A symlinked bundle path for pack.
    let app = generated();
    std::os::unix::fs::symlink(target.path().join("b.bundle"), app.path().join("b.bundle"))
        .unwrap();
    let out = spot(
        app.path(),
        &["app", "pack", "app", "--out", "b.bundle", "--force"],
    );
    expect_exit(&out, 2);
    assert!(files_under(target.path()).is_empty());
}

#[cfg(unix)]
#[test]
fn a_key_file_that_others_can_read_is_refused_and_never_printed() {
    use std::os::unix::fs::PermissionsExt;
    let dir = generated();
    // A recognisable value: it must appear in no output.
    let secret = "SECRETSEEDSECRETSEEDSECRETSEEDSECRETSEEDSECR=";
    let key = dir.path().join("publisher.key");
    std::fs::write(&key, format!("{secret}\n")).unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(dir.path().join("publisher.cert"), "{}").unwrap();
    let out = spot(
        dir.path(),
        &[
            "app",
            "pack",
            "app",
            "--out",
            "signed.bundle",
            "--sign",
            "publisher.key",
            "--certificate",
            "publisher.cert",
        ],
    );
    expect_exit(&out, 2);
    let text = everything(&out);
    assert!(text.contains("mode 0600"), "{text}");
    assert!(
        !text.contains(secret),
        "the key content is never printed: {text}"
    );
    assert!(
        !dir.path().join("signed.bundle").exists(),
        "nothing is written when the key is refused"
    );

    // Tightened to 0600 the key file is no longer the problem: the next refusal is about the
    // certificate, still without the key.
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let out = spot(
        dir.path(),
        &[
            "app",
            "pack",
            "app",
            "--out",
            "signed.bundle",
            "--sign",
            "publisher.key",
            "--certificate",
            "publisher.cert",
        ],
    );
    assert!(!everything(&out).contains(secret));
    assert!(
        !everything(&out).contains("mode 0600"),
        "{}",
        everything(&out)
    );
}

// --- end to end with PostgreSQL ----------------------------------------------------------------------

/// `spot app new` from an empty directory, installed on a site with `install-app --apply`, then a
/// document of the generated DocType created as the Administrator.
///
/// The scaffolder writes the layout `install-app <dir>` reads (`app/app.toml`, WP-DX-01.T4), so the
/// test only places the generated app in the root.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scaffolded_app_installs_and_takes_a_document() {
    let Some(env) = common::Env::new("").await else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let first_secret = env.site();

    let work = env.root.0.join("work");
    std::fs::create_dir_all(&work).unwrap();
    expect_exit(&spot(&work, &["app", "new", "demo"]), 0);
    expect_exit(&spot(&work, &["app", "check", "app"]), 0);
    let placed = env.root.0.join("demo");
    common::copy_dir(&work.join("app"), &placed);
    env.write_common(&["'sample'", "'demo'"], "");

    let out = common::run(
        &env.root.0,
        &["install-app", "demo", "--site", &env.site, "--apply"],
        None,
    );
    assert!(out.status.success(), "{}{}", stdout(&out), stderr(&out));
    let listed = common::ok(&env.root.0, &["list-apps", "--site", &env.site]);
    assert!(listed.contains("demo"), "{listed}");

    let started = common::start(&env).await;
    let admin = common::admin(started.addr, &env.site, &first_secret).await;
    let created = common::http(
        started.addr,
        &env.site,
        Some(&admin),
        "POST",
        "/api/v1/resource/note",
        &[],
        Some(serde_json::json!({"title": "First note"})),
    )
    .await;
    common::expect(&created, 201);
    let id = created.body["data"]["id"].as_str().unwrap().to_owned();
    let read = common::http(
        started.addr,
        &env.site,
        Some(&admin),
        "GET",
        &format!("/api/v1/resource/note/{id}"),
        &[],
        None,
    )
    .await;
    common::expect(&read, 200);
    assert_eq!(read.body["data"]["title"], "First note");
    started.finish().await;
}
