//! The hosted site operations: the child-process CLI runner (exit codes, timeout, spawn failure,
//! output cap), the settings, the set of operations, and the management result against the output
//! of the same command (a shell script stands in for the `spot` binary; no database needed).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use spot_cli::services::mgmt_site::{ChildCli, ORDER, Settings, operations_for};
use spot_mgmt_api::handlers::HandlerRequest;
use spot_mgmt_api::ports::BoxFuture;
use spot_mgmt_ops::{Catalogue, Invocation, OpId, ParamValue};
use spot_mgmt_site::{CliError, CliRequest, Control, SiteCli, invoke};

fn shell(script: &str) -> ChildCli {
    ChildCli::new(PathBuf::from("/bin/sh"), PathBuf::from("/srv/spot")).with_leading_args(vec![
        "-c".to_owned(),
        script.to_owned(),
        "spot".to_owned(),
    ])
}

fn request(args: &[&str], timeout_secs: u64) -> CliRequest {
    CliRequest {
        argv: args.iter().map(|a| (*a).to_owned()).collect(),
        timeout_secs,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_gets_the_root_and_its_arguments_and_returns_what_it_printed() {
    let cli = shell("printf 'args: %s\\n' \"$*\"; printf 'warn\\n' >&2; exit 2");
    let out = cli
        .run(request(&["backup", "list", "--site=demo"], 10))
        .await
        .unwrap();
    assert_eq!(out.exit, 2);
    assert_eq!(
        out.stdout,
        "args: --root=/srv/spot backup list --site=demo\n"
    );
    assert_eq!(out.stderr, "warn\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_value_with_shell_syntax_is_one_argument() {
    let cli = shell("printf '%s\\n' \"$#\" \"$2\"");
    let out = cli
        .run(request(&["--reason=a; rm -rf / $(x)"], 10))
        .await
        .unwrap();
    assert_eq!(out.exit, 0);
    assert_eq!(out.stdout, "2\n--reason=a; rm -rf / $(x)\n");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_that_runs_too_long_is_stopped() {
    let cli = shell("sleep 30");
    assert_eq!(cli.run(request(&[], 1)).await, Err(CliError::Timeout));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_program_is_a_spawn_failure() {
    let cli = ChildCli::new(PathBuf::from("/nonexistent/spot"), PathBuf::from("/srv"));
    assert_eq!(cli.run(request(&[], 5)).await, Err(CliError::Spawn));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_killed_by_a_signal_is_a_failure() {
    let cli = shell("kill -9 $$");
    let out = cli.run(request(&[], 10)).await.unwrap();
    assert_eq!(out.exit, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_output_is_bounded() {
    let cli = shell("head -c 600000 /dev/zero | tr '\\0' x");
    let out = cli.run(request(&[], 20)).await.unwrap();
    assert_eq!(out.exit, 0);
    assert_eq!(out.stdout.len(), 256 * 1024);
}

#[test]
fn the_settings_default_decode_and_refuse_what_is_out_of_bounds() {
    let defaults = Settings::default();
    assert_eq!(defaults.short_timeout_seconds, 600);
    assert_eq!(defaults.long_timeout_seconds, 6 * 3600);
    defaults.validate().unwrap();
    assert_eq!(ORDER, 1060);
    let decoded: Settings = toml::from_str("short_timeout_seconds = 30").unwrap();
    assert_eq!(decoded.short_timeout_seconds, 30);
    assert!(toml::from_str::<Settings>("surprise = 1").is_err());
    let tiny = Settings {
        short_timeout_seconds: 1,
        ..Settings::default()
    };
    assert!(tiny.validate().is_err());
    let inverted = Settings {
        short_timeout_seconds: 600,
        long_timeout_seconds: 60,
    };
    assert!(inverted.validate().is_err());
}

#[test]
fn eighteen_operations_are_added_and_the_catalogue_builds() {
    let cli: Arc<dyn SiteCli> = Arc::new(shell("exit 0"));
    let ops = operations_for("demo", cli, None, &Settings::default());
    assert_eq!(ops.len(), 18);
    assert_eq!(ops.iter().filter(|op| op.is_migration()).count(), 10);
    let mut builder = Catalogue::builder();
    for op in &ops {
        builder.add("site", op.spec.clone());
    }
    assert_eq!(builder.build().unwrap().len(), 18);
}

struct Quiet;

impl Control for Quiet {
    fn progress<'a>(&'a self, _step: &'a str, _percent: u8) -> BoxFuture<'a, ()> {
        Box::pin(std::future::ready(()))
    }
    fn cancelled(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_management_result_is_the_output_of_the_same_command() {
    let listing = "target nightly (1 backups of site demo)\\nb-1 1793500000 bytes=10 verified encrypted digest=abc\\n";
    let cli: Arc<dyn SiteCli> = Arc::new(shell(&format!("printf '{listing}'")));
    let ops = operations_for("demo", Arc::clone(&cli), None, &Settings::default());
    let mut builder = Catalogue::builder();
    for op in &ops {
        builder.add("site", op.spec.clone());
    }
    let catalogue = builder.build().unwrap();
    let id = OpId::new("site.backup.list").unwrap();
    let params = catalogue
        .validate(&Invocation::new(id.clone()).with("site", ParamValue::Text("demo".to_owned())))
        .unwrap();
    let request = HandlerRequest {
        command_id: "c1".to_owned(),
        principal: "alice".to_owned(),
        operation: id,
        params,
    };
    // What the CLI itself prints for the same arguments.
    let direct = cli
        .run(CliRequest {
            argv: vec!["backup".into(), "list".into(), "--site=demo".into()],
            timeout_secs: 10,
        })
        .await
        .unwrap();
    // The handler runs the site's own context: rebuild it the way the host does.
    let site_ctx = spot_mgmt_site::SiteContext {
        site: "demo".to_owned(),
        cli,
        facts: Arc::new(spot_cli::services::mgmt_site::HostFacts),
        migrations: None,
        timeouts: spot_mgmt_site::Timeouts::default(),
    };
    let result = invoke(&site_ctx, &Quiet, &request).await.unwrap();
    let expected: Vec<&str> = direct.stdout.lines().collect();
    assert_eq!(result["output"], json!(expected));
    assert_eq!(result["backups"][0]["id"], "b-1");
}
