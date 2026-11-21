//! WP-DX-01.T4: `spot app new` writes the layout `spot install-app` reads (`app/app.toml`,
//! `app/doctypes/`, `app/permissions/`), and a generated app passes `spot app check`, packs,
//! verifies and installs without an edit. The commands run as processes in a temporary working
//! directory; the end-to-end case needs PostgreSQL (`SPOT_TEST_PG_URL`) and is skipped with a
//! printed reason when it is unset.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use serde_json::Value as Json;
use spot_storage_contract::RecordId;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A scratch directory, removed on drop.
struct Dir(PathBuf);
impl Dir {
    fn new() -> Dir {
        let path = std::env::temp_dir().join(format!("spot_cli_layout_{}", RecordId::new_v7()));
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

/// `spot ARGS` with `dir` as the working directory, no configuration root, and `tmp` as the
/// temporary directory (so the staging copies the commands make can be counted afterwards).
fn spot_in(dir: &Path, tmp: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spot"))
        .args(args)
        .current_dir(dir)
        .env_remove("SPOT_ROOT")
        .env("TMPDIR", tmp)
        .env("TMP", tmp)
        .env("TEMP", tmp)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn spot(dir: &Path, args: &[&str]) -> Output {
    spot_in(dir, &std::env::temp_dir(), args)
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[track_caller]
fn expect_exit(output: &Output, wanted: i32) {
    assert_eq!(
        output.status.code().unwrap(),
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
            if entry.file_type().unwrap().is_dir() {
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

fn generated() -> Dir {
    let dir = Dir::new();
    expect_exit(&spot(dir.path(), &["app", "new", "demo"]), 0);
    dir
}

#[test]
fn new_writes_the_installable_layout_and_not_the_packers_file_name() {
    let dir = generated();
    for expected in [
        "app/app.toml",
        "app/app.wasm",
        "app/doctypes/note.toml",
        "app/permissions/grants.toml",
    ] {
        assert!(dir.path().join(expected).is_file(), "{expected} exists");
    }
    let files = files_under(dir.path());
    assert!(
        !files.iter().any(|f| f.ends_with("manifest.toml")),
        "the old manifest name is not written: {files:?}"
    );
    let readme = std::fs::read_to_string(dir.path().join("README.md")).unwrap();
    assert!(readme.contains("app/app.toml"), "{readme}");
    assert!(!readme.contains("app/manifest.toml"), "{readme}");
    // The manifest is the generator's, byte for byte, under its new name.
    let manifest = std::fs::read_to_string(dir.path().join("app/app.toml")).unwrap();
    assert!(manifest.contains("name = \"demo\""), "{manifest}");
}

#[test]
fn a_dry_run_lists_app_toml() {
    let dir = Dir::new();
    let out = spot(dir.path(), &["app", "new", "demo", "--dry-run"]);
    expect_exit(&out, 0);
    let text = stdout(&out);
    assert!(text.contains("app/app.toml"), "{text}");
    assert!(!text.contains("manifest.toml"), "{text}");
    assert!(files_under(dir.path()).is_empty());
    let json = spot(dir.path(), &["app", "new", "demo", "--dry-run", "--json"]);
    expect_exit(&json, 0);
    assert!(stdout(&json).contains("app/app.toml"), "{}", stdout(&json));
}

#[test]
fn a_target_directory_with_files_in_it_is_refused_without_force() {
    let dir = Dir::new();
    std::fs::create_dir_all(dir.path().join("app")).unwrap();
    std::fs::write(dir.path().join("app/keep.txt"), "mine").unwrap();
    let refused = spot(dir.path(), &["app", "new", "demo"]);
    expect_exit(&refused, 2);
    assert!(stderr(&refused).contains("--force"), "{}", stderr(&refused));
    assert_eq!(files_under(dir.path()), ["app/keep.txt"], "nothing written");

    // An empty app directory is fine, and `--force` writes beside what is there.
    std::fs::remove_file(dir.path().join("app/keep.txt")).unwrap();
    expect_exit(&spot(dir.path(), &["app", "new", "demo"]), 0);
    let again = spot(dir.path(), &["app", "new", "demo"]);
    expect_exit(&again, 2);
    expect_exit(&spot(dir.path(), &["app", "new", "demo", "--force"]), 0);
}

#[test]
fn a_generated_app_checks_packs_verifies_and_inspects() {
    let dir = generated();
    let tmp = Dir::new();
    let check = spot_in(dir.path(), tmp.path(), &["app", "check", "app"]);
    expect_exit(&check, 0);
    assert!(stdout(&check).contains("ok"), "{}", stdout(&check));
    let json = spot_in(dir.path(), tmp.path(), &["app", "check", "app", "--json"]);
    expect_exit(&json, 0);
    assert!(serde_json::from_str::<Json>(stdout(&json).trim()).is_ok());

    let first = spot_in(
        dir.path(),
        tmp.path(),
        &["app", "pack", "app", "--out", "one.bundle"],
    );
    expect_exit(&first, 0);
    let second = spot_in(
        dir.path(),
        tmp.path(),
        &["app", "pack", "app", "--out", "two.bundle"],
    );
    expect_exit(&second, 0);
    assert_eq!(
        std::fs::read(dir.path().join("one.bundle")).unwrap(),
        std::fs::read(dir.path().join("two.bundle")).unwrap(),
        "packing twice gives equal bytes"
    );

    let verified = spot_in(
        dir.path(),
        tmp.path(),
        &[
            "app",
            "verify",
            "one.bundle",
            "--allow-unsigned-development",
        ],
    );
    expect_exit(&verified, 0);
    assert!(stdout(&verified).contains("demo"), "{}", stdout(&verified));

    let inspect = spot_in(
        dir.path(),
        tmp.path(),
        &["app", "inspect", "one.bundle", "--json"],
    );
    expect_exit(&inspect, 0);
    let value: Json = serde_json::from_str(stdout(&inspect).trim()).unwrap();
    assert_eq!(value["app_id"], "demo");

    // The app directory is untouched and no staging copy is left behind.
    assert!(!dir.path().join("app/manifest.toml").exists());
    assert!(dir.path().join("app/app.toml").is_file());
    assert!(
        files_under(tmp.path()).is_empty(),
        "no staged file remains: {:?}",
        files_under(tmp.path())
    );
    let leftover = std::fs::read_dir(tmp.path()).unwrap().count();
    assert_eq!(leftover, 0, "no staging directory remains");
}

#[test]
fn a_directory_without_app_toml_is_reported_as_the_packer_reports_it() {
    let dir = generated();
    std::fs::remove_file(dir.path().join("app/app.toml")).unwrap();
    let bad = spot(dir.path(), &["app", "check", "app"]);
    expect_exit(&bad, 2);
    let text = stderr(&bad);
    assert!(text.contains("pack.manifest_missing"), "{text}");
    assert!(text.contains("1 problem:"), "exactly one problem: {text}");
}

#[test]
fn the_old_file_name_still_checks_and_packs_as_before() {
    let dir = generated();
    std::fs::rename(
        dir.path().join("app/app.toml"),
        dir.path().join("app/manifest.toml"),
    )
    .unwrap();
    expect_exit(&spot(dir.path(), &["app", "check", "app"]), 0);
    expect_exit(
        &spot(dir.path(), &["app", "pack", "app", "--out", "old.bundle"]),
        0,
    );
    // An add command on such a project edits the file that is there; `app.toml` does not appear.
    expect_exit(
        &spot(dir.path(), &["doctype", "add", "demo", "Ledger", "--force"]),
        0,
    );
    assert!(dir.path().join("app/manifest.toml").is_file());
    assert!(!dir.path().join("app/app.toml").exists());
}

#[test]
fn both_file_names_in_one_directory_are_refused_and_nothing_is_packed() {
    let dir = generated();
    std::fs::copy(
        dir.path().join("app/app.toml"),
        dir.path().join("app/manifest.toml"),
    )
    .unwrap();
    let check = spot(dir.path(), &["app", "check", "app"]);
    expect_exit(&check, 2);
    assert!(stderr(&check).contains("pack.layout"), "{}", stderr(&check));
    let pack = spot(dir.path(), &["app", "pack", "app", "--out", "x.bundle"]);
    expect_exit(&pack, 2);
    assert!(!dir.path().join("x.bundle").exists());
}

#[cfg(unix)]
#[test]
fn a_symlink_in_the_app_directory_is_refused_by_check() {
    let dir = generated();
    let outside = Dir::new();
    std::fs::write(outside.path().join("secret.txt"), "x").unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("secret.txt"),
        dir.path().join("app/fixtures/link.json"),
    )
    .unwrap();
    let out = spot(dir.path(), &["app", "check", "app"]);
    expect_exit(&out, 2);
    let text = stderr(&out);
    assert!(text.contains("pack.path_escape"), "{text}");
    assert!(text.contains("fixtures/link.json"), "{text}");
}

#[test]
fn the_add_commands_keep_the_manifest_named_app_toml() {
    let dir = generated();
    expect_exit(
        &spot(
            dir.path(),
            &["doctype", "add", "demo", "Sales Invoice", "--force"],
        ),
        0,
    );
    assert!(dir.path().join("app/doctypes/sales_invoice.toml").is_file());
    expect_exit(
        &spot(dir.path(), &["script", "add", "demo", "greet", "--force"]),
        0,
    );
    expect_exit(
        &spot(
            dir.path(),
            &["controller", "add", "demo", "note", "--force"],
        ),
        0,
    );
    assert!(dir.path().join("src/note_controller.rs").is_file());
    assert!(!dir.path().join("app/manifest.toml").exists());
    let manifest = std::fs::read_to_string(dir.path().join("app/app.toml")).unwrap();
    assert!(manifest.contains("demo:greet"), "{manifest}");
    assert!(manifest.contains("demo:validate_note"), "{manifest}");
    expect_exit(&spot(dir.path(), &["app", "check", "app"]), 0);

    // A second `controller add` for the same step still reports the duplicate, not a missing app.
    let again = spot(
        dir.path(),
        &["controller", "add", "demo", "note", "--force"],
    );
    expect_exit(&again, 2);
    assert!(
        stderr(&again).contains("already exists"),
        "{}",
        stderr(&again)
    );
}

#[test]
fn a_host_level_controller_key_is_ignored_by_check_and_blocks_the_rewrites() {
    let dir = generated();
    let path = dir.path().join("app/app.toml");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(
        &path,
        text.replacen("[app]\n", "[app]\ncontroller = \"native:demo\"\n", 1),
    )
    .unwrap();
    // `check` reads the manifest as the packer does: the host-level key is not its business.
    expect_exit(&spot(dir.path(), &["app", "check", "app"]), 0);
    expect_exit(
        &spot(dir.path(), &["app", "pack", "app", "--out", "c.bundle"]),
        0,
    );
    // A command that must rewrite app.toml would drop the key: it refuses and writes nothing.
    let refused = spot(
        dir.path(),
        &["controller", "add", "demo", "note", "--force"],
    );
    expect_exit(&refused, 2);
    assert!(
        stderr(&refused).contains("controller"),
        "{}",
        stderr(&refused)
    );
    assert!(!dir.path().join("src").exists());
    assert_eq!(
        std::fs::read_to_string(&path)
            .unwrap()
            .matches("controller = ")
            .count(),
        1
    );
    // A command that does not touch the manifest still works.
    expect_exit(
        &spot(dir.path(), &["doctype", "add", "demo", "Ledger", "--force"]),
        0,
    );
}

// --- end to end with PostgreSQL ----------------------------------------------------------------------

/// `spot app new`, `check`, `pack`, `verify` (unsigned, development) and then `install-app
/// --apply` of the generated directory exactly as generated, and a document of its DocType.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_generated_app_installs_without_edits() {
    let Some(env) = common::Env::new("").await else {
        eprintln!("skipped: SPOT_TEST_PG_URL is not set");
        return;
    };
    let first_secret = env.site();

    let work = env.root.0.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let tmp = Dir::new();
    expect_exit(&spot_in(&work, tmp.path(), &["app", "new", "demo"]), 0);
    expect_exit(&spot_in(&work, tmp.path(), &["app", "check", "app"]), 0);
    expect_exit(
        &spot_in(
            &work,
            tmp.path(),
            &["app", "pack", "app", "--out", "demo.bundle"],
        ),
        0,
    );
    expect_exit(
        &spot_in(
            &work,
            tmp.path(),
            &[
                "app",
                "verify",
                "demo.bundle",
                "--allow-unsigned-development",
            ],
        ),
        0,
    );

    // The generated directory is placed as it is: no copy, no rename.
    let placed = env.root.0.join("demo");
    common::copy_dir(&work.join("app"), &placed);
    assert!(placed.join("app.toml").is_file());
    assert!(!placed.join("manifest.toml").exists());
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
    started.finish().await;
}
