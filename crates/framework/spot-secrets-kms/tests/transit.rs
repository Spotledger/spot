//! The Transit client: configuration checks, the token file, and the egress guard.
#![allow(missing_docs, clippy::expect_used, clippy::err_expect)]

mod support;

use spot_egress::{EgressPolicy, Guard, InMemorySink, Sender, TestResolver};
use spot_secrets_kms::{GuardGate, KmsClient, TransitClient, TransitConfig};
use std::sync::Arc;
use support::*;

async fn open_client(config: TransitConfig) -> Result<TransitClient, spot_secrets_kms::KmsError> {
    let fake = FakeTransit::new();
    TransitClient::open(config, fake as Arc<dyn Sender>, Arc::new(OpenGate)).await
}

fn good(dir: &std::path::Path) -> TransitConfig {
    transit_config(write_token(dir, TOKEN))
}

#[tokio::test]
async fn a_good_configuration_opens_and_reads_the_key() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let client = client(&fake, dir.path()).await;
    let info = client.key_info().await.expect("key info");
    assert_eq!(info.latest_version, 1);
    assert_eq!(info.min_decryption_version, 1);
    assert!(info.enabled);
}

#[tokio::test]
async fn the_address_must_be_a_bare_https_url() {
    let dir = tempfile::tempdir().expect("dir");
    for bad in [
        "http://kms.test:8200",
        "ftp://kms.test",
        "kms.test",
        "https://user:pw@kms.test",
        "https://kms.test/?x=1",
        "https://kms.test/#frag",
        "",
    ] {
        let mut config = good(dir.path());
        config.address = bad.into();
        let err = open_client(config).await.err().expect("refused");
        assert_eq!(err.code(), "secrets_kms.config_invalid", "{bad}");
    }
}

#[tokio::test]
async fn mount_key_namespace_and_timeout_are_checked() {
    let dir = tempfile::tempdir().expect("dir");
    type Edit = Box<dyn Fn(&mut TransitConfig)>;
    let cases: Vec<Edit> = vec![
        Box::new(|c: &mut TransitConfig| c.mount = String::new()),
        Box::new(|c: &mut TransitConfig| c.mount = "../secret".into()),
        Box::new(|c: &mut TransitConfig| c.mount = "a/b/c/d/e".into()),
        Box::new(|c: &mut TransitConfig| c.mount = "trans it".into()),
        Box::new(|c: &mut TransitConfig| c.key_name = "a/b".into()),
        Box::new(|c: &mut TransitConfig| c.key_name = String::new()),
        Box::new(|c: &mut TransitConfig| c.key_name = "x".repeat(65)),
        Box::new(|c: &mut TransitConfig| c.namespace = Some(String::new())),
        Box::new(|c: &mut TransitConfig| c.namespace = Some("bad ns\r\n".into())),
        Box::new(|c: &mut TransitConfig| c.request_timeout = std::time::Duration::ZERO),
        Box::new(|c: &mut TransitConfig| c.request_timeout = std::time::Duration::from_secs(3600)),
    ];
    for (i, tweak) in cases.iter().enumerate() {
        let mut config = good(dir.path());
        tweak(&mut config);
        let err = open_client(config).await.err().expect("refused");
        assert_eq!(err.code(), "secrets_kms.config_invalid", "case {i}");
    }
    let mut config = good(dir.path());
    config.mount = "team/transit".into();
    config.namespace = Some("admin/team-a".into());
    open_client(config)
        .await
        .expect("nested mount and namespace are fine");
}

#[tokio::test]
async fn a_missing_empty_or_odd_token_file_is_refused_without_naming_the_token() {
    let dir = tempfile::tempdir().expect("dir");
    let mut config = good(dir.path());
    config.token_file = dir.path().join("absent.token");
    let err = open_client(config).await.err().expect("missing");
    assert_eq!(err.code(), "secrets_kms.token_unreadable");

    for content in ["", "   \n", "has space inside", "tab\there"] {
        let path = write_token(dir.path(), content);
        let err = open_client(transit_config(path))
            .await
            .err()
            .expect("odd token");
        assert_eq!(err.code(), "secrets_kms.token_unreadable", "{content:?}");
        assert!(!err.to_string().contains(content.trim()) || content.trim().is_empty());
    }
    // A directory is not a token file.
    let mut config = good(dir.path());
    config.token_file = dir.path().to_path_buf();
    assert_eq!(
        open_client(config).await.err().expect("dir").code(),
        "secrets_kms.token_unreadable"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_world_or_group_readable_token_file_refuses_to_start() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("dir");
    for mode in [0o644, 0o640, 0o604, 0o666] {
        let path = write_token(dir.path(), TOKEN);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        let err = open_client(transit_config(path))
            .await
            .err()
            .expect("too open");
        assert_eq!(err.code(), "secrets_kms.token_unreadable", "{mode:o}");
        assert!(err.to_string().contains("mode"), "{err}");
        assert!(!err.to_string().contains(TOKEN));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_token_file_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let real = write_token(dir.path(), TOKEN);
    let link = dir.path().join("link.token");
    std::os::unix::fs::symlink(&real, &link).expect("link");
    let err = open_client(transit_config(link)).await.err().expect("link");
    assert_eq!(err.code(), "secrets_kms.token_unreadable");
}

#[tokio::test]
async fn a_rotated_token_is_picked_up_by_reload() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let client = client(&fake, dir.path()).await;
    fake.revoke_token();
    assert_eq!(
        client.key_info().await.err().expect("revoked").code(),
        "secrets_kms.rejected"
    );
    // The operator rotates the token in the KMS and in the file; the fake's accepted token
    // is the one the file now holds.
    fake.set_token("fresh-token-0002");
    write_token(dir.path(), "fresh-token-0002");
    client.reload_token().await.expect("reload");
    client.key_info().await.expect("works again");
    // A failed reload keeps the token in use.
    std::fs::remove_file(dir.path().join("kms.token")).expect("remove");
    assert!(client.reload_token().await.is_err());
    client.key_info().await.expect("old token still in use");
}

fn guard(resolved: &str) -> Arc<Guard> {
    let policy = EgressPolicy::from_toml(
        "[tenants.acme]\n[[tenants.acme.allow]]\nscheme = \"https\"\nhost = \"kms.test\"\nports = [8200]\n",
    )
    .expect("policy");
    let resolver = Arc::new(TestResolver::new());
    resolver.add_response("kms.test", vec![vec![resolved.parse().expect("ip"); 1]; 8]);
    Arc::new(Guard::new(
        policy,
        resolver,
        Arc::new(InMemorySink::new()),
        clock(),
    ))
}

#[tokio::test]
async fn a_private_address_is_refused_by_the_default_guard() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    for private in ["10.0.0.5", "127.0.0.1", "169.254.169.254", "192.168.1.9"] {
        let client = TransitClient::open(
            good(dir.path()),
            Arc::clone(&fake) as Arc<dyn Sender>,
            Arc::new(GuardGate::new(guard(private), "acme")),
        )
        .await
        .expect("open makes no network call");
        let err = client.key_info().await.err().expect("refused");
        assert_eq!(err.code(), "secrets_kms.egress_refused", "{private}");
    }
    assert_eq!(
        fake.state.lock().expect("state").infos,
        0,
        "nothing was sent"
    );
}

#[tokio::test]
async fn a_public_allowed_host_passes_the_guard() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let client = TransitClient::open(
        good(dir.path()),
        Arc::clone(&fake) as Arc<dyn Sender>,
        Arc::new(GuardGate::new(guard("93.184.216.34"), "acme")),
    )
    .await
    .expect("open");
    client.key_info().await.expect("allowed");
}

#[tokio::test]
async fn a_tenant_without_an_allow_entry_is_refused() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let client = TransitClient::open(
        good(dir.path()),
        Arc::clone(&fake) as Arc<dyn Sender>,
        Arc::new(GuardGate::new(guard("93.184.216.34"), "other-tenant")),
    )
    .await
    .expect("open");
    let err = client.key_info().await.err().expect("refused");
    assert_eq!(err.code(), "secrets_kms.egress_refused");
}

#[tokio::test]
async fn error_answers_never_echo_the_body_or_the_token() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let client = client(&fake, dir.path()).await;
    fake.revoke_token();
    let err = client.key_info().await.err().expect("denied");
    let text = format!("{err} {err:?}");
    assert!(!text.contains("permission denied"), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
    assert!(!format!("{client:?}").contains(TOKEN));
}

#[tokio::test]
async fn encrypt_and_decrypt_check_their_inputs() {
    let dir = tempfile::tempdir().expect("dir");
    let fake = FakeTransit::new();
    let client = client(&fake, dir.path()).await;
    assert_eq!(
        client.encrypt(AAD, b"").await.err().expect("empty").code(),
        "secrets_kms.input_invalid"
    );
    assert_eq!(
        client
            .encrypt(AAD, &vec![1u8; 5000])
            .await
            .err()
            .expect("long")
            .code(),
        "secrets_kms.input_invalid"
    );
    let ct = client.encrypt(AAD, b"value").await.expect("encrypt");
    assert_eq!(ct.key_version, 1);
    let plain = client.decrypt(AAD, &ct).await.expect("decrypt");
    assert_eq!(plain.expose(), b"value");
    // A ciphertext whose version prefix and stated version disagree never leaves the process.
    let mut wrong = ct.clone();
    wrong.key_version = 2;
    assert_eq!(
        client
            .decrypt(AAD, &wrong)
            .await
            .err()
            .expect("mismatch")
            .code(),
        "secrets_kms.input_invalid"
    );
    let junk = spot_secrets_kms::KmsCiphertext {
        key_version: 1,
        bytes: b"not a transit ciphertext".to_vec(),
    };
    assert_eq!(
        client.decrypt(AAD, &junk).await.err().expect("junk").code(),
        "secrets_kms.input_invalid"
    );
    assert!(!format!("{ct:?}").contains("vault:"));
}
