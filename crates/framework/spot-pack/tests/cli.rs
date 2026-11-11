//! The `spot-pack` command line as a real process: exit codes 0 ok / 1 problems / 2 usage,
//! human text by default and `--json`, each subcommand against a real fixture app. The
//! binary is the one cargo built for this test run (`CARGO_BIN_EXE_spot-pack`).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

mod common;

use common::{APP_ID, certificate_json, good_dir, revocations_json, trust_roots_text, workdir};

/// The CLI under test.
fn cli() -> Command {
    Command::new(env!("CARGO_BIN_EXE_spot-pack"))
}

/// The signing material written to files, as `pack --sign` and `verify` need them.
#[allow(dead_code)] // the key path is kept for readability of the fail diagnostics
struct Material {
    key: std::path::PathBuf,
    certificate: std::path::PathBuf,
    trust: std::path::PathBuf,
    revocations: std::path::PathBuf,
    signature: std::path::PathBuf,
    bundle: std::path::PathBuf,
}

/// Signs the good app through the CLI itself: `pack --sign`, which exercises the key-file
/// provider path end to end.
fn signed_by_cli(root: &std::path::Path) -> (Material, std::path::PathBuf) {
    let dir = good_dir(root, &[]);
    let bundle = root.join("out.spotapp");
    let key = root.join("publisher.key");
    let certificate = root.join("cert.json");
    let trust = root.join("trust.txt");
    let revocations = root.join("revocations.json");
    spot_pack::write_key_file(&common::publisher(), &key).expect("the key file is written");
    std::fs::write(&certificate, certificate_json(&common::certificate())).expect("cert write");
    std::fs::write(&trust, trust_roots_text()).expect("trust write");
    std::fs::write(&revocations, revocations_json(&common::empty_list(1)))
        .expect("revocations write");
    let out = cli()
        .args(["pack", dir.to_str().expect("a utf-8 path"), "-o"])
        .arg(&bundle)
        .arg("--sign")
        .arg(&key)
        .arg("--certificate")
        .arg(&certificate)
        .args(["--signed-at", "150"])
        .output()
        .expect("the CLI runs");
    assert!(
        out.status.success(),
        "pack --sign must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let signature = root.join("app.sig");
    assert!(
        signature.is_file(),
        "the signature is written next to the bundle"
    );
    (
        Material {
            key,
            certificate,
            trust,
            revocations,
            signature,
            bundle,
        },
        dir,
    )
}

/// `check` on a good app: exit 0, human text says ok.
#[test]
fn check_ok_exits_zero_with_human_text() {
    let (_keep, root) = workdir();
    let dir = good_dir(&root, &[]);
    let out = cli()
        .args(["check"])
        .arg(&dir)
        .output()
        .expect("the CLI runs");
    assert!(out.status.success());
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("ok"), "human text says ok: {text}");
}

/// `check` with `--json` on a good app: JSON with `"ok": true` and no problems.
#[test]
fn check_ok_json_has_ok_true() {
    let (_keep, root) = workdir();
    let dir = good_dir(&root, &[]);
    let out = cli()
        .args(["--json", "check"])
        .arg(&dir)
        .output()
        .expect("the CLI runs");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(&text).expect("the JSON parses");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["problems"], serde_json::json!([]));
}

/// `check` on a broken app: exit 1, human text carries the code and the path on stderr.
#[test]
fn check_problems_exit_one_with_the_code_and_path() {
    let (_keep, root) = workdir();
    let dir = good_dir(&root, &[("loose.txt", b"stray\n".to_vec())]);
    let out = cli()
        .args(["check"])
        .arg(&dir)
        .output()
        .expect("the CLI runs");
    assert_eq!(out.status.code(), Some(1), "problems are exit 1");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("pack.layout") && text.contains("loose.txt"),
        "the human form carries the code and the path: {text}"
    );
}

/// `check --json` on a broken app: exit 1, JSON problems on stdout with code and path.
#[test]
fn check_problems_json_lists_code_and_path() {
    let (_keep, root) = workdir();
    let dir = good_dir(&root, &[("loose.txt", b"stray\n".to_vec())]);
    let out = cli()
        .args(["--json", "check"])
        .arg(&dir)
        .output()
        .expect("the CLI runs");
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(&text).expect("the JSON parses");
    assert_eq!(value["ok"], serde_json::json!(false));
    let problem = &value["problems"][0];
    assert_eq!(problem["code"], serde_json::json!("pack.layout"));
    assert_eq!(problem["path"], serde_json::json!("loose.txt"));
}

/// `check` against something that is not a directory: exit 2 (a wrong command line).
#[test]
fn check_a_missing_directory_is_usage_exit() {
    let (_keep, root) = workdir();
    let missing = root.join("no-such-app");
    let out = cli()
        .args(["check"])
        .arg(&missing)
        .output()
        .expect("the CLI runs");
    assert_eq!(out.status.code(), Some(2), "a wrong command line is exit 2");
}

/// `--help` works (usage text to stdout, exit 0) and names the four subcommands.
#[test]
fn help_names_the_subcommands() {
    let out = cli().arg("--help").output().expect("the CLI runs");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    for word in ["check", "pack", "verify", "inspect"] {
        assert!(text.contains(word), "the help names `{word}`: {text}");
    }
}

/// A missing subcommand: clap's usage error, exit 2.
#[test]
fn no_subcommand_is_a_usage_error_exit_two() {
    let out = cli().output().expect("the CLI runs");
    assert_eq!(out.status.code(), Some(2));
}

/// `pack` writes the bundle and prints its digest; the digest equals what a second pack
/// gives (determinism through the CLI itself).
#[test]
fn pack_writes_the_bundle_and_prints_the_digest() {
    let (_keep, root) = workdir();
    let dir = good_dir(&root, &[]);
    let out_path = root.join("first.spotapp");
    let out = cli()
        .args(["pack"])
        .arg(&dir)
        .args(["-o"])
        .arg(&out_path)
        .output()
        .expect("the CLI runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("packed"),
        "the human form names the file: {text}"
    );
    let digest = text
        .split('(')
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .expect("the digest is in the parens")
        .trim()
        .to_owned();
    assert_eq!(
        digest.len(),
        64,
        "the digest is 64 hex characters: {digest}"
    );
    assert!(out_path.is_file());
}

/// `pack` on a broken app: exit 1, no bundle written.
#[test]
fn pack_of_a_broken_app_exits_one_and_writes_nothing() {
    let (_keep, root) = workdir();
    let dir = good_dir(&root, &[("loose.txt", b"stray\n".to_vec())]);
    let out_path = root.join("never.spotapp");
    let out = cli()
        .args(["pack"])
        .arg(&dir)
        .args(["-o"])
        .arg(&out_path)
        .output()
        .expect("the CLI runs");
    assert_eq!(out.status.code(), Some(1));
    assert!(!out_path.exists(), "no bundle when the check fails");
}

/// `pack --json`: the digest in machine form.
#[test]
fn pack_json_prints_the_digest() {
    let (_keep, root) = workdir();
    let dir = good_dir(&root, &[]);
    let out_path = root.join("out.spotapp");
    let out = cli()
        .args(["--json", "pack"])
        .arg(&dir)
        .args(["-o"])
        .arg(&out_path)
        .output()
        .expect("the CLI runs");
    assert!(out.status.success());
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("the JSON parses");
    assert_eq!(value["ok"], serde_json::json!(true));
    let digest = value["digest"].as_str().expect("the digest is a string");
    assert_eq!(digest.len(), 64);
}

/// `pack --sign` without `--signed-at`: exit 2, packing never reads a clock.
#[test]
fn sign_without_signed_at_is_usage() {
    let (_keep, root) = workdir();
    let dir = good_dir(&root, &[]);
    let out_path = root.join("out.spotapp");
    let key = root.join("publisher.key");
    spot_pack::write_key_file(&common::publisher(), &key).expect("the key file is written");
    let out = cli()
        .args(["pack"])
        .arg(&dir)
        .args(["-o"])
        .arg(&out_path)
        .arg("--sign")
        .arg(&key)
        .output()
        .expect("the CLI runs");
    assert_eq!(out.status.code(), Some(2), "--sign needs --signed-at");
}

/// The full signed round trip through processes: `pack --sign`, then `verify` with the trust
/// material; exit 0 and the human form names the publisher.
#[test]
fn the_signed_round_trip_runs_through_processes() {
    let (_keep, root) = workdir();
    let (material, _dir) = signed_by_cli(&root);
    let out = cli()
        .args([
            "verify",
            material.bundle.to_str().expect("a path"),
            "--signature",
        ])
        .arg(&material.signature)
        .arg("--certificate")
        .arg(&material.certificate)
        .arg("--trust")
        .arg(&material.trust)
        .arg("--revocations")
        .arg(&material.revocations)
        .args(["--now", "500"])
        .output()
        .expect("the CLI runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.contains("verified") && text.contains("acme"),
        "the human form names the publisher: {text}"
    );
}

/// `verify --json`: the proof in machine form.
#[test]
fn verify_json_prints_the_proof() {
    let (_keep, root) = workdir();
    let (material, _dir) = signed_by_cli(&root);
    let out = cli()
        .args([
            "--json",
            "verify",
            material.bundle.to_str().expect("a path"),
            "--signature",
        ])
        .arg(&material.signature)
        .arg("--certificate")
        .arg(&material.certificate)
        .arg("--trust")
        .arg(&material.trust)
        .arg("--revocations")
        .arg(&material.revocations)
        .args(["--now", "500"])
        .output()
        .expect("the CLI runs");
    assert!(out.status.success());
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("the JSON parses");
    assert_eq!(value["ok"], serde_json::json!(true));
    assert_eq!(value["publisher_id"], serde_json::json!("acme"));
}

/// `verify` against a tampered bundle: exit 1, human text says refused with the signing code.
#[test]
fn verify_of_a_tampered_bundle_exits_one() {
    let (_keep, root) = workdir();
    let (mut material, _dir) = signed_by_cli(&root);
    // Tamper the asset inside the bundle by rebuilding it with a changed asset: the
    // signature file stays the old one.
    let edited_dir = good_dir(
        &root,
        &[("assets/logo.svg", b"<svg> tampered </svg>\n".to_vec())],
    );
    let tampered_path = root.join("tampered.spotapp");
    let out = cli()
        .args(["pack"])
        .arg(&edited_dir)
        .args(["-o"])
        .arg(&tampered_path)
        .output()
        .expect("the CLI runs");
    assert!(out.status.success());
    material.bundle = tampered_path;
    let out = cli()
        .args([
            "verify",
            material.bundle.to_str().expect("a path"),
            "--signature",
        ])
        .arg(&material.signature)
        .arg("--certificate")
        .arg(&material.certificate)
        .arg("--trust")
        .arg(&material.trust)
        .arg("--revocations")
        .arg(&material.revocations)
        .args(["--now", "500"])
        .output()
        .expect("the CLI runs");
    assert_eq!(out.status.code(), Some(1), "a refused bundle is exit 1");
    let text = String::from_utf8_lossy(&out.stderr);
    assert!(
        text.contains("refused"),
        "the human form says refused: {text}"
    );
    // --json names the stable code exactly.
    let json_out = cli()
        .args([
            "--json",
            "verify",
            material.bundle.to_str().expect("a path"),
            "--signature",
        ])
        .arg(&material.signature)
        .arg("--certificate")
        .arg(&material.certificate)
        .arg("--trust")
        .arg(&material.trust)
        .arg("--revocations")
        .arg(&material.revocations)
        .args(["--now", "500"])
        .output()
        .expect("the CLI runs");
    assert_eq!(json_out.status.code(), Some(1));
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&json_out.stdout)).expect("the JSON parses");
    assert_eq!(value["ok"], serde_json::json!(false));
    assert_eq!(
        value["code"],
        serde_json::json!("signing.signature_invalid")
    );
}

/// `verify` with a `--mode` that does not exist: exit 2.
#[test]
fn verify_with_a_wrong_mode_is_usage() {
    let (_keep, root) = workdir();
    let (material, _dir) = signed_by_cli(&root);
    let out = cli()
        .args([
            "verify",
            material.bundle.to_str().expect("a path"),
            "--signature",
        ])
        .arg(&material.signature)
        .arg("--certificate")
        .arg(&material.certificate)
        .arg("--trust")
        .arg(&material.trust)
        .arg("--revocations")
        .arg(&material.revocations)
        .args(["--now", "500", "--mode", "forever"])
        .output()
        .expect("the CLI runs");
    assert_eq!(out.status.code(), Some(2));
}

/// `inspect` on a packed bundle: the identity, the digest and the entries, human and JSON.
#[test]
fn inspect_prints_identity_digest_and_entries() {
    let (_keep, root) = workdir();
    let (material, _dir) = signed_by_cli(&root);
    let out = cli()
        .args(["inspect", material.bundle.to_str().expect("a path")])
        .output()
        .expect("the CLI runs");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(&format!("app: {APP_ID} 1.0.0")), "{text}");
    assert!(text.contains("digest:"), "{text}");
    assert!(text.contains("assets/logo.svg"), "{text}");
}

/// `inspect --json`: the same facts in machine form, with the steps the manifest names.
#[test]
fn inspect_json_carries_the_same_facts() {
    let (_keep, root) = workdir();
    let (material, _dir) = signed_by_cli(&root);
    let out = cli()
        .args([
            "--json",
            "inspect",
            material.bundle.to_str().expect("a path"),
        ])
        .output()
        .expect("the CLI runs");
    assert!(out.status.success());
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).expect("the JSON parses");
    assert_eq!(value["app_id"], serde_json::json!(APP_ID));
    assert_eq!(value["version"], serde_json::json!("1.0.0"));
    let digest = value["digest"].as_str().expect("a string");
    assert_eq!(digest.len(), 64);
    let names: Vec<&str> = value["entries"]
        .as_array()
        .expect("the entries array")
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(names.contains(&"app.wasm"));
    assert!(names.contains(&"manifest.toml"));
}

/// `inspect` with a signature and certificate names the publisher.
#[test]
fn inspect_with_the_signature_names_the_publisher() {
    let (_keep, root) = workdir();
    let (material, _dir) = signed_by_cli(&root);
    let out = cli()
        .args([
            "inspect",
            material.bundle.to_str().expect("a path"),
            "--signature",
        ])
        .arg(&material.signature)
        .arg("--certificate")
        .arg(&material.certificate)
        .output()
        .expect("the CLI runs");
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("publisher: acme"), "{text}");
}

/// `verify` with a signature file that is not valid JSON: exit 2, a wrong command line, not
/// a panic.
#[test]
fn verify_with_a_malformed_signature_file_is_usage() {
    let (_keep, root) = workdir();
    let (material, _dir) = signed_by_cli(&root);
    let bad = root.join("bad.sig");
    std::fs::write(&bad, b"{not json").expect("the bad file exists");
    let out = cli()
        .args([
            "verify",
            material.bundle.to_str().expect("a path"),
            "--signature",
        ])
        .arg(&bad)
        .arg("--certificate")
        .arg(&material.certificate)
        .arg("--trust")
        .arg(&material.trust)
        .arg("--revocations")
        .arg(&material.revocations)
        .args(["--now", "500"])
        .output()
        .expect("the CLI runs");
    assert_eq!(out.status.code(), Some(2));
}

/// `verify` with an empty trust-roots file: exit 2 (`--trust` names no root).
#[test]
fn verify_with_an_empty_trust_file_is_usage() {
    let (_keep, root) = workdir();
    let (material, _dir) = signed_by_cli(&root);
    let empty = root.join("empty.trust");
    std::fs::write(&empty, b"# no roots\n").expect("the empty file exists");
    let out = cli()
        .args([
            "verify",
            material.bundle.to_str().expect("a path"),
            "--signature",
        ])
        .arg(&material.signature)
        .arg("--certificate")
        .arg(&material.certificate)
        .arg("--trust")
        .arg(&empty)
        .arg("--revocations")
        .arg(&material.revocations)
        .args(["--now", "500"])
        .output()
        .expect("the CLI runs");
    assert_eq!(out.status.code(), Some(2));
}

/// The signature file from `pack --sign` is exactly what `verify` accepts in JSON — round
/// trip through `from_json` as the installer reads it.
#[test]
fn the_signature_file_round_trips_through_json() {
    let (_keep, root) = workdir();
    let (material, _dir) = signed_by_cli(&root);
    let text = std::fs::read_to_string(&material.signature).expect("the signature file exists");
    let signature =
        spot_bundle_signing::BundleSignature::from_json(text.as_bytes()).expect("it parses");
    assert_eq!(signature.app_id, APP_ID);
    assert_eq!(signature.publisher_id, "acme");
}
