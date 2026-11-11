//! `spot install-app <bundle>` and the audit of refused installs and resets:
//! a signed bundle verifies and unpacks; every kind of bad bundle is refused with a stable code
//! and leaves the app folder and the site file untouched; each refusal is one audit line.
//!
//! None of this needs a database: the trust gate runs before the site's database is opened.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_bundle_signing::{
    BundleSignature, KeyPair, PublisherCertificate, RevocationBody, RevocationList, RevokedKey,
    SignedCertificate, canonical::b64, generate_keypair, issue_certificate, issue_revocation_list,
};
use spot_cli::bundle_install;
use spot_cli::onetime::Issue;
use spot_cli::ops::{self, Install};
use spot_pack::{PackOptions, sign_packed};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

mod support;

const NAME: &str = "ext_app";
const SIGNED_AT: i64 = 150;
const NOW: i64 = 500;

/// A component with no imports and no exports: the eight header bytes.
const EMPTY_COMPONENT: [u8; 8] = [0x00, 0x61, 0x73, 0x6D, 0x0D, 0x00, 0x01, 0x00];

fn root_key() -> KeyPair {
    generate_keypair(&[1; 32])
}
fn publisher_key() -> KeyPair {
    generate_keypair(&[2; 32])
}
fn stranger_root() -> KeyPair {
    generate_keypair(&[9; 32])
}

fn certificate(id: &str, valid_from: i64, valid_to: i64, root: &KeyPair) -> SignedCertificate {
    issue_certificate(
        PublisherCertificate {
            publisher_id: id.to_owned(),
            display_name: "Acme".to_owned(),
            key: publisher_key().public(),
            valid_from,
            valid_to,
            issued_by: root.public().key_id(),
            serial: 7,
        },
        root,
    )
}

fn list(revoking: Option<&KeyPair>) -> RevocationList {
    issue_revocation_list(
        RevocationBody {
            generation: 1,
            issued_at: 0,
            issued_by: root_key().public().key_id(),
            keys: revoking
                .map(|k| RevokedKey {
                    key_id: k.public().key_id(),
                    effective_from: 100,
                    compromised: false,
                    reason: "test".to_owned(),
                })
                .into_iter()
                .collect(),
            publishers: Vec::new(),
            bundles: Vec::new(),
        },
        &root_key(),
    )
}

struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(tag: &str, require_signed: bool) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "spot_cli_bundle_{tag}_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("trust")).unwrap();
        std::fs::create_dir_all(dir.join("incoming")).unwrap();
        std::fs::create_dir_all(dir.join("sites/s1")).unwrap();
        // The place the operator lists for the app: empty, with the empty `doctypes` directory.
        std::fs::create_dir_all(dir.join("apps").join(NAME).join("doctypes")).unwrap();
        std::fs::write(
            dir.join("common.toml"),
            format!(
                "apps = ['apps/{NAME}']\n[server]\nbind = ['127.0.0.1:0']\n\
                 [postgres]\nadmin_url = 'postgres://nobody@127.0.0.1:1/none'\n\
                 [trust]\nroots = 'trust/roots.txt'\nrevocations = 'trust/revocations.json'\n\
                 require_signed = {require_signed}\n"
            ),
        )
        .unwrap();
        std::fs::write(
            dir.join("sites/s1/site.toml"),
            "site = 's1'\ndatabase = 's1db'\napps = ['platform']\n",
        )
        .unwrap();
        let fixture = Self { dir };
        fixture.pin(&root_key(), &list(None));
        fixture
    }

    fn pin(&self, root: &KeyPair, revocations: &RevocationList) {
        std::fs::write(
            self.dir.join("trust/roots.txt"),
            format!("# the pinned root\n{}\n", b64(root.public().as_bytes())),
        )
        .unwrap();
        std::fs::write(
            self.dir.join("trust/revocations.json"),
            revocations.to_json(),
        )
        .unwrap();
    }

    /// Packs an app whose one asset holds `asset`, returns the bundle bytes.
    fn pack(&self, asset: &str) -> Vec<u8> {
        let app = self.dir.join("src_app");
        let _ = std::fs::remove_dir_all(&app);
        std::fs::create_dir_all(app.join("assets")).unwrap();
        // The packer wants the DocType directory; the bundle format has no place for it.
        std::fs::create_dir_all(app.join("doctypes")).unwrap();
        std::fs::write(
            app.join("doctypes/customer.toml"),
            format!(
                "name = \"customer\"\nlabel = \"Customer\"\napp = \"{NAME}\"\nmodule = \"sales\"\n\n\
                 [[fields]]\nname = \"customer_name\"\nlabel = \"Customer Name\"\ntype = \"text\"\nrequired = true\n"
            ),
        )
        .unwrap();
        std::fs::write(
            app.join("manifest.toml"),
            format!(
                "[app]\nname = \"{NAME}\"\nversion = \"1.0.0\"\n\n[[assets]]\npath = \"assets/note.txt\"\n"
            ),
        )
        .unwrap();
        std::fs::write(app.join("app.wasm"), EMPTY_COMPONENT).unwrap();
        std::fs::write(app.join("assets/note.txt"), asset).unwrap();
        let (_, packed) = spot_pack::pack_app(&app, &PackOptions::default())
            .unwrap_or_else(|r| panic!("{}", spot_pack::report::check_text(&r)));
        packed.bytes
    }

    fn signature(&self, bytes: &[u8], cert: &SignedCertificate, key: &KeyPair) -> BundleSignature {
        let packed = spot_pack::PackedBundle {
            bytes: bytes.to_vec(),
            digest: "0".repeat(64).parse().unwrap(),
        };
        let bundle = packed.open(&PackOptions::default()).unwrap();
        let pair = generate_keypair(&key.to_seed_bytes());
        sign_packed(&bundle, NAME, "1.0.0", cert, &pair, SIGNED_AT).unwrap()
    }

    /// Writes bundle, signature and certificate to `incoming/`; returns the bundle path.
    fn place(
        &self,
        bytes: &[u8],
        signature: Option<&BundleSignature>,
        cert: Option<&SignedCertificate>,
    ) -> PathBuf {
        let incoming = self.dir.join("incoming");
        for f in ["app.sig", "app.cert"] {
            let _ = std::fs::remove_file(incoming.join(f));
        }
        let bundle = incoming.join("app.spotapp");
        std::fs::write(&bundle, bytes).unwrap();
        if let Some(s) = signature {
            std::fs::write(incoming.join("app.sig"), s.to_json()).unwrap();
        }
        if let Some(c) = cert {
            std::fs::write(incoming.join("app.cert"), c.to_json()).unwrap();
        }
        bundle
    }

    /// A good signed bundle in place.
    fn good(&self) -> PathBuf {
        let bytes = self.pack("hello");
        let cert = certificate("acme", 100, 1200, &root_key());
        let sig = self.signature(&bytes, &cert, &publisher_key());
        self.place(&bytes, Some(&sig), Some(&cert))
    }

    /// Every file under the root except the audit file, for the "nothing changed" assertions.
    fn snapshot(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let entry = entry.unwrap();
                let path = entry.path();
                if entry.file_type().unwrap().is_dir() {
                    out.insert(path.clone(), Vec::new());
                    walk(&path, out);
                } else if path.file_name().unwrap() != "audit.jsonl" {
                    out.insert(path.clone(), std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.dir, &mut out);
        out
    }

    fn audit_lines(&self) -> Vec<serde_json::Value> {
        std::fs::read_to_string(self.dir.join("sites/s1/audit.jsonl"))
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    async fn install(&self, path: &Path) -> Result<String, spot_cli::Error> {
        ops::install_app_with(
            &self.dir,
            path.to_str().unwrap(),
            Some("s1"),
            Install::default(),
            &Issue::at(NOW),
        )
        .await
    }

    /// The install is refused with `code`; the folder and site are untouched; one audit line.
    async fn refused(&self, path: &Path, code: &str) {
        let before = self.snapshot();
        let err = self.install(path).await.unwrap_err().to_string();
        assert!(err.starts_with(&format!("{code}:")), "{err}");
        assert_eq!(before, self.snapshot(), "a refusal must change nothing");
        assert!(!self.dir.join("apps").join(NAME).join("app.toml").exists());
        let lines = self.audit_lines();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["action"], "install-app");
        assert_eq!(lines[0]["outcome"], "refused");
        assert_eq!(lines[0]["code"], code);
        // Nothing secret: the line is ids, a code and a digest.
        let text = lines[0].to_string();
        for word in ["secret", "private", "seed", "password", "token"] {
            assert!(!text.contains(word), "{text}");
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn loaded(f: &Fixture) -> spot_cli::config::Loaded {
    spot_cli::config::load(&f.dir, false).unwrap()
}

#[test]
fn a_signed_bundle_verifies_and_unpacks_safely() {
    let f = Fixture::new("good", true);
    let path = f.good();
    let installed = bundle_install::install(&f.dir, &loaded(&f), &path, &Issue::at(NOW)).unwrap();
    assert_eq!(installed.app.name, NAME);
    assert!(
        installed.note.contains("verified bundle"),
        "{}",
        installed.note
    );
    assert!(
        installed.note.contains("publisher acme"),
        "{}",
        installed.note
    );
    let target = f.dir.join("apps").join(NAME);
    assert!(target.join("app.toml").is_file());
    assert!(target.join("app.wasm").is_file());
    assert_eq!(
        std::fs::read(target.join("assets/note.txt")).unwrap(),
        b"hello"
    );
    // No temporary directory is left beside it.
    let names: Vec<_> = std::fs::read_dir(f.dir.join("apps"))
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(names, [NAME]);
    // The same verified bundle again lands on the same content: not an error.
    bundle_install::install(&f.dir, &loaded(&f), &path, &Issue::at(NOW)).unwrap();
}

#[tokio::test]
async fn a_good_bundle_passes_the_gate_and_stops_only_at_the_database() {
    let f = Fixture::new("gate", true);
    let path = f.good();
    let err = f.install(&path).await.unwrap_err().to_string();
    // The database is a dead address: the failure is the connection, not a trust refusal.
    assert!(!err.starts_with("cli."), "{err}");
    assert!(f.audit_lines().is_empty());
    assert!(f.dir.join("apps").join(NAME).join("app.toml").is_file());
}

#[tokio::test]
async fn an_altered_payload_is_refused() {
    let f = Fixture::new("payload", true);
    let cert = certificate("acme", 100, 1200, &root_key());
    // The signature of the real bundle, the bytes of another one with a changed asset.
    let sig = f.signature(&f.pack("hello"), &cert, &publisher_key());
    let tampered = f.pack("hellp");
    let path = f.place(&tampered, Some(&sig), Some(&cert));
    f.refused(&path, "cli.bundle_signature_invalid").await;
}

#[tokio::test]
async fn a_flipped_signature_is_refused() {
    let f = Fixture::new("flipped", true);
    let bytes = f.pack("hello");
    let cert = certificate("acme", 100, 1200, &root_key());
    let good = f.signature(&bytes, &cert, &publisher_key());
    let mut raw = *good.signature();
    raw[0] ^= 0x01;
    let bad = BundleSignature::from_parts(
        good.app_id.clone(),
        good.app_version.clone(),
        good.publisher_id.clone(),
        good.key_id.clone(),
        good.signed_at,
        raw,
    );
    let path = f.place(&bytes, Some(&bad), Some(&cert));
    f.refused(&path, "cli.bundle_signature_invalid").await;
}

#[tokio::test]
async fn a_publisher_the_site_does_not_trust_is_refused() {
    let f = Fixture::new("stranger", true);
    let bytes = f.pack("hello");
    let cert = certificate("mallory", 100, 1200, &stranger_root());
    let sig = f.signature(&bytes, &cert, &publisher_key());
    let path = f.place(&bytes, Some(&sig), Some(&cert));
    f.refused(&path, "cli.bundle_untrusted_publisher").await;
}

#[tokio::test]
async fn a_revoked_publisher_key_is_refused() {
    let f = Fixture::new("revoked", true);
    f.pin(&root_key(), &list(Some(&publisher_key())));
    let path = f.good();
    f.refused(&path, "cli.bundle_revoked").await;
}

#[tokio::test]
async fn an_expired_certificate_is_refused() {
    let f = Fixture::new("expired", true);
    let bytes = f.pack("hello");
    let cert = certificate("acme", 100, 400, &root_key());
    let sig = f.signature(&bytes, &cert, &publisher_key());
    let path = f.place(&bytes, Some(&sig), Some(&cert));
    f.refused(&path, "cli.bundle_certificate_expired").await;
}

#[tokio::test]
async fn an_unsigned_bundle_is_refused_when_signing_is_required() {
    let f = Fixture::new("unsigned", true);
    let path = f.place(&f.pack("hello"), None, None);
    f.refused(&path, "cli.bundle_unsigned").await;
}

#[test]
fn an_unsigned_bundle_installs_only_on_an_explicit_development_site() {
    let f = Fixture::new("dev", false);
    let path = f.place(&f.pack("hello"), None, None);
    let installed = bundle_install::install(&f.dir, &loaded(&f), &path, &Issue::at(NOW)).unwrap();
    assert!(installed.note.contains("UNSIGNED"), "{}", installed.note);
    assert!(installed.note.contains("development"), "{}", installed.note);
    // A signature that is present is still verified on a development site.
    let g = Fixture::new("dev2", false);
    let bytes = g.pack("hello");
    let cert = certificate("acme", 100, 1200, &root_key());
    let sig = g.signature(&g.pack("other"), &cert, &publisher_key());
    let path = g.place(&bytes, Some(&sig), Some(&cert));
    let refusal = bundle_install::install(&g.dir, &loaded(&g), &path, &Issue::at(NOW))
        .err()
        .unwrap();
    assert_eq!(refusal.code, "cli.bundle_signature_invalid");
    assert!(!g.dir.join("apps").join(NAME).join("app.toml").exists());
}

#[tokio::test]
async fn a_site_that_pins_no_root_trusts_nobody() {
    let f = Fixture::new("noroot", true);
    let path = f.good();
    std::fs::write(f.dir.join("trust/roots.txt"), "# nothing pinned\n").unwrap();
    f.refused(&path, "cli.bundle_no_trust").await;
}

#[tokio::test]
async fn a_bundle_with_a_parent_directory_entry_is_refused() {
    let f = Fixture::new("escape", true);
    let bytes = stored_zip(&[("../evil.toml", b"x"), ("manifest.toml", b"x")]);
    let cert = certificate("acme", 100, 1200, &root_key());
    let path = f.place(&bytes, None, Some(&cert));
    f.refused(&path, "cli.bundle_unsafe_path").await;
}

#[tokio::test]
async fn a_plain_directory_is_refused_when_signing_is_required() {
    let f = Fixture::new("dir", true);
    let app = f.dir.join("apps").join(NAME);
    std::fs::write(app.join("app.toml"), "[app]\n").unwrap();
    let before = f.snapshot();
    let err = f.install(&app).await.unwrap_err().to_string();
    assert!(err.starts_with("cli.directory_unsigned:"), "{err}");
    let err = f.install(Path::new(NAME)).await.unwrap_err().to_string();
    assert!(err.starts_with("cli.directory_unsigned:"), "{err}");
    assert_eq!(before, f.snapshot());
    assert_eq!(f.audit_lines().len(), 2);
}

#[tokio::test]
async fn a_refused_reset_is_audited_with_its_reason() {
    let f = Fixture::new("reset", true);
    let before = f.snapshot();
    // The operator confirmation is missing.
    let err = spot_cli::users::reset_admin_checked(&f.dir, "s1", &Issue::at(NOW), false)
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("--yes-i-am-the-operator"), "{err}");
    // The site does not exist: nothing to write the record into, so a warning and the refusal.
    assert!(
        spot_cli::users::reset_admin_checked(&f.dir, "nope", &Issue::at(NOW), true)
            .await
            .is_err()
    );
    // The credential store (the database) refuses.
    assert!(
        spot_cli::users::reset_admin_checked(&f.dir, "s1", &Issue::at(NOW), true)
            .await
            .is_err()
    );
    let reasons: Vec<_> = f
        .audit_lines()
        .iter()
        .map(|l| l["reason"].clone())
        .collect();
    assert_eq!(
        reasons,
        ["operator_confirmation_missing", "credential_store_refused"]
    );
    for line in f.audit_lines() {
        assert_eq!(line["action"], "reset-admin");
        assert_eq!(line["outcome"], "refused");
    }
    assert_eq!(
        before,
        f.snapshot(),
        "a refused reset changes no credential"
    );
}

/// A zip with stored entries, hand-written so a test can name an entry the packer never would.
fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFF_u32;
        for &byte in data {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    let mut out = Vec::new();
    let mut central = Vec::new();
    for (name, data) in entries {
        let offset = u32::try_from(out.len()).unwrap();
        let crc = crc32(data);
        let len = u32::try_from(data.len()).unwrap();
        let name_len = u16::try_from(name.len()).unwrap();
        out.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(data);
        central.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02, 20, 0, 20, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&len.to_le_bytes());
        central.extend_from_slice(&name_len.to_le_bytes());
        central.extend_from_slice(&[0; 12]);
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name.as_bytes());
    }
    let start = u32::try_from(out.len()).unwrap();
    let size = u32::try_from(central.len()).unwrap();
    let count = u16::try_from(entries.len()).unwrap();
    out.extend_from_slice(&central);
    out.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06, 0, 0, 0, 0]);
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&size.to_le_bytes());
    out.extend_from_slice(&start.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
    out
}

/// The path past the gate, on a real PostgreSQL site (skipped without one): a verified,
/// extension-only bundle installs, is recorded in the site file, and installing it again is a
/// no-op.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signed_bundle_installs_on_a_real_site() {
    let Some(env) = support::Env::new().await else {
        return;
    };
    env.bare_site();
    let root = env.root.clone();
    let platform = support::repo().join("apps/platform");
    std::fs::create_dir_all(root.join("apps").join(NAME).join("doctypes")).unwrap();
    std::fs::create_dir_all(root.join("trust")).unwrap();
    std::fs::create_dir_all(root.join("incoming")).unwrap();
    std::fs::write(
        root.join("common.toml"),
        format!(
            "apps = [{:?}, 'apps/{NAME}']\n[server]\nbind = ['127.0.0.1:0']\n\
             [postgres]\nadmin_url = {:?}\n\
             [trust]\nroots = 'trust/roots.txt'\nrevocations = 'trust/revocations.json'\n\
             require_signed = true\n",
            platform.display().to_string(),
            env.url
        ),
    )
    .unwrap();
    // The fixture helpers write under `dir`; the site root is that directory. The `Env` removes it.
    let g = Fixture { dir: root.clone() };
    g.pin(&root_key(), &list(None));
    let path = g.good();
    std::mem::forget(g);
    let install = Install {
        apply: true,
        seed: false,
    };
    let out = ops::install_app_with(
        &root,
        path.to_str().unwrap(),
        Some(&env.site),
        install,
        &Issue::at(NOW),
    )
    .await
    .unwrap();
    assert!(out.contains("install mode: verified bundle"), "{out}");
    assert!(env.site_file().contains(NAME), "{}", env.site_file());
    let again = ops::install_app_with(
        &root,
        path.to_str().unwrap(),
        Some(&env.site),
        install,
        &Issue::at(NOW),
    )
    .await
    .unwrap();
    assert!(again.contains("already installed"), "{again}");
}
