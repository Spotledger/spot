#![allow(missing_docs, clippy::unwrap_used)]
use spot_cli::config::load;

#[test]
fn reports_independent_configuration_errors_together() {
    let root = std::env::temp_dir().join(format!("spot_cli_config_{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("common.toml"),
        "apps = []\n[server]\nbind = []\n[postgres]\nadmin_url = 'invalid'\npool_max_size = 0\n",
    )
    .unwrap();
    let error = load(&root, true).err().unwrap().to_string();
    assert!(error.contains("apps"), "{error}");
    assert!(error.contains("bind"), "{error}");
    assert!(error.contains("admin_url"), "{error}");
    assert!(error.contains("pool_max_size"), "{error}");
    std::fs::remove_dir_all(root).unwrap();
}

/// A root with one app directory (so `apps` validates) and one site whose `site.toml` is `site`.
fn root_named(tag: &str, site: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "spot_cli_partition_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("apps/books/doctypes")).unwrap();
    std::fs::create_dir_all(root.join("sites/acme")).unwrap();
    std::fs::write(
        root.join("common.toml"),
        format!(
            "apps = [{:?}]\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\nadmin_url = 'postgres://u:p@127.0.0.1:15432/postgres'\n",
            root.join("apps/books").display().to_string()
        ),
    )
    .unwrap();
    std::fs::write(root.join("sites/acme/site.toml"), site).unwrap();
    root
}

const SITE: &str = "site = 'acme'\ndatabase = 'acme'\n";

#[test]
fn a_partition_profile_is_read_and_a_missing_one_means_a_single_node_site() {
    let plain = root_named("absent", SITE);
    let loaded = load(&plain, false).unwrap();
    assert!(loaded.sites[0].partition.is_none());
    std::fs::remove_dir_all(plain).unwrap();

    let partitioned = root_named(
        "present",
        &format!(
            "{SITE}\n[partition]\nnode = 'node-a'\ndoctypes = ['sales_invoice']\n\n\
             [[partition.scope]]\ndoctype = 'sales_invoice'\nkey = 'company'\nfield = 'company'\n"
        ),
    );
    let loaded = load(&partitioned, false).unwrap();
    let partition = loaded.sites[0].partition.as_ref().unwrap();
    assert_eq!(partition.node, "node-a");
    assert_eq!(partition.doctypes, ["sales_invoice"]);
    assert_eq!(partition.scope[0].field, "company");
    assert!(partition.problems().is_empty());
    std::fs::remove_dir_all(partitioned).unwrap();
}

#[test]
fn a_partition_profile_that_could_not_check_a_write_is_refused_with_every_problem() {
    let root = root_named(
        "invalid",
        &format!(
            "{SITE}\n[partition]\nnode = 'node a'\ndoctypes = ['sales_invoice', 'sales_invoice']\n\n\
             [[partition.scope]]\ndoctype = 'sales_invoice'\nkey = 'company'\nfield = 'company'\n\n\
             [[partition.scope]]\ndoctype = 'sales_invoice'\nkey = 'company'\nfield = 'branch'\n"
        ),
    );
    let error = load(&root, false).err().unwrap().to_string();
    assert!(error.contains("partition.node"), "{error}");
    assert!(error.contains("names `sales_invoice` twice"), "{error}");
    assert!(
        error.contains("names `sales_invoice`'s `company` key twice"),
        "{error}"
    );
    std::fs::remove_dir_all(root).unwrap();

    // A profile with no partitioned DocType would claim the profile and check nothing.
    let empty = root_named("empty", &format!("{SITE}\n[partition]\nnode = 'node-a'\n"));
    let error = load(&empty, false).err().unwrap().to_string();
    assert!(error.contains("partition.doctypes"), "{error}");
    std::fs::remove_dir_all(empty).unwrap();
}

#[test]
fn the_mail_sent_log_follows_the_job_store_unless_told_otherwise() {
    use spot_cli::config::Services;

    let mut services = Services::default();
    assert_eq!(
        services.mail.sent_log, "auto",
        "the default follows the job store"
    );
    assert!(services.validate().is_empty(), "{:?}", services.validate());
    assert!(services.mail.sent_log_in_postgres("postgres"));
    assert!(!services.mail.sent_log_in_postgres("memory"));

    services.mail.sent_log = "memory".into();
    assert!(
        !services.mail.sent_log_in_postgres("postgres"),
        "memory is a choice"
    );
    services.mail.sent_log = "postgres".into();
    assert!(services.mail.sent_log_in_postgres("postgres"));

    // `postgres` without the PostgreSQL job store, and a word nobody knows, are refused.
    services.jobs.store = "memory".into();
    assert!(
        services
            .validate()
            .iter()
            .any(|e| e.contains("sent_log = postgres needs services.jobs.store = postgres"))
    );
    services.mail.sent_log = "somewhere".into();
    assert!(
        services
            .validate()
            .iter()
            .any(|e| e.contains("services.mail.sent_log must be auto, memory or postgres"))
    );
    // `auto` with the memory job store is allowed and uses the memory log.
    services.mail.sent_log = "auto".into();
    assert!(services.validate().is_empty(), "{:?}", services.validate());
}
