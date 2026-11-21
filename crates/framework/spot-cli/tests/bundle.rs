//! `spot bundle` (WP-ALM-02.T3): the hand-over file, the direction rule, the key policy and the
//! import evaluation, driven without a process over in-memory sites; and the commands over a real
//! host with PostgreSQL (`SPOT_TEST_PG_URL`, skipped with a printed reason when unset).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

use serde_json::json;
use spot_bundle_signing::{
    KeyPair, PublisherCertificate, RevocationBody, SignedCertificate, TrustAnchor, TrustedRoot,
    generate_keypair, issue_certificate, issue_revocation_list,
};
use spot_custom_bundle::{Bundle, CaptureOptions, FakeSite, SiteReader, capture};
use spot_environments::Role;
use spot_meta::{Definition, FieldSpec, FieldType};

/// The command modules, driven without a process.
#[path = "../src/commands/mod.rs"]
mod commands;

use commands::bundle::{
    AppliedMeta, EnvRecord, HandOverHeader, ImageBase, ImportContext, Trust, build_image,
    check_direction, decode_handover, encode_handover, evaluate, hex_decode, load_trust,
    make_handover, next_role, parse_anchor, read_env, read_seed, role_of,
};

const NOW: i64 = 500;

fn root() -> KeyPair {
    generate_keypair(&[1_u8; 32])
}

fn publisher() -> KeyPair {
    generate_keypair(&[2_u8; 32])
}

fn certificate() -> SignedCertificate {
    issue_certificate(
        PublisherCertificate {
            publisher_id: "acme-software".to_owned(),
            display_name: "Acme Software".to_owned(),
            key: publisher().public(),
            valid_from: 100,
            valid_to: 1000,
            issued_by: root().public().key_id(),
            serial: 7,
        },
        &root(),
    )
}

fn trust_of(root_key: &KeyPair) -> Trust {
    Trust {
        anchor: TrustAnchor::new(vec![TrustedRoot::new(root_key.public(), 0, None)]),
        revocations: issue_revocation_list(
            RevocationBody {
                generation: 1,
                issued_at: 0,
                issued_by: root_key.public().key_id(),
                keys: Vec::new(),
                publishers: Vec::new(),
                bundles: Vec::new(),
            },
            root_key,
        ),
    }
}

fn definition() -> Definition {
    let mut definition = Definition::new("sales_invoice");
    definition.app = Some("accounting".to_owned());
    let mut note = FieldSpec::new("note", FieldType::Text);
    note.max_chars = Some(100);
    definition.fields = vec![note];
    definition
}

fn empty_site() -> FakeSite {
    FakeSite::new()
        .with_app("accounting", semver::Version::new(1, 2, 3))
        .with_definition(definition())
}

fn source_site() -> FakeSite {
    let mut risk = FieldSpec::new("custom_risk_class", FieldType::Text);
    risk.max_chars = Some(50);
    empty_site()
        .with_custom_field("sales_invoice", risk, None)
        .with_rule("positive_qty", "doc.qty > 0")
        .with_fixtures("invoice", vec![json!({ "name": "invoice-one" })])
}

fn source_bundle() -> Bundle {
    capture(
        &source_site(),
        &CaptureOptions::new("acme", "1.0.0", "glm", "2026-11-12T08:00:00Z"),
    )
    .unwrap()
}

fn handover(source: Role, target: Role) -> Vec<u8> {
    make_handover(
        &source_bundle(),
        "dev-site",
        source,
        target,
        "glm",
        &publisher(),
        &certificate(),
        NOW,
    )
    .unwrap()
}

fn context<'a>(kind: Role, trust: &'a Trust, image: &'a FakeSite) -> ImportContext<'a> {
    ImportContext {
        site: "test-site",
        kind,
        trust,
        image,
        now: NOW,
        created_at: "2026-11-12T09:00:00Z",
    }
}

fn reason(result: Result<impl Sized, commands::bundle::Refusal>) -> &'static str {
    match result {
        Ok(_) => "accepted",
        Err(refusal) => refusal.reason,
    }
}

#[test]
fn the_command_parses_its_arguments() {
    let command = commands::bundle::command();
    let export = command
        .clone()
        .try_get_matches_from([
            "bundle",
            "export",
            "--site",
            "a",
            "--out",
            "f",
            "--signing-key",
            "k",
            "--certificate",
            "c",
        ])
        .unwrap();
    assert_eq!(export.subcommand_name(), Some("export"));
    assert!(
        command
            .clone()
            .try_get_matches_from(["bundle", "export", "--site", "a"])
            .is_err(),
        "an export names its output, key and certificate"
    );
    let import = command
        .clone()
        .try_get_matches_from(["bundle", "import", "f", "--site", "b", "--apply"])
        .unwrap();
    let (_, sub) = import.subcommand().unwrap();
    assert!(sub.get_flag("apply"));
    assert!(
        command
            .clone()
            .try_get_matches_from(["bundle", "diff", "a.bundle", "b.bundle"])
            .is_ok()
    );
    assert!(
        command
            .try_get_matches_from(["bundle", "env", "link", "--site", "a", "--kind", "dev"])
            .is_ok()
    );
}

#[test]
fn a_signed_bundle_round_trips_into_the_next_environment_as_a_plan_not_a_change() {
    let trust = trust_of(&root());
    let target = empty_site();
    let outcome = evaluate(
        &handover(Role::Dev, Role::Test),
        &context(Role::Test, &trust, &target),
    )
    .unwrap();
    assert_eq!(outcome.header.source_kind, "dev");
    assert_eq!(outcome.header.digest, source_bundle().digest().to_hex());
    let keys: Vec<String> = outcome
        .plan
        .changes()
        .iter()
        .map(|c| format!("{}/{}", c.section, c.key))
        .collect();
    assert!(
        keys.iter().any(|k| k.starts_with("meta/")),
        "the custom field is planned: {keys:?}"
    );
    assert!(keys.iter().any(|k| k.starts_with("rules/")), "{keys:?}");
    assert!(keys.iter().any(|k| k.starts_with("fixtures/")), "{keys:?}");
    // A plan, not a change: the target is exactly as it was; only the in-memory copy changed.
    assert!(target.rules().is_empty());
    assert!(target.fixtures().is_empty());
    assert!(!outcome.after.rules().is_empty());
    assert!(outcome.conflicts.is_empty());
}

#[test]
fn a_tampered_archive_is_refused() {
    let trust = trust_of(&root());
    let target = empty_site();
    let mut bytes = handover(Role::Dev, Role::Test);
    let last = bytes.len() - 5;
    bytes[last] ^= 0xFF;
    let verdict = reason(evaluate(&bytes, &context(Role::Test, &trust, &target)));
    assert!(
        matches!(verdict, "invalid_bundle" | "digest_mismatch" | "unreadable"),
        "{verdict}"
    );
}

#[test]
fn a_header_edited_to_point_elsewhere_no_longer_verifies() {
    let trust = trust_of(&root());
    let target = empty_site();
    let bytes = handover(Role::Dev, Role::Test);
    let (mut header, archive) = decode_handover(&bytes).unwrap();
    header.target_kind = "prod".to_owned();
    let edited = encode_handover(&header, archive).unwrap();
    assert_eq!(
        reason(evaluate(&edited, &context(Role::Prod, &trust, &target))),
        "signature_invalid",
        "the signature binds the two kinds"
    );
    let (mut header, archive) = decode_handover(&bytes).unwrap();
    header.created_by = "someone-else".to_owned();
    let edited = encode_handover(&header, archive).unwrap();
    assert_eq!(
        reason(evaluate(&edited, &context(Role::Test, &trust, &target))),
        "signature_invalid"
    );
}

#[test]
fn an_unsigned_bundle_is_refused() {
    let trust = trust_of(&root());
    let target = empty_site();
    let bytes = handover(Role::Dev, Role::Test);
    let (mut header, archive) = decode_handover(&bytes).unwrap();
    header.signature = None;
    header.certificate = None;
    let unsigned = encode_handover(&header, archive).unwrap();
    assert_eq!(
        reason(evaluate(&unsigned, &context(Role::Test, &trust, &target))),
        "unsigned"
    );
}

#[test]
fn a_signer_the_target_does_not_trust_is_refused() {
    let other_root = generate_keypair(&[9_u8; 32]);
    let trust = trust_of(&other_root);
    let target = empty_site();
    assert_eq!(
        reason(evaluate(
            &handover(Role::Dev, Role::Test),
            &context(Role::Test, &trust, &target)
        )),
        "signature_invalid"
    );
}

#[test]
fn a_bundle_addressed_to_another_environment_is_refused() {
    let trust = trust_of(&root());
    let target = empty_site();
    // Signed for test, offered to a dev site.
    assert_eq!(
        reason(evaluate(
            &handover(Role::Dev, Role::Test),
            &context(Role::Dev, &trust, &target)
        )),
        "wrong_target"
    );
}

#[test]
fn only_one_rung_up_the_ladder_is_allowed_and_production_never_goes_down() {
    assert!(check_direction(Role::Dev, Role::Test).is_ok());
    assert!(check_direction(Role::Test, Role::Prod).is_ok());
    assert!(
        check_direction(Role::Dev, Role::Prod).is_err(),
        "no skipping"
    );
    assert!(check_direction(Role::Test, Role::Dev).is_err());
    let down = check_direction(Role::Prod, Role::Dev).unwrap_err();
    assert_eq!(down.reason, "direction");
    assert!(down.message.contains("sandbox"), "{}", down.message);
    assert_eq!(next_role(Role::Prod), None);
    // A hand-over cannot even be made against the direction.
    let refused = make_handover(
        &source_bundle(),
        "prod-site",
        Role::Prod,
        Role::Dev,
        "glm",
        &publisher(),
        &certificate(),
        NOW,
    );
    assert!(refused.is_err());
}

#[test]
fn a_conflict_with_the_targets_own_customisation_is_reported_with_both_sides() {
    let trust = trust_of(&root());
    // The target already has a rule of the same name, written differently.
    let target = empty_site().with_rule("positive_qty", "doc.qty > 1");
    let outcome = evaluate(
        &handover(Role::Dev, Role::Test),
        &context(Role::Test, &trust, &target),
    )
    .unwrap();
    let conflict = outcome
        .conflicts
        .iter()
        .find(|c| c.section() == "rules")
        .expect("the rule conflicts");
    assert_eq!(conflict.kind.slug(), "both_added_differently");
    assert!(conflict.primary.ours.is_some() && conflict.primary.theirs.is_some());
    // The plan still names the item; nothing was decided for the reviewer.
    assert!(
        outcome
            .plan
            .changes()
            .iter()
            .any(|c| c.section == "rules" && c.key.contains("positive_qty"))
    );
}

#[test]
fn the_hand_over_file_format_is_strict() {
    let bytes = handover(Role::Dev, Role::Test);
    let (header, archive) = decode_handover(&bytes).unwrap();
    assert_eq!(header.bundle_len as usize, archive.len());
    assert!(decode_handover(b"not a hand-over file").is_err());
    let mut short = bytes.clone();
    short.truncate(short.len() - 1);
    assert!(decode_handover(&short).is_err(), "the length must match");
    let mut extra: Vec<u8> = bytes;
    extra.push(0);
    assert!(decode_handover(&extra).is_err());
    let unknown = br#"{"format":"spot-bundle-handover","version":1,"surprise":true}"#;
    assert!(decode_handover(&[unknown.as_slice(), b"\n"].concat()).is_err());
}

#[test]
fn the_key_policy_is_required_and_parsed_strictly() {
    let dir = std::env::temp_dir().join(format!("spot_bundle_trust_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let bare = EnvRecord {
        kind: "test".to_owned(),
        trust_anchor: None,
        revocations: None,
    };
    assert_eq!(
        load_trust(&dir, &bare).err().unwrap().reason,
        "no_key_policy"
    );

    let public: String = root()
        .public()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    std::fs::write(
        dir.join("anchor.json"),
        format!(r#"{{"roots":[{{"public_key":"{public}","valid_from":0}}]}}"#),
    )
    .unwrap();
    std::fs::write(
        dir.join("revocations.json"),
        trust_of(&root()).revocations.to_json(),
    )
    .unwrap();
    let linked = EnvRecord {
        kind: "test".to_owned(),
        trust_anchor: Some("anchor.json".to_owned()),
        revocations: Some("revocations.json".to_owned()),
    };
    assert!(load_trust(&dir, &linked).is_ok());

    std::fs::write(dir.join("anchor.json"), r#"{"roots":[]}"#).unwrap();
    assert_eq!(
        load_trust(&dir, &linked).err().unwrap().reason,
        "key_policy_invalid"
    );
    assert!(parse_anchor(b"{\"roots\":[{\"public_key\":\"zz\",\"valid_from\":0}]}").is_err());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_environment_record_and_the_kind_names() {
    let dir = std::env::temp_dir().join(format!("spot_bundle_env_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(read_env(&dir).unwrap(), None);
    std::fs::write(dir.join("environment.toml"), "kind = \"prod\"\n").unwrap();
    assert_eq!(read_env(&dir).unwrap().unwrap().kind, "prod");
    std::fs::write(dir.join("environment.toml"), "kind = \"prod\"\nextra = 1\n").unwrap();
    assert!(read_env(&dir).is_err(), "unknown keys are refused");
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(role_of("development"), Some(Role::Dev));
    assert_eq!(role_of("staging"), None);
    assert_eq!(hex_decode("0aff"), Some(vec![0x0a, 0xff]));
    assert_eq!(hex_decode("0a f"), None);
    let seed_file = std::env::temp_dir().join(format!("spot_bundle_seed_{}", std::process::id()));
    std::fs::write(&seed_file, "ab".repeat(32)).unwrap();
    assert_eq!(read_seed(&seed_file).unwrap(), [0xab; 32]);
    std::fs::write(&seed_file, "ab").unwrap();
    assert!(read_seed(&seed_file).is_err());
    let _ = std::fs::remove_file(&seed_file);
}

#[test]
fn the_site_image_carries_the_applied_custom_fields() {
    let base = ImageBase {
        apps: vec![("accounting".to_owned(), "1.2.3".to_owned())],
        host: "0.1.0".to_owned(),
        definitions: vec![definition()],
    };
    let meta = AppliedMeta {
        fields: vec![(
            "sales_invoice".to_owned(),
            "name = \"custom_risk_class\"\ntype = \"text\"\nmax_chars = 50\n".to_owned(),
            None,
        )],
        properties: Vec::new(),
    };
    let image = build_image(&base, &meta).unwrap();
    let bundle = capture(
        &image,
        &CaptureOptions::new("n", "1.0.0", "glm", "2026-11-12T08:00:00Z"),
    )
    .unwrap();
    assert!(bundle.section("meta").is_some());
    let broken = AppliedMeta {
        fields: vec![("sales_invoice".to_owned(), "not toml {".to_owned(), None)],
        properties: Vec::new(),
    };
    assert!(build_image(&base, &broken).is_err());
}

// --- the commands over a real host -------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn export_and_import_between_two_real_sites() {
    let Some(env) = common::Env::new("").await else {
        println!("SKIPPED: SPOT_TEST_PG_URL is not set");
        return;
    };
    let root_dir = env.root.0.clone();
    let dev = env.site.clone();
    let _first_sign_in_secret = env.site();
    let test = format!("{dev}-b");
    common::ok(&root_dir, &["new-site", &test]);

    let keys = root_dir.join("keys");
    std::fs::create_dir_all(&keys).unwrap();
    std::fs::write(keys.join("seed"), hex(&publisher().to_seed_bytes())).unwrap();
    // The fixed certificate window ends in 1970; this one is valid now.
    std::fs::write(keys.join("cert.json"), {
        issue_certificate(
            PublisherCertificate {
                publisher_id: "acme-software".to_owned(),
                display_name: "Acme Software".to_owned(),
                key: publisher().public(),
                valid_from: 0,
                valid_to: 4_102_444_800,
                issued_by: root().public().key_id(),
                serial: 8,
            },
            &root(),
        )
        .to_json()
    })
    .unwrap();
    std::fs::write(
        keys.join("anchor.json"),
        format!(
            r#"{{"roots":[{{"public_key":"{}","valid_from":0}}]}}"#,
            hex(root().public().as_bytes())
        ),
    )
    .unwrap();
    std::fs::write(
        keys.join("revocations.json"),
        trust_of(&root()).revocations.to_json(),
    )
    .unwrap();

    let path = |name: &str| keys.join(name).display().to_string();
    common::ok(
        &root_dir,
        &["bundle", "env", "link", "--site", &dev, "--kind", "dev"],
    );
    common::ok(
        &root_dir,
        &[
            "bundle",
            "env",
            "link",
            "--site",
            &test,
            "--kind",
            "test",
            "--trust-anchor",
            &path("anchor.json"),
            "--revocations",
            &path("revocations.json"),
        ],
    );
    let listing = common::ok(&root_dir, &["bundle", "env", "list"]);
    assert!(listing.contains(&format!("{dev}\tdev")), "{listing}");
    // A kind never changes quietly.
    let relink = common::run(
        &root_dir,
        &["bundle", "env", "link", "--site", &dev, "--kind", "prod"],
        None,
    );
    assert_eq!(relink.status.code(), Some(2));

    let file = root_dir.join("dev-to-test.handover").display().to_string();
    let exported = common::ok(
        &root_dir,
        &[
            "bundle",
            "export",
            "--site",
            &dev,
            "--out",
            &file,
            "--signing-key",
            &path("seed"),
            "--certificate",
            &path("cert.json"),
        ],
    );
    let digest = exported
        .split("digest ")
        .nth(1)
        .and_then(|rest| rest.split(',').next())
        .unwrap()
        .to_owned();
    // The file is never overwritten.
    let again = common::run(
        &root_dir,
        &[
            "bundle",
            "export",
            "--site",
            &dev,
            "--out",
            &file,
            "--signing-key",
            &path("seed"),
            "--certificate",
            &path("cert.json"),
        ],
        None,
    );
    assert_eq!(again.status.code(), Some(2));
    // A development site cannot export to production.
    let skip = common::run(
        &root_dir,
        &[
            "bundle",
            "export",
            "--site",
            &dev,
            "--out",
            &path("skip"),
            "--to",
            "prod",
            "--signing-key",
            &path("seed"),
            "--certificate",
            &path("cert.json"),
        ],
        None,
    );
    assert_eq!(skip.status.code(), Some(2));

    let imported = common::ok(&root_dir, &["bundle", "import", &file, "--site", &test]);
    assert!(imported.contains(&digest), "{imported}");
    assert!(imported.contains("nothing was changed"), "{imported}");

    // A tampered file is refused with exit 2 and names no value.
    let mut bytes = std::fs::read(&file).unwrap();
    let last = bytes.len() - 3;
    bytes[last] ^= 0xFF;
    let tampered = root_dir.join("tampered.handover");
    std::fs::write(&tampered, bytes).unwrap();
    let refused = common::run(
        &root_dir,
        &[
            "bundle",
            "import",
            &tampered.display().to_string(),
            "--site",
            &test,
        ],
        None,
    );
    assert_eq!(refused.status.code(), Some(2));

    // The same file offered to the development site is the wrong direction and target.
    let wrong = common::run(
        &root_dir,
        &["bundle", "import", &file, "--site", &dev],
        None,
    );
    assert_eq!(wrong.status.code(), Some(2));
}
