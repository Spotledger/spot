//! `spot types generate|check` (WP-EXT-09.T2): the TypeScript declarations of a site's effective
//! metadata. The commands read the site's files (the apps it has installed and the recorded
//! runtime changes of `sites/<site>/metadata.json`), never a database, so every case here runs
//! without PostgreSQL: a configuration root with a site file is written to a temporary directory
//! and the `spot` binary cargo built for this run is executed in it.
//!
//! The Custom Field cases write the record the metadata service writes when the API adds a field
//! (`{"change": "add_custom_field", ...}`); the command reads that record, which is the same file
//! a restarted node rebuilds from, so "a new custom field appears after one command, without a
//! restart" is the file changing under a command that holds no state.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

mod common;

const SITE: &str = "devsite";

/// A configuration root (platform and a private copy of the sample app, one site) in a
/// temporary directory, removed on drop.
struct Root(PathBuf);

impl Root {
    fn new(tag: &str) -> Root {
        let dir = std::env::temp_dir().join(format!(
            "spot_types_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        common::copy_dir(&common::repo().join("apps/sample"), &dir.join("sample"));
        std::fs::write(
            dir.join("common.toml"),
            format!(
                "apps = [{:?}, \"sample\"]\n[server]\nbind = ['127.0.0.1:0']\nshutdown_grace = 2\n\
                 [postgres]\nadmin_url = \"postgres://spot:spot@127.0.0.1:1/postgres\"\n\
                 [trust]\nrequire_signed = false\n",
                common::repo().join("apps/platform").display().to_string(),
            ),
        )
        .unwrap();
        let site = dir.join("sites").join(SITE);
        std::fs::create_dir_all(&site).unwrap();
        std::fs::write(
            site.join("site.toml"),
            format!(
                "site = \"{SITE}\"\ndatabase = \"spot_{SITE}\"\napps = [\"platform\", \"sample\"]\n"
            ),
        )
        .unwrap();
        Root(dir)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_spot"));
        command
            .current_dir(&self.0)
            .arg("--root")
            .arg(&self.0)
            .args(args);
        command
    }

    fn spot(&self, args: &[&str]) -> Run {
        Run::of(self.command(args).output().unwrap())
    }

    /// `types <verb> --site devsite <extra...>`.
    fn types(&self, verb: &str, extra: &[&str]) -> Run {
        let mut args = vec!["types", verb, "--site", SITE];
        args.extend_from_slice(extra);
        self.spot(&args)
    }

    /// Records one runtime change the way the metadata service does.
    fn record_custom_field(&self, doctype: &str, spec: serde_json::Value) {
        let record = serde_json::json!({
            "version": 1,
            "changes": [{ "change": "add_custom_field", "doctype": doctype, "spec": spec }],
        });
        std::fs::write(
            self.0.join("sites").join(SITE).join("metadata.json"),
            record.to_string(),
        )
        .unwrap();
    }

    /// The declarations the library makes for the sample app on this root, through the host's
    /// own loader: what a correct `generate --app sample` must write.
    fn expected_for_sample(&self) -> String {
        let loaded = spot_cli::config::load(&self.0, false).unwrap();
        let site = loaded.sites.iter().find(|s| s.site == SITE).unwrap();
        let meta = spot_cli::services::metadata::effective(&self.0, &loaded, site).unwrap();
        let catalogue = spot_cli::apps::catalogue(&self.0, &loaded).unwrap();
        let sample = catalogue.iter().find(|a| a.name == "sample").unwrap();
        let names: Vec<&str> = sample.definitions.iter().map(|d| d.name.as_str()).collect();
        spot_dts::generate_for(meta.definitions(), &names)
            .unwrap()
            .text
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    fn of(output: Output) -> Run {
        Run {
            code: output.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&output.stdout).into_owned(),
            err: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    #[track_caller]
    fn expect(&self, code: i32) -> &Run {
        assert_eq!(
            self.code, code,
            "stdout: {}\nstderr: {}",
            self.out, self.err
        );
        self
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

fn modified(path: &Path) -> std::time::SystemTime {
    std::fs::metadata(path).unwrap().modified().unwrap()
}

// --- generate ---------------------------------------------------------------------------------

#[test]
fn the_sample_app_is_generated_byte_for_byte_and_a_second_run_changes_nothing() {
    let root = Root::new("golden");
    let first = root.types("generate", &["--app", "sample", "--out", "types"]);
    first.expect(0);
    assert!(first.out.contains("created"), "{}", first.out);
    let types = root.path().join("types/spot.d.ts");
    let tsconfig = root.path().join("types/tsconfig.spot.json");
    assert_eq!(read(&types), root.expected_for_sample());
    assert_eq!(
        read(&tsconfig),
        spot_dts::tsconfig_snippet("spot.d.ts"),
        "the tsconfig is the library's snippet for the sibling file"
    );
    for needle in ["export interface Project", "export interface Task"] {
        assert!(read(&types).contains(needle), "{needle}");
    }

    let (types_mtime, tsconfig_mtime) = (modified(&types), modified(&tsconfig));
    std::thread::sleep(std::time::Duration::from_millis(50));
    let second = root.types("generate", &["--app", "sample", "--out", "types"]);
    second.expect(0);
    assert!(second.out.contains("unchanged"), "{}", second.out);
    assert!(!second.out.contains("created") && !second.out.contains("updated"));
    assert_eq!(
        modified(&types),
        types_mtime,
        "an unchanged file is not touched"
    );
    assert_eq!(modified(&tsconfig), tsconfig_mtime);

    // Two independent roots give the same bytes: the output is deterministic.
    let other = Root::new("golden_twin");
    other
        .types("generate", &["--app", "sample", "--out", "types"])
        .expect(0);
    assert_eq!(read(&types), read(&other.path().join("types/spot.d.ts")));

    // The JSON form names the same facts.
    let json = root.types("generate", &["--app", "sample", "--out", "types", "--json"]);
    json.expect(0);
    let value: serde_json::Value = serde_json::from_str(json.out.trim()).unwrap();
    assert_eq!(value["files"][0]["status"], "unchanged");
    assert_eq!(value["files"][0]["file"], "spot.d.ts");
}

#[test]
fn a_custom_field_appears_after_one_command_and_check_flips_and_back() {
    let root = Root::new("custom");
    root.types("generate", &["--app", "sample", "--out", "types"])
        .expect(0);
    root.types("check", &["--app", "sample", "--out", "types"])
        .expect(0);
    let before = read(&root.path().join("types/spot.d.ts"));
    assert!(!before.contains("custom_tier"));

    // The API adds a Custom Field (recorded in the site's metadata record); no restart.
    root.record_custom_field(
        "project",
        serde_json::json!({"name": "custom_tier", "label": "Tier", "type": "text", "max_chars": 40}),
    );
    let stale = root.types("check", &["--app", "sample", "--out", "types"]);
    stale.expect(2);
    assert!(stale.err.contains("Project"), "{}", stale.err);
    assert!(
        stale
            .err
            .contains("spot types generate --site devsite --app sample --out types"),
        "{}",
        stale.err
    );
    let json = root.types("check", &["--app", "sample", "--out", "types", "--json"]);
    json.expect(2);
    let value: serde_json::Value = serde_json::from_str(json.out.trim()).unwrap();
    assert_eq!(value["ok"], false);
    assert_eq!(value["changed_doctypes"][0], "Project");

    let again = root.types("generate", &["--app", "sample", "--out", "types"]);
    again.expect(0);
    assert!(again.out.contains("updated"), "{}", again.out);
    let after = read(&root.path().join("types/spot.d.ts"));
    assert!(after.contains("custom_tier"), "{after}");
    root.types("check", &["--app", "sample", "--out", "types"])
        .expect(0);
}

#[test]
fn a_missing_output_is_stale_and_names_the_command() {
    let root = Root::new("missing");
    let run = root.types("check", &["--app", "sample", "--out", "nowhere"]);
    run.expect(2);
    assert!(run.err.contains("spot.d.ts is missing"), "{}", run.err);
    assert!(
        run.err.contains("tsconfig.spot.json is missing"),
        "{}",
        run.err
    );
    assert!(run.err.contains("run: spot types generate"), "{}", run.err);
    assert!(
        !root.path().join("nowhere").exists(),
        "check writes nothing"
    );
}

#[test]
fn a_doctype_selection_pulls_in_what_it_references() {
    let root = Root::new("closure");
    root.types("generate", &["--doctype", "project", "--out", "types"])
        .expect(0);
    let text = read(&root.path().join("types/spot.d.ts"));
    assert!(text.contains("export interface Project"), "{text}");
    // The table-multiselect over `label` reaches the label DocType and its child.
    assert!(text.contains("export interface Label"), "{text}");
    assert!(
        !text.contains("export interface TimeEntry"),
        "an unreferenced DocType stays out"
    );
}

#[test]
fn selections_that_name_nothing_real_are_usage_errors() {
    let root = Root::new("usage");
    root.spot(&[
        "types",
        "generate",
        "--site",
        "no-such-site",
        "--out",
        "types",
    ])
    .expect(1);
    root.types(
        "generate",
        &["--doctype", "no_such_doctype", "--out", "types"],
    )
    .expect(1);
    root.types("generate", &["--app", "no_such_app", "--out", "types"])
        .expect(1);
    // A missing `--site` is clap's own parse error: it keeps clap's exit code 2 (commands.md).
    root.spot(&["types", "generate", "--out", "types"])
        .expect(2);
    assert!(
        !root.path().join("types").exists(),
        "a usage error writes nothing"
    );
}

#[test]
fn a_name_collision_reports_every_error_and_writes_nothing() {
    let root = Root::new("collision");
    // `a1` and `a_1` both become the interface `A1`.
    for name in ["a1", "a_1"] {
        std::fs::write(
            root.path().join(format!("sample/doctypes/{name}.toml")),
            format!(
                "name = \"{name}\"\nlabel = \"{name}\"\napp = \"sample\"\nmodule = \"projects\"\n\
                 title_field = \"title\"\n\n[[fields]]\nname = \"title\"\nlabel = \"Title\"\n\
                 type = \"text\"\nmax_chars = 140\nrequired = true\n"
            ),
        )
        .unwrap();
    }
    let run = root.types("generate", &["--app", "sample", "--out", "types"]);
    run.expect(3);
    assert!(run.err.contains("A1"), "{}", run.err);
    assert!(run.err.contains("nothing was written"), "{}", run.err);
    assert!(!root.path().join("types").exists(), "nothing was written");
}

#[test]
fn a_hostile_custom_field_label_cannot_break_out_of_its_comment() {
    let root = Root::new("hostile");
    root.record_custom_field(
        "project",
        serde_json::json!({
            "name": "custom_tier",
            "label": "Tier */ export interface Evil { x: any }\nnext `line`",
            "type": "text",
            "max_chars": 40,
            "default": "DEFAULT-MUST-NOT-APPEAR",
        }),
    );
    root.types("generate", &["--app", "sample", "--out", "types"])
        .expect(0);
    let text = read(&root.path().join("types/spot.d.ts"));
    assert!(
        !text.lines().any(|l| l.starts_with("export interface Evil")),
        "the label cannot start a declaration: {text}"
    );
    assert!(!text.contains("Tier */"), "the closer is neutralised");
    assert!(
        !text.contains("DEFAULT-MUST-NOT-APPEAR"),
        "a declaration never carries a default value"
    );
    // The script tier's parser finds no syntax error in the output.
    let options = spot_ts_transpile::Options::default();
    if let Err(diagnostics) = spot_ts_transpile::transpile(&text, "spot.d.ts", &options) {
        let syntax: Vec<_> = diagnostics
            .iter()
            .filter(|d| d.code == "ts.syntax")
            .collect();
        assert!(syntax.is_empty(), "{syntax:?}\n{text}");
    }
    // `check` reads the line-spanning label as comment text, not as an interface name.
    root.record_custom_field(
        "project",
        serde_json::json!({
            "name": "custom_tier",
            "label": "Tier2 */ export interface Evil { x: any }\nnext",
            "type": "text",
            "max_chars": 40,
        }),
    );
    let stale = root.types("check", &["--app", "sample", "--out", "types"]);
    stale.expect(2);
    assert!(!stale.err.contains("Evil"), "{}", stale.err);
}

// --- the output path --------------------------------------------------------------------------

#[cfg(unix)]
#[test]
fn a_symlinked_output_is_refused_and_nothing_outside_is_written() {
    let root = Root::new("symlink");
    let target = root.path().join("elsewhere");
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, root.path().join("link")).unwrap();
    root.types("generate", &["--app", "sample", "--out", "link"])
        .expect(2);
    assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);

    // A symlinked file inside a real output directory is refused too.
    let real = root.path().join("real");
    std::fs::create_dir_all(&real).unwrap();
    let victim = root.path().join("victim.txt");
    std::fs::write(&victim, "precious").unwrap();
    std::os::unix::fs::symlink(&victim, real.join("spot.d.ts")).unwrap();
    root.types("generate", &["--app", "sample", "--out", "real"])
        .expect(2);
    assert_eq!(read(&victim), "precious");
    root.types("check", &["--app", "sample", "--out", "real"])
        .expect(2);
}

#[cfg(unix)]
#[test]
fn a_read_only_output_directory_is_a_clean_failure() {
    use std::os::unix::fs::PermissionsExt;
    let root = Root::new("readonly");
    let out = root.path().join("locked");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o555)).unwrap();
    // A superuser can write anyway; the case then proves nothing and says so.
    let probe = out.join("probe");
    if std::fs::write(&probe, "x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        eprintln!("skipped: this user can write into a read-only directory");
        std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    let run = root.types("generate", &["--app", "sample", "--out", "locked"]);
    std::fs::set_permissions(&out, std::fs::Permissions::from_mode(0o755)).unwrap();
    run.expect(3);
    assert!(run.err.contains("cannot be written"), "{}", run.err);
    assert_eq!(
        std::fs::read_dir(&out).unwrap().count(),
        0,
        "no temporary file is left"
    );
}

#[test]
fn two_runs_in_parallel_leave_one_complete_file() {
    let root = Root::new("parallel");
    let args = [
        "types", "generate", "--site", SITE, "--app", "sample", "--out", "types",
    ];
    let spawn = || {
        root.command(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (a, b) = (spawn(), spawn());
    for child in [a, b] {
        Run::of(child.wait_with_output().unwrap()).expect(0);
    }
    assert_eq!(
        read(&root.path().join("types/spot.d.ts")),
        root.expected_for_sample()
    );
    let leftovers: Vec<String> = std::fs::read_dir(root.path().join("types"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

// --- help -------------------------------------------------------------------------------------

#[test]
fn help_lists_the_command() {
    let root = Root::new("help");
    let help = root.spot(&["--help"]);
    help.expect(0);
    assert!(
        help.out
            .lines()
            .any(|l| l.trim_start().starts_with("types")),
        "{}",
        help.out
    );
    let types = root.spot(&["types", "--help"]);
    types.expect(0);
    assert!(types.out.contains("generate") && types.out.contains("check"));
}
