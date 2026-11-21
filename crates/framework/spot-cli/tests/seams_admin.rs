//! WP-EXT-03.T2: the override listing. `listing` is the data of `platform/seams/overrides` and of
//! `spot overrides list --json`; the command runs as a process over a root with the sample app
//! and no database (it reads configuration and app directories only).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use serde_json::Value as Json;
use spot_cli::platform_registry_builder;
use spot_cli::services::seams_admin::listing;
use spot_controllers::{ControllerRegistry, ControllerSource};
use spot_storage_contract::RecordId;
use std::path::PathBuf;
use std::process::{Command, Output};

struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn root() -> Root {
    let root = Root(std::env::temp_dir().join(format!("spot_cli_seams_{}", RecordId::new_v7())));
    common::copy_dir(&common::repo().join("apps/sample"), &root.0.join("sample"));
    std::fs::create_dir_all(root.0.join("sites/demo")).unwrap();
    std::fs::write(
        root.0.join("common.toml"),
        "apps = ['sample']\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\nadmin_url = 'postgres://spot@localhost/postgres'\n",
    )
    .unwrap();
    std::fs::write(
        root.0.join("sites/demo/site.toml"),
        "site = 'demo'\ndatabase = 'demo'\napps = ['platform', 'sample']\n",
    )
    .unwrap();
    root
}

fn spot(root: &Root, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_spot"))
        .arg("--root")
        .arg(&root.0)
        .args(args)
        .output()
        .unwrap()
}

fn code(o: &Output) -> i32 {
    o.status.code().unwrap()
}

fn sample_report() -> spot_seams::OverrideReport {
    let mut registry = ControllerRegistry::new();
    for controller in spot_sample_app::controllers() {
        registry.register("sample", controller, ControllerSource::Native);
    }
    let mut builder = platform_registry_builder().unwrap();
    registry.install_into(&mut builder);
    builder.build().unwrap().override_report()
}

#[test]
fn the_listing_is_the_reports_rows_plus_conflicts_and_nothing_else() {
    let report = sample_report();
    let value = listing(&report, &[]);
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, ["conflicts", "overrides", "seams"]);
    assert_eq!(value["conflicts"], serde_json::json!([]));
    assert_eq!(
        value["overrides"].as_array().unwrap().len(),
        report.overrides.len()
    );
    // Names, kinds and reasons: no code, arguments or handlers.
    let text = value.to_string();
    for forbidden in ["handler", "arguments", "secret"] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }
    assert_eq!(
        listing(&report, &[]),
        listing(&report, &[]),
        "deterministic"
    );
}

#[test]
fn the_command_prints_the_report_as_text_and_json() {
    let root = root();
    let text = spot(&root, &["overrides", "list", "--site", "demo"]);
    assert_eq!(code(&text), 0, "{}", String::from_utf8_lossy(&text.stderr));
    let out = String::from_utf8_lossy(&text.stdout).into_owned();
    assert!(out.contains("OVERRIDES") && out.contains("SEAMS"), "{out}");

    let json = spot(&root, &["overrides", "list", "--site", "demo", "--json"]);
    assert_eq!(code(&json), 0);
    let value: Json = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(value["conflicts"], serde_json::json!([]));
    assert!(value["overrides"].is_array() && value["seams"].is_array());
    // Deterministic across runs.
    let again = spot(&root, &["overrides", "list", "--site", "demo", "--json"]);
    assert_eq!(json.stdout, again.stdout);

    // `--app` keeps only rows that involve the app.
    let filtered = spot(
        &root,
        &[
            "overrides",
            "list",
            "--site",
            "demo",
            "--app",
            "nonesuch",
            "--json",
        ],
    );
    assert_eq!(code(&filtered), 0);
    let value: Json = serde_json::from_slice(&filtered.stdout).unwrap();
    assert!(value["overrides"].as_array().unwrap().is_empty());
}

#[test]
fn an_unreadable_site_is_exit_three_with_a_clear_message() {
    let root = root();
    let out = spot(&root, &["overrides", "list", "--site", "nosuch"]);
    assert_eq!(code(&out), 3);
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("there is no site `nosuch`"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let empty = Root(std::env::temp_dir().join(format!("spot_cli_seams_{}", RecordId::new_v7())));
    std::fs::create_dir_all(&empty.0).unwrap();
    assert_eq!(
        code(&spot(&empty, &["overrides", "list", "--site", "demo"])),
        3
    );
}

#[test]
fn help_lists_overrides_and_a_missing_site_is_a_clap_error() {
    let root = root();
    let help = spot(&root, &["--help"]);
    assert!(String::from_utf8_lossy(&help.stdout).contains("overrides"));
    assert_eq!(code(&spot(&root, &["overrides", "list"])), 2);
}
