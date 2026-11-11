//! Unit tests of the secrets service.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;

fn settings(text: &str) -> Settings {
    toml::from_str(text).unwrap()
}

/// A provider whose unwrap answers something else than what was wrapped.
struct Liar(spot_secrets::MemoryKeyring);

impl KeyProvider for Liar {
    fn current(&self) -> KeyRef {
        self.0.current()
    }
    fn wrap<'a>(
        &'a self,
        data_key: &'a [u8],
        aad: &'a [u8],
    ) -> spot_secrets::ProviderFuture<'a, spot_secrets::WrappedKey> {
        self.0.wrap(data_key, aad)
    }
    fn unwrap<'a>(
        &'a self,
        _key: &'a KeyRef,
        _wrapped: &'a spot_secrets::WrappedKey,
        _aad: &'a [u8],
    ) -> spot_secrets::ProviderFuture<'a, spot_secrets::SecretBytes> {
        Box::pin(async { Ok(spot_secrets::SecretBytes::new(b"not the canary at all, no.".to_vec())) })
    }
    fn index_key<'a>(
        &'a self,
        key: &'a KeyRef,
    ) -> spot_secrets::ProviderFuture<'a, spot_secrets::SecretBytes> {
        self.0.index_key(key)
    }
}

#[tokio::test]
async fn a_canary_mismatch_is_refused_naming_the_file() {
    let good = spot_secrets::MemoryKeyring::new("k").unwrap();
    canary_of(&good, Path::new("/keys/a.json")).await.unwrap();
    let liar = Liar(spot_secrets::MemoryKeyring::new("k").unwrap());
    let error = canary_of(&liar, Path::new("/keys/a.json")).await.unwrap_err();
    assert!(error.0.starts_with(codes::CANARY_FAILED), "{}", error.0);
    assert!(error.0.contains("/keys/a.json"));
    assert!(!error.0.contains("not the canary"));
}

#[test]
fn constant_time_equality() {
    assert!(ct_eq(b"abc", b"abc"));
    assert!(!ct_eq(b"abc", b"abd"));
    assert!(!ct_eq(b"abc", b"ab"));
}

#[test]
fn unknown_keys_are_refused_by_name() {
    let table: toml::Value = toml::from_str("kek_file = '/k'\ntoken = 'x'").unwrap();
    let error = table.try_into::<Settings>().unwrap_err();
    assert!(error.message().starts_with("unknown field"));
}

#[test]
fn a_relative_or_shared_path_is_refused() {
    let root = Path::new("/srv/spot");
    let relative = settings("kek_file = 'keys/{site}.json'");
    assert!(
        relative
            .kek_path(root, "acme", 1)
            .unwrap_err()
            .0
            .starts_with(codes::KEK_PATH_REFUSED)
    );
    let shared = settings("kek_file = '/etc/spot/kek.json'");
    assert!(shared.kek_path(root, "acme", 1).is_ok());
    assert!(
        shared
            .kek_path(root, "acme", 2)
            .unwrap_err()
            .0
            .contains("{site}")
    );
    let per_site = settings("kek_file = '/etc/spot/{site}/kek.json'");
    assert_eq!(
        per_site.kek_path(root, "acme", 3).unwrap(),
        PathBuf::from("/etc/spot/acme/kek.json")
    );
}

#[test]
fn a_key_file_inside_the_site_folders_or_with_dotdot_is_refused() {
    let root = Path::new("/srv/spot");
    let inside = settings("kek_file = '/srv/spot/sites/{site}/kek.json'");
    assert!(
        inside
            .kek_path(root, "acme", 1)
            .unwrap_err()
            .0
            .starts_with(codes::KEK_PATH_REFUSED)
    );
    let dots = settings("kek_file = '/etc/spot/../kek.json'");
    assert!(dots.kek_path(root, "acme", 1).is_err());
}

#[test]
fn no_key_file_and_no_other_provider() {
    let root = Path::new("/srv/spot");
    assert!(
        settings("")
            .kek_path(root, "acme", 1)
            .unwrap_err()
            .0
            .starts_with(codes::KEK_UNSET)
    );
    let kms = settings("provider = 'kms'\nkek_file = '/k'");
    assert!(
        kms.kek_path(root, "acme", 1)
            .unwrap_err()
            .0
            .starts_with(codes::KEK_UNSET)
    );
}
