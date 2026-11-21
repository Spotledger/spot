//! `spot promote` (WP-ALM-04.T5): the command tree and the environment identity without a
//! process, and a full dev -> test -> prod promotion over three real sites with PostgreSQL
//! (`SPOT_TEST_PG_URL`, and `pg_dump` for the rehearsal clone; each is skipped with a printed
//! reason when missing).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use spot_bundle_signing::{
    KeyPair, PublisherCertificate, RevocationBody, TrustAnchor, TrustedRoot, generate_keypair,
    issue_certificate, issue_revocation_list,
};
use spot_custom_bundle::{CaptureOptions, FakeSite, capture};
use spot_environments::Role;
use spot_meta::{Definition, FieldSpec, FieldType};

/// The command modules, driven without a process.
#[path = "../src/commands/mod.rs"]
mod commands;

use commands::bundle::make_handover;

fn root_key() -> KeyPair {
    generate_keypair(&[1_u8; 32])
}

fn publisher() -> KeyPair {
    generate_keypair(&[2_u8; 32])
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[test]
fn the_command_tree_asks_who_acts() {
    let command = commands::promote::command();
    for step in ["request", "approve"] {
        let sub = command.find_subcommand(step).unwrap();
        let by = sub.get_arguments().find(|a| a.get_id() == "as").unwrap();
        assert!(by.is_required_set(), "{step} needs --as");
    }
    let parsed = command
        .try_get_matches_from(["promote", "apply", "p1", "--max-rehearsal-age-secs=-1"])
        .unwrap();
    let (_, apply) = parsed.subcommand().unwrap();
    assert_eq!(apply.get_one::<i64>("max-rehearsal-age-secs"), Some(&-1));
}

#[test]
fn guards_still_refuse_what_the_wired_path_relies_on() {
    use commands::promote::{Env, check_approvals, check_environments, check_rehearsal};
    let env = |role: &str| Env {
        set: "s".into(),
        role: role.into(),
    };
    assert!(check_environments(&env("dev"), &env("prod")).is_err());
    assert!(check_approvals("prod", "alice", &["alice", "alice"]).is_err());
    assert!(check_rehearsal("prod", Some(0), 100, 10).is_err());
}

fn pg_dump_available() -> bool {
    commands::promote::pg_bin_for_tests().is_some()
}

fn code(output: &std::process::Output) -> Option<i32> {
    output.status.code()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_promotion_runs_dev_to_test_and_test_to_prod_through_the_wired_ports() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    if !pg_dump_available() {
        println!("SKIPPED: pg_dump was not found (set SPOT_PG_BIN)");
        return;
    }
    let root = env.root.0.clone();
    let dev = env.site.clone();
    let _secret = env.site();
    let test = format!("{dev}-b");
    let prod = format!("{dev}-c");
    common::ok(&root, &["new-site", &test]);
    common::ok(&root, &["new-site", &prod]);

    // Keys: one publisher certificate, one trust anchor and revocation list for test and prod.
    let keys = root.join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(keys.join("seed"), hex(&publisher().to_seed_bytes())).unwrap();
    let certificate = issue_certificate(
        PublisherCertificate {
            publisher_id: "acme-software".to_owned(),
            display_name: "Acme Software".to_owned(),
            key: publisher().public(),
            valid_from: 0,
            valid_to: 4_102_444_800,
            issued_by: root_key().public().key_id(),
            serial: 9,
        },
        &root_key(),
    );
    std::fs::write(
        keys.join("anchor.json"),
        format!(
            r#"{{"roots":[{{"public_key":"{}","valid_from":0}}]}}"#,
            hex(root_key().public().as_bytes())
        ),
    )
    .unwrap();
    let revocations = issue_revocation_list(
        RevocationBody {
            generation: 1,
            issued_at: 0,
            issued_by: root_key().public().key_id(),
            keys: Vec::new(),
            publishers: Vec::new(),
            bundles: Vec::new(),
        },
        &root_key(),
    );
    std::fs::write(keys.join("revocations.json"), revocations.to_json()).unwrap();
    let path = |name: &str| keys.join(name).display().to_string();
    common::ok(
        &root,
        &["bundle", "env", "link", "--site", &dev, "--kind", "dev"],
    );
    for (site, kind) in [(&test, "test"), (&prod, "prod")] {
        common::ok(
            &root,
            &[
                "bundle",
                "env",
                "link",
                "--site",
                site,
                "--kind",
                kind,
                "--trust-anchor",
                &path("anchor.json"),
                "--revocations",
                &path("revocations.json"),
            ],
        );
    }

    // A bundle that adds one custom field to a DocType of the sample app.
    let mut definition = Definition::new("task");
    definition.app = Some("sample".to_owned());
    let image = FakeSite::new()
        .with_app("sample", semver::Version::new(1, 0, 0))
        .with_definition(definition)
        .with_custom_field(
            "task",
            FieldSpec::new("custom_risk_class", FieldType::Text),
            None,
        );
    let bundle = capture(
        &image,
        &CaptureOptions::new("risk", "1.0.0", "alice", "2026-11-13T08:00:00Z"),
    )
    .unwrap();
    let key = publisher();
    let handover = |from: &str, source: Role, target: Role, name: &str| {
        let bytes = make_handover(
            &bundle,
            from,
            source,
            target,
            "alice",
            &key,
            &certificate,
            1_000,
        )
        .unwrap();
        let file = root.join(name);
        std::fs::write(&file, bytes).unwrap();
        file.display().to_string()
    };
    let dev_to_test = handover(&dev, Role::Dev, Role::Test, "dev-test.handover");
    let test_to_prod = handover(&test, Role::Test, Role::Prod, "test-prod.handover");

    // The wrong environment is refused before anything is recorded: dev cannot skip to prod.
    let skip = common::run(
        &root,
        &[
            "promote",
            "request",
            "--from",
            &dev,
            "--to",
            &prod,
            "--bundle",
            &dev_to_test,
            "--as",
            "alice",
        ],
        None,
    );
    assert_eq!(code(&skip), Some(2), "{}", stderr(&skip));

    // dev -> test: additive, so the test policy needs no approver; the run rehearses on a clone,
    // applies, verifies and writes both chains.
    let requested = common::ok(
        &root,
        &[
            "promote",
            "request",
            "--from",
            &dev,
            "--to",
            &test,
            "--bundle",
            &dev_to_test,
            "--as",
            "alice",
        ],
    );
    let id = requested
        .split("promotion ")
        .nth(1)
        .and_then(|rest| rest.split(':').next())
        .unwrap()
        .to_owned();
    let applied = common::ok(&root, &["promote", "apply", &id]);
    assert!(applied.contains("verified"), "{applied}");
    let status = common::ok(&root, &["promote", "status", &id]);
    assert!(status.contains("state: applied"), "{status}");
    // An applied promotion is not applied twice and cannot be aborted.
    assert_eq!(
        code(&common::run(&root, &["promote", "apply", &id], None)),
        Some(2)
    );
    assert_eq!(
        code(&common::run(&root, &["promote", "abort", &id], None)),
        Some(2)
    );

    // test -> prod: two approvers, one not the requester, and a fresh rehearsal.
    let requested = common::ok(
        &root,
        &[
            "promote",
            "request",
            "--from",
            &test,
            "--to",
            &prod,
            "--bundle",
            &test_to_prod,
            "--as",
            "alice",
        ],
    );
    let id = requested
        .split("promotion ")
        .nth(1)
        .and_then(|rest| rest.split(':').next())
        .unwrap()
        .to_owned();
    let approve = |who: &str| {
        common::run(
            &root,
            &["promote", "approve", &id, "--as", who, "--role", "owner"],
            None,
        )
    };
    assert_eq!(
        code(&approve("alice")),
        Some(2),
        "the requester never approves production"
    );
    assert_eq!(code(&approve("bob")), Some(0));
    assert_eq!(
        code(&approve("bob")),
        Some(2),
        "the same approver twice is refused"
    );
    let one = common::run(&root, &["promote", "apply", &id], None);
    assert_eq!(code(&one), Some(2), "one approver is not enough");
    assert!(
        stderr(&one).contains("promote.approvals_missing"),
        "{}",
        stderr(&one)
    );
    assert_eq!(code(&approve("carol")), Some(0));
    let stale = common::run(
        &root,
        &["promote", "apply", &id, "--max-rehearsal-age-secs=-1"],
        None,
    );
    assert_eq!(
        code(&stale),
        Some(2),
        "a stale rehearsal refuses production"
    );
    assert!(
        stderr(&stale).contains("promote.rehearsal_stale"),
        "{}",
        stderr(&stale)
    );
    let applied = common::ok(&root, &["promote", "apply", &id]);
    assert!(applied.contains("verified"), "{applied}");

    // Abort: a requested promotion is closed and its clones are dropped.
    let requested = common::ok(
        &root,
        &[
            "promote",
            "request",
            "--from",
            &test,
            "--to",
            &prod,
            "--bundle",
            &test_to_prod,
            "--as",
            "alice",
        ],
    );
    let id = requested
        .split("promotion ")
        .nth(1)
        .and_then(|rest| rest.split(':').next())
        .unwrap()
        .to_owned();
    let aborted = common::ok(&root, &["promote", "abort", &id]);
    assert!(aborted.contains("aborted"), "{aborted}");
}
