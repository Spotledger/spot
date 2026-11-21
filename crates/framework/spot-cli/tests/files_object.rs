//! The object-store file providers, hosted (WP-PLT-10.T8): settings, refusals, the feature gate,
//! the start-up probe and, when an emulator is configured, a round trip.
//!
//! Nothing here needs the network except the two emulator cases, which read `SPOT_TEST_S3_URL`
//! (MinIO) or `SPOT_TEST_AZURE_URL` (Azurite) and print `SKIPPED` with the reason when unset.
#![allow(clippy::unwrap_used, clippy::expect_used, missing_docs)]

use std::path::PathBuf;
use std::time::Duration;

use spot_cli::config::Files;
use spot_cli::services::files_object::{CredentialRef, PROBE_TIMEOUT, open, probe, validate};
use spot_core::SiteId;
use spot_files::error::FileError;
use spot_files::key::{FileKey, FileKeyPrefix};
use spot_files::memory::MemoryFileStore;
use spot_files::store::{
    Body, FileFuture, FileMeta, FileResult, FileStore, PutOptions, StoredFile,
};

fn files(text: &str) -> Files {
    toml::from_str(text).unwrap()
}

fn refusals(text: &str) -> Vec<String> {
    validate(&files(text))
}

fn site() -> SiteId {
    SiteId::new("acme").unwrap()
}

const S3_OK: &str = r#"
provider = "s3"
[s3]
bucket = "spot-files"
"#;

#[test]
fn the_local_providers_keys_and_defaults_are_unchanged() {
    let d = Files::default();
    assert_eq!(d.provider, "local");
    assert_eq!(d.root, "files");
    assert_eq!(d.max_bytes, 25 * 1024 * 1024);
    assert!(d.s3.is_none() && d.azure.is_none());
    assert!(validate(&d).is_empty());
    let parsed = files("provider = \"memory\"");
    assert!(validate(&parsed).is_empty());
}

#[test]
fn every_refusal_of_the_object_provider_settings_names_its_key() {
    assert!(refusals(S3_OK).is_empty(), "{:?}", refusals(S3_OK));

    // A sub-table for a provider that is not selected.
    let wrong = refusals("provider = \"local\"\n[s3]\nbucket = \"b\"");
    assert!(
        wrong.iter().any(|e| e.contains("services.files.s3")),
        "{wrong:?}"
    );
    let wrong = refusals("provider = \"s3\"\n[s3]\nbucket = \"b\"\n[azure]\naccount = \"a\"");
    assert!(
        wrong.iter().any(|e| e.contains("services.files.azure")),
        "{wrong:?}"
    );
    // The selected provider without its table.
    assert!(
        refusals("provider = \"s3\"")
            .iter()
            .any(|e| e.contains("[services.files.s3]"))
    );
    assert!(
        refusals("provider = \"azure\"")
            .iter()
            .any(|e| e.contains("[services.files.azure]"))
    );

    // A missing bucket.
    let e = refusals("provider = \"s3\"\n[s3]\nregion = \"us-east-1\"");
    assert!(
        e.iter().any(|e| e.contains("services.files.s3.bucket")),
        "{e:?}"
    );

    // Plain HTTP to a remote endpoint, with and without the explicit flag.
    let remote = "provider = \"s3\"\n[s3]\nbucket = \"b\"\nendpoint = \"http://s3.example.com\"\nallow_http = true";
    let e = refusals(remote);
    assert!(
        e.iter().any(|e| e.contains("allow_insecure_remote")),
        "{e:?}"
    );
    assert!(refusals(&format!("{remote}\nallow_insecure_remote = true")).is_empty());
    // Loopback is fine.
    assert!(
        refusals("provider = \"s3\"\n[s3]\nbucket = \"b\"\nendpoint = \"http://127.0.0.1:9000\"\nallow_http = true")
            .is_empty()
    );
    // Plain HTTP without allow_http.
    let e =
        refusals("provider = \"s3\"\n[s3]\nbucket = \"b\"\nendpoint = \"http://127.0.0.1:9000\"");
    assert!(e.iter().any(|e| e.contains("allow_http")), "{e:?}");

    // A reference that is neither `env:` nor an absolute `file:`.
    let e = refusals(
        "provider = \"s3\"\n[s3]\nbucket = \"b\"\naccess_key_id_ref = \"AKIAPLANTED\"\nsecret_access_key_ref = \"env:SECRET\"",
    );
    assert!(e.iter().any(|e| e.contains("access_key_id_ref")), "{e:?}");
    assert!(
        !e.iter().any(|e| e.contains("AKIAPLANTED")),
        "the text is never quoted"
    );
    let e = refusals(
        "provider = \"s3\"\n[s3]\nbucket = \"b\"\naccess_key_id_ref = \"file:relative\"\nsecret_access_key_ref = \"env:SECRET\"",
    );
    assert!(e.iter().any(|e| e.contains("absolute path")), "{e:?}");
    // Both references or neither.
    let e =
        refusals("provider = \"s3\"\n[s3]\nbucket = \"b\"\nsecret_access_key_ref = \"env:SECRET\"");
    assert!(
        e.iter().any(|e| e.contains("both access_key_id_ref")),
        "{e:?}"
    );

    // Azure: kind, credential, container.
    let az = "provider = \"azure\"\n[azure]\naccount = \"acct\"\ncontainer = \"c\"\ncredential_kind = \"sas\"\ncredential_ref = \"env:AZ\"";
    assert!(refusals(az).is_empty(), "{:?}", refusals(az));
    let e = refusals(
        "provider = \"azure\"\n[azure]\naccount = \"acct\"\ncontainer = \"c\"\ncredential_kind = \"password\"",
    );
    assert!(e.iter().any(|e| e.contains("credential_kind")), "{e:?}");
    assert!(
        e.iter().any(|e| e.contains("credential_ref is required")),
        "{e:?}"
    );
    let e = refusals(
        "provider = \"azure\"\n[azure]\ncredential_kind = \"sas\"\ncredential_ref = \"env:AZ\"",
    );
    assert!(e.iter().any(|e| e.contains("account")) && e.iter().any(|e| e.contains("container")));

    // Unknown keys in a sub-table are refused by the parser.
    assert!(
        toml::from_str::<Files>("provider = \"s3\"\n[s3]\nbucket = \"b\"\nsecret = \"x\"").is_err()
    );
}

#[test]
fn an_unknown_provider_is_an_error_not_local() {
    let f = Files {
        provider: "gcs".into(),
        ..Files::default()
    };
    let e = open(&f, PathBuf::from("/nonexistent"))
        .err()
        .unwrap()
        .to_string();
    assert!(
        e.contains("services.files.provider") && e.contains("gcs"),
        "{e}"
    );
}

#[cfg(unix)]
struct ScratchDir(PathBuf);

#[cfg(unix)]
impl ScratchDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("spot-files-object-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

#[cfg(unix)]
impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
#[test]
fn credential_references_resolve_and_refuse_without_leaking_the_value() {
    use std::os::unix::fs::PermissionsExt;
    let dir = ScratchDir::new();
    let setting = "services.files.s3.secret_access_key_ref";
    let planted = "PLANTED-SECRET-VALUE-123";

    let file = dir.0.join("secret");
    std::fs::write(&file, format!("{planted}\n")).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let reference = CredentialRef::parse(setting, &format!("file:{}", file.display())).unwrap();
    let e = reference.resolve(setting).unwrap_err().to_string();
    assert!(
        e.contains("readable by its group or others") && e.contains(setting),
        "{e}"
    );
    assert!(!e.contains(planted), "{e}");

    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(reference.resolve(setting).unwrap(), planted);
    assert!(!format!("{reference:?}").contains(planted));

    let empty = dir.0.join("empty");
    std::fs::write(&empty, "\n").unwrap();
    std::fs::set_permissions(&empty, std::fs::Permissions::from_mode(0o600)).unwrap();
    let e = CredentialRef::parse(setting, &format!("file:{}", empty.display()))
        .unwrap()
        .resolve(setting)
        .unwrap_err()
        .to_string();
    assert!(e.contains("is empty"), "{e}");

    let missing = CredentialRef::parse(setting, "file:/nonexistent/spot-secret").unwrap();
    assert!(
        missing
            .resolve(setting)
            .unwrap_err()
            .to_string()
            .contains("cannot be read")
    );
    let unset = CredentialRef::parse(setting, "env:SPOT_TEST_FILES_OBJECT_UNSET_VARIABLE").unwrap();
    assert!(
        unset
            .resolve(setting)
            .unwrap_err()
            .to_string()
            .contains("not set or is empty")
    );
}

#[cfg(not(feature = "files-s3"))]
#[test]
fn without_the_feature_an_s3_site_is_refused_at_start_up() {
    let e = open(&files(S3_OK), PathBuf::from("/nonexistent"))
        .err()
        .unwrap()
        .to_string();
    assert!(
        e.contains("services.files.provider = \"s3\"")
            && e.contains("without the files-s3 feature"),
        "{e}"
    );
}

#[cfg(not(feature = "files-azure"))]
#[test]
fn without_the_feature_an_azure_site_is_refused_at_start_up() {
    let text = "provider = \"azure\"\n[azure]\naccount = \"acct\"\ncontainer = \"c\"\ncredential_kind = \"sas\"\ncredential_ref = \"env:AZ\"";
    let e = open(&files(text), PathBuf::from("/nonexistent"))
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("without the files-azure feature"), "{e}");
}

/// How the fake store misbehaves.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    Put,
    Get,
    Delete,
    Hang,
}

struct Faulty {
    inner: MemoryFileStore,
    fault: Fault,
}

impl FileStore for Faulty {
    fn put<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
        body: Body,
        opts: PutOptions,
    ) -> FileFuture<'a, FileResult<StoredFile>> {
        Box::pin(async move {
            match self.fault {
                Fault::Put => Err(FileError::io("planted put failure")),
                Fault::Hang => std::future::pending().await,
                _ => self.inner.put(site, key, body, opts).await,
            }
        })
    }
    fn get<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
    ) -> FileFuture<'a, FileResult<(FileMeta, Body)>> {
        Box::pin(async move {
            if self.fault == Fault::Get {
                return Err(FileError::io("planted get failure"));
            }
            self.inner.get(site, key).await
        })
    }
    fn head<'a>(
        &'a self,
        site: &'a SiteId,
        key: FileKey,
    ) -> FileFuture<'a, FileResult<Option<FileMeta>>> {
        self.inner.head(site, key)
    }
    fn delete<'a>(&'a self, site: &'a SiteId, key: FileKey) -> FileFuture<'a, FileResult<bool>> {
        Box::pin(async move {
            if self.fault == Fault::Delete {
                return Ok(false);
            }
            self.inner.delete(site, key).await
        })
    }
    fn list<'a>(
        &'a self,
        site: &'a SiteId,
        prefix: &'a FileKeyPrefix,
        after: Option<&'a FileKey>,
        limit: usize,
    ) -> FileFuture<'a, FileResult<Vec<FileMeta>>> {
        self.inner.list(site, prefix, after, limit)
    }
}

fn faulty(fault: Fault) -> Faulty {
    Faulty {
        inner: MemoryFileStore::new(),
        fault,
    }
}

#[tokio::test]
async fn the_probe_passes_on_a_healthy_store_and_leaves_nothing_behind() {
    let store = faulty(Fault::None);
    probe(&store, &site(), PROBE_TIMEOUT).await.unwrap();
    let all = store
        .list(&site(), &FileKeyPrefix::new("").unwrap(), None, 10)
        .await
        .unwrap();
    assert!(all.is_empty(), "the probe object was deleted");
}

#[tokio::test]
async fn each_probe_failure_has_its_own_message() {
    for (fault, expect) in [
        (Fault::Put, "cannot write"),
        (Fault::Get, "cannot read back"),
        (Fault::Delete, "cannot delete"),
    ] {
        let e = probe(&faulty(fault), &site(), PROBE_TIMEOUT)
            .await
            .unwrap_err();
        assert!(e.contains(expect), "{e}");
        assert!(
            !e.contains("planted"),
            "the backend's text stays out of the message: {e}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_probe_that_hangs_fails_with_timeout_after_ten_seconds() {
    let started = tokio::time::Instant::now();
    let e = probe(&faulty(Fault::Hang), &site(), PROBE_TIMEOUT)
        .await
        .unwrap_err();
    assert!(e.contains("timeout"), "{e}");
    assert_eq!(started.elapsed(), Duration::from_secs(10));
}

#[test]
fn the_probe_timeout_is_ten_seconds() {
    assert_eq!(PROBE_TIMEOUT, Duration::from_secs(10));
}

/// The provider contract through the host's own build and probe: one helper for both emulators.
#[cfg(any(feature = "files-s3", feature = "files-azure"))]
async fn provider_roundtrip(files: &Files) {
    use spot_files::key::Visibility;
    use spot_files::store::body_from_vec;
    use tokio::io::AsyncReadExt;
    let opened = open(files, PathBuf::from("/nonexistent")).unwrap();
    let info = opened.object.expect("an object provider");
    assert!(!info.describe().is_empty());
    probe(opened.store.as_ref(), &site(), PROBE_TIMEOUT)
        .await
        .unwrap();
    let key = FileKey::new(Visibility::Private, "round/trip.txt").unwrap();
    let data = b"0123456789".to_vec();
    opened
        .store
        .put(
            &site(),
            key.clone(),
            body_from_vec(data.clone()),
            PutOptions::new("text/plain", 1024, true),
        )
        .await
        .unwrap();
    let (meta, mut body) = opened.store.get(&site(), key.clone()).await.unwrap();
    let mut bytes = Vec::new();
    body.read_to_end(&mut bytes).await.unwrap();
    assert_eq!(bytes, data);
    assert_eq!(meta.size, 10);
    assert!(opened.store.delete(&site(), key.clone()).await.unwrap());
    assert!(opened.store.head(&site(), key).await.unwrap().is_none());
}

#[cfg(feature = "files-s3")]
#[tokio::test]
async fn an_s3_emulator_round_trips_through_the_hosts_build() {
    let Ok(url) = std::env::var("SPOT_TEST_S3_URL") else {
        eprintln!("SKIPPED: SPOT_TEST_S3_URL is not set (needs MinIO or another S3 emulator)");
        return;
    };
    let bucket = std::env::var("SPOT_TEST_S3_BUCKET").unwrap_or_else(|_| "spot-test".into());
    let text = format!(
        "provider = \"s3\"\n[s3]\nbucket = \"{bucket}\"\nendpoint = \"{url}\"\nallow_http = true\npath_style = true\nregion = \"us-east-1\"\naccess_key_id_ref = \"env:SPOT_TEST_S3_KEY\"\nsecret_access_key_ref = \"env:SPOT_TEST_S3_SECRET\""
    );
    provider_roundtrip(&files(&text)).await;
}

#[cfg(feature = "files-azure")]
#[tokio::test]
async fn an_azurite_emulator_round_trips_through_the_hosts_build() {
    let Ok(url) = std::env::var("SPOT_TEST_AZURE_URL") else {
        eprintln!("SKIPPED: SPOT_TEST_AZURE_URL is not set (needs Azurite)");
        return;
    };
    let container =
        std::env::var("SPOT_TEST_AZURE_CONTAINER").unwrap_or_else(|_| "spot-test".into());
    let text = format!(
        "provider = \"azure\"\n[azure]\naccount = \"devstoreaccount1\"\ncontainer = \"{container}\"\nendpoint = \"{url}\"\nallow_http = true\ncredential_kind = \"access_key\"\ncredential_ref = \"env:SPOT_TEST_AZURE_KEY\""
    );
    provider_roundtrip(&files(&text)).await;
}
