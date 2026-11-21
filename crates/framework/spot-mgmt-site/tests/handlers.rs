//! Each operation against a scripted CLI: the exact command line it runs, the answer it returns,
//! the refusals (other site, no token), the exit-code mapping, cancel and redaction.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use serde_json::Value;
use spot_mgmt_site::{CliError, Maintenance, invoke};

use common::{CancelNow, FakeCli, Quiet, SITE, context, output, request};

const LIST: &str = "target nightly (2 backups of site demo)\n\
    b-20261101 1793500000 bytes=1048576 verified encrypted digest=0123456789ab\n\
    b-20261102 1793586400 bytes=2097152 unverified unencrypted digest=ba9876543210\n";

#[tokio::test]
async fn backup_run_runs_the_cli_command_and_attaches_the_line() {
    let cli = FakeCli::new(vec![output(
        0,
        "backup b-1 written\nverified: digest only\n",
        "",
    )]);
    let ctx = context(cli.clone());
    let ctl = Quiet::new();
    let req = request("site.backup.run", &[("site", SITE), ("target", "nightly")]);
    let result = invoke(&ctx, &ctl, &req).await.unwrap();
    assert_eq!(
        cli.argv_list(),
        vec![vec!["backup", "run", "--site=demo", "--target=nightly"]]
    );
    assert_eq!(result["exit"], 0);
    assert_eq!(
        result["cli"],
        "spot backup run --site demo --target nightly"
    );
    assert_eq!(result["output"][0], "backup b-1 written");
    assert_eq!(result["truncated"], false);
    let progress = ctl.progress.lock().unwrap().clone();
    assert_eq!(progress.first().map(|p| p.1), Some(0));
    assert_eq!(progress.last().map(|p| p.1), Some(100));
}

#[tokio::test]
async fn backup_list_returns_the_backups_the_cli_lists() {
    let cli = FakeCli::new(vec![output(0, LIST, "")]);
    let result = invoke(
        &context(cli.clone()),
        &Quiet::new(),
        &request("site.backup.list", &[("site", SITE)]),
    )
    .await
    .unwrap();
    let backups = result["backups"].as_array().unwrap();
    assert_eq!(backups.len(), 2);
    assert_eq!(backups[0]["id"], "b-20261101");
    assert_eq!(backups[0]["verified"], true);
    assert_eq!(backups[0]["encrypted"], true);
    assert_eq!(backups[0]["bytes"], 1_048_576);
    assert_eq!(backups[1]["verified"], false);
    assert_eq!(cli.argv_list(), vec![vec!["backup", "list", "--site=demo"]]);
}

#[tokio::test]
async fn backup_verify_passes_the_id_as_the_cli_flag() {
    let cli = FakeCli::new(vec![output(
        0,
        "verified: digest, size and header only\n",
        "",
    )]);
    let req = request(
        "site.backup.verify",
        &[("site", SITE), ("backup_id", "b-20261101")],
    );
    invoke(&context(cli.clone()), &Quiet::new(), &req)
        .await
        .unwrap();
    assert_eq!(
        cli.argv_list(),
        vec![vec!["backup", "verify", "--site=demo", "--id=b-20261101"]]
    );
}

#[tokio::test]
async fn a_command_for_another_site_is_forbidden_and_runs_nothing() {
    let zeros = "0".repeat(64);
    for (op, params) in [
        (
            "site.backup.run",
            vec![("site", "other"), ("target", "nightly")],
        ),
        ("site.backup.list", vec![("site", "other")]),
        ("site.status", vec![("site", "other")]),
        ("site.maintenance.leave", vec![("site", "other")]),
        (
            "site.migrate.show",
            vec![("site", "other"), ("plan_hash", zeros.as_str())],
        ),
    ] {
        let cli = FakeCli::new(vec![]);
        let error = invoke(&context(cli.clone()), &Quiet::new(), &request(op, &params))
            .await
            .unwrap_err();
        assert_eq!(error.code, "mgmt.forbidden", "{op}");
        assert!(cli.argv_list().is_empty(), "{op} ran something");
    }
}

#[tokio::test]
async fn preflight_returns_the_token_and_apply_redacts_it() {
    let preflight = "preflight site=demo backup=b-1 target=nightly\n\
        confirm_token=abcDEF123 expires_in=600s\n";
    let cli = FakeCli::new(vec![
        output(0, preflight, ""),
        output(0, "restored site=demo confirm_token=abcDEF123\n", ""),
    ]);
    let ctx = context(cli.clone());
    let pre = invoke(
        &ctx,
        &Quiet::new(),
        &request(
            "site.restore.preflight",
            &[("site", SITE), ("backup_id", "b-1"), ("target", "nightly")],
        ),
    )
    .await
    .unwrap();
    assert_eq!(pre["fields"]["confirm_token"], "abcDEF123");
    let applied = invoke(
        &ctx,
        &Quiet::new(),
        &request(
            "site.restore.apply",
            &[
                ("site", SITE),
                ("backup_id", "b-1"),
                ("target", "nightly"),
                ("confirm", "abcDEF123"),
            ],
        ),
    )
    .await
    .unwrap();
    let text = applied.to_string();
    assert!(!text.contains("abcDEF123"), "the token leaked: {text}");
    assert_eq!(
        applied["cli"],
        "spot restore apply --site demo --from b-1 --target nightly --confirm <prompt>"
    );
    assert_eq!(
        cli.argv_list()[1],
        vec![
            "restore",
            "apply",
            "--site=demo",
            "--from=b-1",
            "--target=nightly",
            "--confirm=abcDEF123"
        ]
    );
}

#[tokio::test]
async fn restore_without_a_plausible_token_never_starts() {
    for token in ["two words", "x y"] {
        let cli = FakeCli::new(vec![]);
        let result = invoke(
            &context(cli.clone()),
            &Quiet::new(),
            &request(
                "site.restore.apply",
                &[
                    ("site", SITE),
                    ("backup_id", "b-1"),
                    ("target", "nightly"),
                    ("confirm", token),
                ],
            ),
        )
        .await;
        let error = result.unwrap_err();
        assert_eq!(error.code, "mgmt.invalid_parameter", "{token:?}");
        assert!(cli.argv_list().is_empty());
    }
}

#[tokio::test]
async fn a_wrong_token_is_the_clis_refusal_and_the_api_refuses_too() {
    let cli = FakeCli::new(vec![output(
        2,
        "",
        "restore.confirm_invalid: the token is not valid\n",
    )]);
    let error = invoke(
        &context(cli),
        &Quiet::new(),
        &request(
            "site.restore.apply",
            &[
                ("site", SITE),
                ("backup_id", "b-1"),
                ("target", "nightly"),
                ("confirm", "wrong"),
            ],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "mgmt.refused_by_policy");
}

#[tokio::test]
async fn exit_codes_map_to_management_codes() {
    for (exit, code) in [
        (1u8, "mgmt.invalid_parameter"),
        (2, "mgmt.refused_by_policy"),
        (3, "mgmt.internal"),
        (9, "mgmt.internal"),
    ] {
        let cli = FakeCli::new(vec![output(exit, "", "x")]);
        let error = invoke(
            &context(cli),
            &Quiet::new(),
            &request("site.backup.run", &[("site", SITE), ("target", "nightly")]),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, code, "exit {exit}");
        assert!(!error.cancelled);
    }
    assert_eq!(spot_mgmt_site::code_of_exit(0), None);
}

#[tokio::test]
async fn a_command_that_cannot_run_is_an_internal_failure() {
    for failure in [CliError::Spawn, CliError::Timeout, CliError::Io] {
        let cli = FakeCli::new(vec![Err(failure)]);
        let error = invoke(
            &context(cli),
            &Quiet::new(),
            &request("site.backup.list", &[("site", SITE)]),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "mgmt.internal");
    }
}

#[tokio::test]
async fn maintenance_enter_and_leave_run_the_gate_commands() {
    let cli = FakeCli::new(vec![
        output(0, "site demo: maintenance gate closed\n", ""),
        output(0, "site demo: maintenance gate open\n", ""),
    ]);
    let ctx = context(cli.clone());
    let enter = invoke(
        &ctx,
        &Quiet::new(),
        &request(
            "site.maintenance.enter",
            &[("site", SITE), ("reason", "--upgrade; rm -rf /")],
        ),
    )
    .await
    .unwrap();
    invoke(
        &ctx,
        &Quiet::new(),
        &request("site.maintenance.leave", &[("site", SITE)]),
    )
    .await
    .unwrap();
    // A reason that looks like a flag or a shell line stays one `--reason=` argument.
    assert_eq!(
        cli.argv_list()[0],
        vec![
            "migrate-apply",
            "maintenance",
            "enter",
            "--site=demo",
            "--reason=--upgrade; rm -rf /"
        ]
    );
    assert_eq!(
        cli.argv_list()[1],
        vec!["migrate-apply", "maintenance", "leave", "--site=demo"]
    );
    assert!(
        enter["cli"]
            .as_str()
            .unwrap()
            .contains("--reason '--upgrade; rm -rf /'")
    );
}

#[tokio::test]
async fn cancel_stops_a_read_but_never_a_backup() {
    let hang = FakeCli::hanging();
    let error = invoke(
        &context(hang.clone()),
        &CancelNow,
        &request("site.backup.list", &[("site", SITE)]),
    )
    .await
    .unwrap_err();
    assert!(error.cancelled);
    assert_eq!(error.code, "mgmt.cancelled");

    // A backup run ignores the cancel request: it completes with the CLI's own answer.
    let cli = FakeCli::new(vec![output(0, "backup done\n", "")]);
    let done = invoke(
        &context(cli),
        &CancelNow,
        &request("site.backup.run", &[("site", SITE), ("target", "nightly")]),
    )
    .await
    .unwrap();
    assert_eq!(done["exit"], 0);
}

#[tokio::test]
async fn credentials_in_the_output_are_redacted_and_the_output_is_capped() {
    let many: String = (0..500).map(|n| format!("line {n}\n")).collect();
    let stdout = format!(
        "connect postgres://spot:hunter2@db.internal:5432/demo\npassword=hunter2 ok\n{many}"
    );
    let cli = FakeCli::new(vec![output(0, &stdout, "api_key=abc123 warning\n")]);
    let result = invoke(
        &context(cli),
        &Quiet::new(),
        &request(
            "site.backup.verify",
            &[("site", SITE), ("backup_id", "b-1")],
        ),
    )
    .await
    .unwrap();
    let text = result.to_string();
    assert!(!text.contains("hunter2"), "{text}");
    assert!(!text.contains("abc123"), "{text}");
    assert!(text.contains("postgres://***@db.internal:5432/demo"));
    assert_eq!(result["truncated"], true);
    assert!(result["output"].as_array().unwrap().len() <= 200);
    assert!(text.len() < 64 * 1024);
}

#[tokio::test]
async fn status_reports_version_gate_last_backup_and_hold() {
    let cli = FakeCli::new(vec![
        output(0, LIST, ""),
        output(0, "site demo: open (no hold)\n", ""),
    ]);
    let mut ctx = context(cli.clone());
    ctx.facts = std::sync::Arc::new(common::FakeFacts(Maintenance::Closed {
        reason: Some("upgrade".to_owned()),
    }));
    let status = invoke(
        &ctx,
        &Quiet::new(),
        &request("site.status", &[("site", SITE)]),
    )
    .await
    .unwrap();
    assert_eq!(status["version"], "0.1.2");
    assert_eq!(status["maintenance"]["state"], "closed");
    assert_eq!(status["maintenance"]["reason"], "upgrade");
    assert_eq!(status["last_backup"]["id"], "b-20261102");
    assert_eq!(status["migration_hold"]["held"], false);
    assert_eq!(
        cli.argv_list(),
        vec![
            vec!["backup", "list", "--site=demo"],
            vec!["migrate-hold", "status", "--site=demo"]
        ]
    );
}

#[tokio::test]
async fn status_survives_an_unreadable_part() {
    let cli = FakeCli::new(vec![output(2, "", "no target"), Err(CliError::Spawn)]);
    let status = invoke(
        &context(cli),
        &Quiet::new(),
        &request("site.status", &[("site", SITE)]),
    )
    .await
    .unwrap();
    assert_eq!(status["last_backup"], Value::Null);
    assert_eq!(status["migration_hold"], Value::Null);
    assert_eq!(status["maintenance"]["state"], "open");
}

#[tokio::test]
async fn a_held_migration_is_reported() {
    let cli = FakeCli::new(vec![
        output(0, LIST, ""),
        output(
            0,
            "site demo: held by mig-7 since 1793500000 (effects)\n",
            "",
        ),
    ]);
    let status = invoke(
        &context(cli),
        &Quiet::new(),
        &request("site.status", &[("site", SITE)]),
    )
    .await
    .unwrap();
    assert_eq!(status["migration_hold"]["held"], true);
}

#[tokio::test]
async fn migration_verbs_refuse_when_the_lifecycle_is_not_wired() {
    let cli = FakeCli::new(vec![]);
    let hash = "a".repeat(64);
    let error = invoke(
        &context(cli.clone()),
        &Quiet::new(),
        &request(
            "site.migrate.show",
            &[("site", SITE), ("plan_hash", hash.as_str())],
        ),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "mgmt.internal");
    assert!(cli.argv_list().is_empty());
}

#[test]
fn a_plausible_token_is_printable_ascii_without_spaces() {
    use spot_mgmt_site::args::plausible_token;
    assert!(plausible_token("abcDEF123-_."));
    assert!(!plausible_token(""));
    assert!(!plausible_token("two words"));
    assert!(!plausible_token("tab\there"));
    assert!(!plausible_token(&"x".repeat(201)));
    assert!(!plausible_token("caf\u{e9}"));
}
