//! The air-gap integration suite: import from media, tamper and trust refusals, downgrade,
//! resumable import, the command and result bundles, the OS mirror and the export builder.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::path::Path;

use common::{
    Authority, MemoryResultSink, TestSigner, audit_segment, import_media, manifest_of, seed_media,
    spec, std_storage,
};
use semver::Version;
use spot_airgap::storage::MemoryStorage;
use spot_airgap::{
    AgentTrust, BundleBuilder, BundleKind, BundleTrust, CommandBundle, CommandResult, LocalRepo,
    MirrorManifest, MirrorPackage, ResultBundle, ResultImport, ResultOutcome, Storage, import,
    import_result, sign_result, verify_mirror,
};
use spot_signed_command::{
    Command, CommandVerifier, MemoryNonceStore, Signer as _, SoftwareSigner, TrustedKeys,
    VerifyPolicy, sign,
};
use spot_updater::{
    Digest as UpdaterDigest, KeyId as UpdaterKeyId, PublicKey as UpdaterPublicKey, SigningKey,
    TrustedUpdateKey, UpdateRevocation, UpdateRevocationBody, UpdateTrust,
};

const MEDIA: &str = "/media";
const REPO: &str = "/repo";

fn media() -> std::path::PathBuf {
    std::path::PathBuf::from(MEDIA)
}

fn repo() -> std::path::PathBuf {
    std::path::PathBuf::from(REPO)
}

/// A seeded media bundle and its manifest.
fn seeded(
    authority: &Authority,
    version: &str,
    files: &[(&str, &[u8])],
) -> (MemoryStorage, spot_airgap::OfflineManifest) {
    let storage = MemoryStorage::new();
    let manifest = manifest_of(BundleKind::Runtime, "spot-runtime", version, 2_000, files);
    let manifest = seed_media(&storage, &media(), authority, &manifest, files);
    (storage, manifest)
}

#[test]
fn import_happy_path_is_atomic_and_idempotent() {
    let authority = Authority::new();
    let revocations = authority.revocations(1, Vec::new());
    let files: &[(&str, &[u8])] = &[("bin/spot", b"binary"), ("README", b"hello")];
    let (storage, manifest) = seeded(&authority, "2.1.0", files);

    let report = import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap();
    assert_eq!(report.version.to_string(), "2.1.0");
    assert_eq!(report.files, 2);
    assert!(!report.resumed);
    assert!(!report.already_present);
    assert_eq!(report.digest, manifest.digest());

    // The bundle landed whole, and the index records it.
    let local = LocalRepo::new(repo());
    assert!(
        storage
            .exists(&local.bundle_dir(
                BundleKind::Runtime,
                "spot-runtime",
                &Version::parse("2.1.0").unwrap()
            ))
            .unwrap()
    );
    assert_eq!(local.read_index(&storage).unwrap().entries.len(), 1);

    // A second import is a no-op.
    let again = import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap();
    assert!(again.already_present);
    assert_eq!(local.read_index(&storage).unwrap().entries.len(), 1);
}

#[test]
fn a_tampered_manifest_fails_the_signature() {
    let authority = Authority::new();
    let revocations = authority.revocations(1, Vec::new());
    let files: &[(&str, &[u8])] = &[("bin/spot", b"binary")];
    let (storage, _) = seeded(&authority, "2.1.0", files);

    // Change a signed field: the digest the signature covers no longer matches.
    let tampered = "format = \"spot-offline-manifest\"\nkind = \"runtime\"\nname = \"spot-runtime\"\n\
                    version = \"2.1.0\"\nreleased_at = 999999999\n[[files]]\npath = \"bin/spot\"\n\
                    size = 6\nsha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n";
    storage.put_file(&media().join("offline.toml"), tampered.as_bytes());
    let error = import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap_err();
    assert_eq!(error.code(), "airgap.signature_invalid");
}

#[test]
fn a_tampered_payload_file_is_located() {
    let authority = Authority::new();
    let revocations = authority.revocations(1, Vec::new());
    let files: &[(&str, &[u8])] = &[("bin/spot", b"binary")];
    let (storage, _) = seeded(&authority, "2.1.0", files);
    storage.put_file(&media().join("files").join("bin/spot"), b"binare");
    let error = import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap_err();
    assert_eq!(error.code(), "airgap.file_digest");
}

#[test]
fn a_missing_payload_file_is_refused() {
    let authority = Authority::new();
    let revocations = authority.revocations(1, Vec::new());
    let files: &[(&str, &[u8])] = &[("bin/spot", b"binary"), ("README", b"hello")];
    let (storage, _) = seeded(&authority, "2.1.0", files);
    storage
        .remove_file(&media().join("files").join("README"))
        .unwrap();
    let error = import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap_err();
    assert_eq!(error.code(), "airgap.file_missing");
}

#[test]
fn an_extra_file_on_the_media_is_refused() {
    let authority = Authority::new();
    let revocations = authority.revocations(1, Vec::new());
    let files: &[(&str, &[u8])] = &[("bin/spot", b"binary")];
    let (storage, _) = seeded(&authority, "2.1.0", files);
    storage.put_file(&media().join("files").join("sneaky"), b"extra");
    let error = import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap_err();
    assert_eq!(error.code(), "airgap.extra_file");
}

#[test]
fn an_untrusted_root_is_refused() {
    let authority = Authority::new();
    let revocations = authority.revocations(1, Vec::new());
    let files: &[(&str, &[u8])] = &[("bin/spot", b"binary")];
    let (storage, _) = seeded(&authority, "2.1.0", files);
    let anchor = Authority::other_root();
    let local = LocalRepo::new(repo());
    let error = import(
        &storage,
        &spot_airgap::ImportRequest {
            media: &media(),
            repo: &local,
            trust: BundleTrust {
                anchor: &anchor,
                revocations: &revocations,
                now: authority.now,
                held_revocation_generation: 0,
            },
            imported_at: authority.now,
        },
    )
    .unwrap_err();
    assert_eq!(error.code(), "airgap.unknown_root");
}

#[test]
fn the_wrong_signer_cannot_produce_a_bundle() {
    let authority = Authority::new();
    let stranger = spot_bundle_signing::generate_keypair(&[7u8; 32]);
    let manifest = manifest_of(
        BundleKind::Runtime,
        "spot-runtime",
        "2.1.0",
        2_000,
        &[("a", b"x")],
    );
    // The certificate names the authority's publisher key, not the stranger's.
    let result = spot_bundle_signing::sign_bundle(
        &manifest.signing_digest().unwrap(),
        &manifest.name,
        &manifest.version.to_string(),
        &authority.certificate,
        &stranger,
        authority.signed_at,
    );
    assert!(result.is_err());
}

#[test]
fn a_revoked_signing_key_is_refused() {
    let authority = Authority::new();
    let revoked = spot_bundle_signing::RevokedKey {
        key_id: authority.publisher.public().key_id(),
        effective_from: authority.signed_at,
        compromised: false,
        reason: "retired".to_owned(),
    };
    let revocations = authority.revocations(2, vec![revoked]);
    let files: &[(&str, &[u8])] = &[("bin/spot", b"binary")];
    let (storage, _) = seeded(&authority, "2.1.0", files);
    let error = import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap_err();
    assert_eq!(error.code(), "airgap.key_revoked");
}

#[test]
fn a_downgrade_is_refused() {
    let authority = Authority::new();
    let revocations = authority.revocations(1, Vec::new());
    let (storage, _) = seeded(&authority, "2.1.0", &[("bin/spot", b"binary")]);
    import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap();

    // A second media carries an older version.
    let older = manifest_of(
        BundleKind::Runtime,
        "spot-runtime",
        "1.9.0",
        2_000,
        &[("bin/spot", b"older")],
    );
    seed_media(
        &storage,
        &media(),
        &authority,
        &older,
        &[("bin/spot", b"older")],
    );
    let error = import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap_err();
    assert_eq!(error.code(), "airgap.downgrade");
}

#[test]
fn a_changed_digest_for_the_same_version_is_refused() {
    let authority = Authority::new();
    let revocations = authority.revocations(1, Vec::new());
    let (storage, _) = seeded(&authority, "2.1.0", &[("bin/spot", b"first")]);
    import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap();

    let changed = manifest_of(
        BundleKind::Runtime,
        "spot-runtime",
        "2.1.0",
        2_000,
        &[("bin/spot", b"second")],
    );
    seed_media(
        &storage,
        &media(),
        &authority,
        &changed,
        &[("bin/spot", b"second")],
    );
    let error = import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap_err();
    assert_eq!(error.code(), "airgap.digest_changed");
}

#[test]
fn an_interrupted_import_resumes() {
    let authority = Authority::new();
    let revocations = authority.revocations(1, Vec::new());
    let files: &[(&str, &[u8])] = &[("bin/spot", b"binary"), ("lib/libspot.so", b"library")];
    let (storage, _) = seeded(&authority, "2.1.0", files);

    // Fail on the first payload write (the marker is write 0), leaving a partial stage.
    storage.fail_write_at(1);
    let error = import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap_err();
    assert_eq!(error.code(), "airgap.io");
    let local = LocalRepo::new(repo());
    assert!(
        local.read_index(&storage).unwrap().entries.is_empty(),
        "nothing is indexed yet"
    );
    let staging = local.staging_dir(
        BundleKind::Runtime,
        "spot-runtime",
        &Version::parse("2.1.0").unwrap(),
    );
    assert!(
        storage.exists(&staging).unwrap(),
        "the partial stage is left behind"
    );

    // Retry: the marker matches, so the import resumes and finishes.
    storage.clear_failure();
    let report = import_media(&storage, &media(), &repo(), &authority, &revocations).unwrap();
    assert!(report.resumed, "the retry resumes the partial import");
    assert!(!report.already_present);
    assert_eq!(local.read_index(&storage).unwrap().entries.len(), 1);
}

#[test]
fn a_command_bundle_replayed_on_the_agent_is_refused_by_nonce() {
    let signer = SoftwareSigner::from_seed(&[7u8; 32]).unwrap();
    let mut trusted = TrustedKeys::new();
    trusted
        .insert(signer.key_id(), &signer.public_key_bytes())
        .unwrap();
    let verifier = CommandVerifier::new(
        trusted,
        VerifyPolicy::default_policy(),
        Box::new(MemoryNonceStore::new()),
    );
    let command = Command::new(
        "server.service.restart",
        serde_json::json!({"service": "spot-runtime"}),
        "agent-1",
        signer.key_id(),
        2_000,
        2_500,
    );
    let signed = sign(&command, &signer).unwrap();
    let mut keys = BTreeMap::new();
    keys.insert(
        signer.key_id().as_str().to_owned(),
        signer.public_key_bytes(),
    );
    let bytes = CommandBundle::pack(&[signed], &keys).unwrap();
    let bundle = CommandBundle::unpack(&bytes).unwrap();
    assert_eq!(bundle.operations().unwrap().len(), 1);

    let first = futures::executor::block_on(bundle.commands[0].verify(&verifier, "agent-1", 2_100))
        .unwrap();
    assert_eq!(first.command.id(), command.id());
    let replay =
        futures::executor::block_on(bundle.commands[0].verify(&verifier, "agent-1", 2_100))
            .unwrap_err();
    assert_eq!(replay.code(), "airgap.command_rejected");
}

#[test]
fn a_result_bundle_reimport_is_idempotent() {
    let signer = SoftwareSigner::from_seed(&[8u8; 32]).unwrap();
    let public = signer.public_key_bytes();
    let key_id = UpdaterKeyId::parse(signer.key_id().as_str()).unwrap();
    let keys = UpdateTrust::new(vec![TrustedUpdateKey::new(
        UpdaterPublicKey::from_bytes(&public).unwrap(),
        1_000,
        None,
    )]);
    let revocations = UpdateRevocation::issue(
        UpdateRevocationBody {
            generation: 1,
            issued_at: 2_000,
            issued_by: key_id,
            keys: Vec::new(),
        },
        &SigningKey::from_seed(&[8u8; 32]),
    );
    let trust = AgentTrust {
        keys: &keys,
        revocations: &revocations,
        now: 3_000,
        held_revocation_generation: 0,
    };
    let bundle = ResultBundle {
        id: "result-1".to_owned(),
        agent_id: "agent-1".to_owned(),
        produced_at: 2_000,
        results: vec![CommandResult {
            command_id: "abc".to_owned(),
            op: "server.service.restart".to_owned(),
            outcome: ResultOutcome::Applied,
            detail: None,
        }],
        audit: audit_segment(b"audit-entries"),
    };
    let signature = sign_result(&bundle, &signer).unwrap();

    let mut sink = MemoryResultSink::default();
    assert_eq!(
        import_result(&mut sink, &bundle, Some(&signature), &trust).unwrap(),
        ResultImport::Imported
    );
    assert_eq!(
        import_result(&mut sink, &bundle, Some(&signature), &trust).unwrap(),
        ResultImport::AlreadyImported
    );

    // The same id with different content is a conflict.
    let mut changed = bundle.clone();
    changed.produced_at += 1;
    let changed_signature = sign_result(&changed, &signer).unwrap();
    let error = import_result(&mut sink, &changed, Some(&changed_signature), &trust).unwrap_err();
    assert_eq!(error.code(), "airgap.result_conflict");
}

#[test]
fn a_result_bundle_with_a_broken_audit_segment_is_refused() {
    let signer = SoftwareSigner::from_seed(&[8u8; 32]).unwrap();
    let public = signer.public_key_bytes();
    let key_id = UpdaterKeyId::parse(signer.key_id().as_str()).unwrap();
    let keys = UpdateTrust::new(vec![TrustedUpdateKey::new(
        UpdaterPublicKey::from_bytes(&public).unwrap(),
        1_000,
        None,
    )]);
    let revocations = UpdateRevocation::issue(
        UpdateRevocationBody {
            generation: 1,
            issued_at: 2_000,
            issued_by: key_id,
            keys: Vec::new(),
        },
        &SigningKey::from_seed(&[8u8; 32]),
    );
    let trust = AgentTrust {
        keys: &keys,
        revocations: &revocations,
        now: 3_000,
        held_revocation_generation: 0,
    };
    let mut audit = audit_segment(b"audit-entries");
    audit.head_hex = "0".repeat(64);
    let bundle = ResultBundle {
        id: "result-2".to_owned(),
        agent_id: "agent-1".to_owned(),
        produced_at: 2_000,
        results: Vec::new(),
        audit,
    };
    let signature = sign_result(&bundle, &signer).unwrap();
    let mut sink = MemoryResultSink::default();
    let error = import_result(&mut sink, &bundle, Some(&signature), &trust).unwrap_err();
    assert_eq!(error.code(), "airgap.audit_segment");
}

#[test]
fn the_mirror_verifies_and_locates_a_tampered_package() {
    let storage = MemoryStorage::new();
    let root = Path::new("/mirror");
    let packages: &[(&str, &[u8])] = &[("a.rpm", b"package-a"), ("b.rpm", b"package-b")];
    for (path, bytes) in packages {
        let target = root.join("packages").join(path);
        storage.mkdir(target.parent().unwrap());
        storage.put_file(&target, bytes);
    }
    let manifest = MirrorManifest {
        name: "rhel9-base".to_owned(),
        generated_at: 2_000,
        packages: packages
            .iter()
            .map(|(path, bytes)| MirrorPackage {
                path: (*path).to_owned(),
                size: bytes.len() as u64,
                sha256: UpdaterDigest::of(bytes),
            })
            .collect(),
    };
    assert_eq!(
        verify_mirror(&storage, root, &manifest).unwrap().packages,
        2
    );

    storage.put_file(&root.join("packages").join("a.rpm"), b"tampered");
    let error = verify_mirror(&storage, root, &manifest).unwrap_err();
    assert_eq!(error.code(), "airgap.mirror_digest");
}

#[test]
fn the_export_builder_round_trips_through_import() {
    let authority = Authority::new();
    let revocations = authority.revocations(1, Vec::new());
    let signer = TestSigner {
        certificate: authority.certificate.clone(),
        key: spot_bundle_signing::generate_keypair(&[2u8; 32]),
    };
    let storage = MemoryStorage::new();
    let release = Path::new("/release");
    let out = Path::new("/out");
    storage.mkdir(release);
    storage.put_file(&release.join("bin/spot"), b"binary");
    storage.put_file(&release.join("README"), b"hello");

    let builder = BundleBuilder::new(spec(BundleKind::Runtime, "spot-runtime", "2.1.0"), &signer);
    let manifest = builder.assemble(&storage, release, out).unwrap();
    assert_eq!(manifest.files.len(), 2);

    let report = import_media(&storage, out, &repo(), &authority, &revocations).unwrap();
    assert_eq!(report.version.to_string(), "2.1.0");
    assert_eq!(report.files, 2);
}

#[test]
fn std_storage_round_trips_on_a_real_filesystem() {
    let authority = Authority::new();
    let revocations = authority.revocations(1, Vec::new());
    let storage = std_storage();
    let temp = tempfile::tempdir().unwrap();
    let release = temp.path().join("release");
    let out = temp.path().join("media");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(release.join("bin")).unwrap();
    std::fs::write(release.join("bin/spot"), b"binary").unwrap();

    let signer = TestSigner {
        certificate: authority.certificate.clone(),
        key: spot_bundle_signing::generate_keypair(&[2u8; 32]),
    };
    let builder = BundleBuilder::new(spec(BundleKind::Runtime, "spot-runtime", "2.1.0"), &signer);
    builder.assemble(&storage, &release, &out).unwrap();

    let local = LocalRepo::new(&repo);
    let report = import(
        &storage,
        &spot_airgap::ImportRequest {
            media: &out,
            repo: &local,
            trust: authority.trust(&revocations),
            imported_at: authority.now,
        },
    )
    .unwrap();
    assert_eq!(report.files, 1);
    assert!(
        repo.join("bundles/runtime/spot-runtime/2.1.0/bin/spot")
            .exists()
    );
    assert!(repo.join("index.toml").exists());
}
