#![allow(missing_docs, clippy::unwrap_used)]
use spot_cli::config::{load, site_name};
use spot_storage_contract::RecordId;
struct Root(std::path::PathBuf);
impl Root {
    fn new(common: &str) -> Self {
        let root =
            Self(std::env::temp_dir().join(format!("spot_cli_config_{}", RecordId::new_v7())));
        std::fs::create_dir_all(root.0.join("app/doctypes")).unwrap();
        std::fs::write(root.0.join("common.toml"), common).unwrap();
        root
    }
    fn error(&self, dev: bool) -> String {
        load(&self.0, dev).err().unwrap().to_string()
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
const GOOD: &str = "apps = ['app']\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\nadmin_url = 'postgres://spot@localhost/postgres'\n";
#[test]
fn aggregates_unknown_common_fields_instead_of_stopping_at_first_error() {
    let root = Root::new(&format!(
        "typo = true\n{}\n[telemetry]\nother_typo = true\n",
        GOOD.replace("[postgres]", "[postgres]\npg_typo = true")
    ));
    let error = root.error(false);
    for expected in ["typo", "other_typo", "pg_typo"] {
        assert!(error.contains(expected), "{error}");
    }
}
#[test]
fn refuses_non_loopback_dev_but_allows_production_bind() {
    let root = Root::new(&GOOD.replace("127.0.0.1:0", "0.0.0.0:8000"));
    assert!(root.error(true).contains("loopback"));
    assert!(load(&root.0, false).is_ok());
}
#[test]
fn strict_unknown_fields_at_every_configuration_level() {
    for text in [
        format!("typo = true\n{GOOD}"),
        GOOD.replace("[server]", "[server]\ntypo = true"),
        GOOD.replace("[postgres]", "[postgres]\ntypo = true"),
        format!("{GOOD}\n[telemetry]\ntypo = true"),
    ] {
        let root = Root::new(&text);
        assert!(load(&root.0, false).is_err());
    }
    for suffix in [
        "\ntypo = true\n",
        "\n[[api_keys]]\nkey_id = 'abcdefg23456'\ndigest = 'bad'\nuser = 'bad'\ntypo = true\n",
    ] {
        let root = Root::new(GOOD);
        std::fs::create_dir_all(root.0.join("sites/demo")).unwrap();
        std::fs::write(
            root.0.join("sites/demo/site.toml"),
            format!("site = 'demo'\ndatabase = 'demo'\n{suffix}"),
        )
        .unwrap();
        assert!(load(&root.0, false).is_err());
    }
}
#[test]
fn reports_bad_sites_and_digest_without_panicking_or_echoing_secrets() {
    let root = Root::new(GOOD);
    for name in ["one", "two"] {
        std::fs::create_dir_all(root.0.join("sites").join(name)).unwrap();
        std::fs::write(root.0.join("sites").join(name).join("site.toml"), format!("site = '{name}'\ndatabase = 'hostile;'\ncollation = \"en'\"\n[[api_keys]]\nkey_id = 'abcdefg23456'\ndigest = '{}'\nuser = 'not-a-user'\n", "é".repeat(32))).unwrap();
    }
    let error = root.error(false);
    for expected in [
        "one/site.toml",
        "two/site.toml",
        "database",
        "collation",
        "API key",
    ] {
        assert!(error.contains(expected), "{error}");
    }
    assert!(!error.contains("é"));
}
#[test]
fn accepts_safe_site_names_and_rejects_path_traversal() {
    for name in ["demo", "a-b", "123"] {
        assert!(site_name(name));
    }
    for name in ["", "../demo", "a/b", "a\\b", "UPPER", "-a", "a-", ".", "é"] {
        assert!(!site_name(name));
    }
}
#[test]
fn metrics_without_a_credential_is_refused_on_a_non_loopback_bind() {
    let open = "\n[services.metrics]\nrequire_auth = false\n";
    // Reachable from elsewhere and open: the host does not start, and says why.
    let root = Root::new(&format!(
        "{}{open}",
        GOOD.replace("127.0.0.1:0", "0.0.0.0:8000")
    ));
    let error = root.error(false);
    assert!(error.contains("services.metrics.require_auth"), "{error}");
    assert!(error.contains("loopback"), "{error}");
    // One non-loopback address among loopback ones is enough to refuse.
    let mixed = Root::new(&format!(
        "{}{open}",
        GOOD.replace("['127.0.0.1:0']", "['127.0.0.1:0', '10.0.0.5:8000']")
    ));
    assert!(mixed.error(false).contains("services.metrics.require_auth"));
    // Loopback only: a co-located collector may scrape without a credential.
    let loopback = Root::new(&format!("{GOOD}{open}"));
    assert!(load(&loopback.0, false).is_ok());
    // The default (credential required) is fine on any bind, and so is switching metrics off.
    let default = Root::new(&GOOD.replace("127.0.0.1:0", "0.0.0.0:8000"));
    assert!(load(&default.0, false).is_ok());
    let disabled = Root::new(&format!(
        "{}\n[services.metrics]\nenabled = false\nrequire_auth = false\n",
        GOOD.replace("127.0.0.1:0", "0.0.0.0:8000")
    ));
    assert!(load(&disabled.0, false).is_ok());
}
