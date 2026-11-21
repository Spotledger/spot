//! WP-SEC-03.T15: the secret canary sweep. One site, one canary in every place the platform
//! stores a secret today, then every artefact the site can produce is searched for every
//! canary in the plain form and in the encodings a leak takes (see `canary.rs`): backups, the
//! support bundle, exports, the event stream, the journals, `/metrics`, the logs of the run at
//! trace level, the bodies of failed requests, the files of the data directory and a
//! `pg_dump` of the database. A canary may appear only in its sealed (envelope) form, in the
//! database.
//!
//! Real PostgreSQL (`SPOT_TEST_PG_URL`, else the test prints one `SKIPPED` line and passes);
//! the backup and `pg_dump` artefacts also need the client tools (`SPOT_TEST_PG_BIN`, else
//! `pg_dump` on `PATH`) and are skipped with a printed reason without them. Nothing is faked.
//!
//! A failure names the canary's label, the artefact and the form. It never prints a value, and
//! no canary is written to a file of the repository.
#![cfg(unix)]
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

#[path = "secret_canary_sweep/artefacts.rs"]
mod artefacts;
#[path = "secret_canary_sweep/canary.rs"]
mod canary;
#[path = "secret_canary_sweep/capture.rs"]
mod capture;
#[path = "secret_canary_sweep/rig.rs"]
mod rig;

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::Path;

use serde_json::json;
use spot_document::secrets::SecretCaller;

use artefacts::Artefacts;
use canary::Canary;
use rig::Rig;

/// A place a secret can live whose write path is not hosted yet (none today: scim_credential,
/// sso_client_secret, saml_provider_key and connector_credential are planted below, through the
/// services that host them): the label of its canary, the task that will host it, and what shows it is hosted (a file or directory stem under
/// `src/services`, `src/commands` or `src`, and a crate name in `Cargo.toml`). The sweep skips
/// it, never fakes it; `planted_elsewhere_lists_only_paths_that_do_not_exist_yet` fails the day
/// the hosting appears, so the entry is removed and the canary planted.
struct Elsewhere {
    label: &'static str,
    owner: &'static str,
    stems: &'static [&'static str],
    crates: &'static [&'static str],
}

const PLANTED_ELSEWHERE: &[Elsewhere] = &[];

/// Whether `name` (a file or directory under `src`) is the hosting module of `stem`:
/// `stem.rs`, `stem/`, `stem_<more>.rs`, or the plural of the stem.
fn is_hosting_module(name: &str, stem: &str) -> bool {
    let base = name.strip_suffix(".rs").unwrap_or(name);
    base == stem || base == format!("{stem}s") || base.starts_with(&format!("{stem}_"))
}

fn depends_on(cargo: &str, krate: &str) -> bool {
    cargo.lines().any(|line| {
        let line = line.trim_start();
        line.starts_with(&format!("{krate} ")) || line.starts_with(&format!("{krate}="))
    })
}

/// Everything in `PLANTED_ELSEWHERE` is still not hosted. When a hosting module appears the
/// entry must go and the sweep must plant that canary through its real write path.
#[test]
fn planted_elsewhere_lists_only_paths_that_do_not_exist_yet() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let cargo = std::fs::read_to_string(manifest.join("Cargo.toml")).unwrap();
    let mut names = Vec::new();
    for dir in ["src", "src/services", "src/commands"] {
        for entry in std::fs::read_dir(manifest.join(dir)).unwrap().flatten() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    let mut now_hosted = Vec::new();
    let mut labels = BTreeSet::new();
    for entry in PLANTED_ELSEWHERE {
        assert!(
            labels.insert(entry.label),
            "`{}` is listed twice in PLANTED_ELSEWHERE",
            entry.label
        );
        let shape_ok = entry.owner.starts_with("WP-")
            && entry.owner.contains(".T")
            && !entry.stems.is_empty()
            && !entry.crates.is_empty();
        assert!(
            shape_ok,
            "`{}` needs an owning task id and what shows it is hosted",
            entry.label
        );
        let hosted_by_file = entry
            .stems
            .iter()
            .any(|stem| names.iter().any(|name| is_hosting_module(name, stem)));
        let hosted_by_crate = entry.crates.iter().any(|krate| depends_on(&cargo, krate));
        if hosted_by_file || hosted_by_crate {
            now_hosted.push(format!("{} ({})", entry.label, entry.owner));
        }
    }
    assert!(
        now_hosted.is_empty(),
        "these are listed in PLANTED_ELSEWHERE but their hosting exists now; plant them in the \
         sweep and remove the entries: {}",
        now_hosted.join(", ")
    );
}

/// The scanner finds a canary in every encoding it claims (the negative control of the harness
/// lives in `canary.rs`; this one proves the capture reaches it end to end).
#[test]
fn the_log_capture_sees_a_logged_canary() {
    let log = capture::install();
    let control = Canary::new("harness_control");
    tracing::info!(target: "secret_canary_sweep", control = %control.value(), "control line");
    let hits = canary::scan(
        "logs:in_process",
        &log.snapshot(),
        std::slice::from_ref(&control),
    );
    assert!(
        !hits.is_empty(),
        "a canary written to the log is not in the capture: the sweep could not see a leak"
    );
}

async fn call(
    addr: SocketAddr,
    host: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: Option<serde_json::Value>,
) -> common::Reply {
    common::http(addr, host, token, method, path, &[], body).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_planted_secret_reaches_any_artefact() {
    if std::env::var_os("SPOT_TEST_PG_URL").is_none() {
        println!(
            "SKIPPED: SPOT_TEST_PG_URL is not set (the sweep runs a whole site on a real PostgreSQL)"
        );
        return;
    }
    let log = capture::install();
    // The values the operator supplies for the sign-in providers and the connector vault.
    let sso_client_secret = Canary::new("sso_client_secret");
    let saml_provider_key = Canary::new("saml_provider_key");
    let connector_credential = Canary::new("connector_credential");
    let Some(rig) = Rig::new(sso_client_secret.value(), saml_provider_key.value()).await else {
        println!("SKIPPED: no PostgreSQL test database could be created");
        return;
    };
    let site = rig.site().to_owned();
    for entry in PLANTED_ELSEWHERE {
        println!(
            "SKIPPED planting: `{}` (hosted by {}, not merged yet)",
            entry.label, entry.owner
        );
    }
    assert!(
        PLANTED_ELSEWHERE.is_empty(),
        "a canary is skipped: plant it through its service"
    );

    // --- the canaries that are made up -------------------------------------------------------
    let note_token = Canary::new("secret_field");
    let webhook_secret = Canary::new("webhook_secret");
    let admin_password = Canary::new("sign_in_phrase");
    let member_password = Canary::new("cli_phrase");
    let wrong_password = Canary::new("wrong_phrase");
    let bad_bearer = Canary::new("bad_bearer");
    let rejected_value = Canary::new("rejected_value");
    // 43 characters, the length of an API key's secret part.
    let wrong_key_secret = Canary::new("wrongsecret");
    assert_eq!(wrong_key_secret.value().len(), 43);
    // Not the control of `the_log_capture_sees_a_logged_canary`: that test runs in the same
    // process and its line must not satisfy this one.
    let control = Canary::new("sweep_control");

    // --- the users: a password chosen through the CLI ------------------------------------------
    rig.env.user("member@example.com", member_password.value());

    let started = rig.start().await;
    let addr = started.addr;

    // The Administrator chooses a password through the sign-in service.
    let first = common::login(addr, &site, "administrator", &rig.first_secret).await;
    common::expect(&first, 200);
    let first_token = first.body["data"]["token"].as_str().unwrap().to_owned();
    let changed = call(
        addr,
        &site,
        Some(&first_token),
        "POST",
        "/api/v1/auth/password",
        Some(json!({"current": rig.first_secret, "new": admin_password.value()})),
    )
    .await;
    common::expect(&changed, 200);
    let admin = changed.body["data"]["token"].as_str().unwrap().to_owned();
    // Provider administration needs a recent step-up (WP-IAM-12.T9: a fresh sign-in no longer
    // counts as one): the Administrator proves the password again on this session.
    let stepped = call(
        addr,
        &site,
        Some(&admin),
        "POST",
        "/api/v1/auth/step-up",
        Some(json!({"password": admin_password.value()})),
    )
    .await;
    common::expect(&stepped, 200);
    let member = common::token_of(addr, &site, "member@example.com", member_password.value()).await;

    // --- the canaries the platform made ------------------------------------------------------
    let (key_id, key_secret) = rig
        .admin_api_key
        .strip_prefix("spk_")
        .and_then(|rest| rest.split_once('_'))
        .map(|(id, secret)| (id.to_owned(), secret.to_owned()))
        .expect("the bootstrap key is spk_<id>_<secret>");
    let mut canaries = vec![
        note_token.clone(),
        webhook_secret.clone(),
        admin_password.clone(),
        member_password.clone(),
        wrong_password.clone(),
        bad_bearer.clone(),
        rejected_value.clone(),
        wrong_key_secret.clone(),
        Canary::from_value("api_key", &rig.admin_api_key),
        Canary::from_value("api_key_secret_part", &key_secret),
        Canary::from_value("initial_secret", &rig.first_secret),
        Canary::from_value("restricted_session_token", &first_token),
        Canary::from_value("admin_session_token", &admin),
        Canary::from_value("member_session_token", &member),
        sso_client_secret.clone(),
        saml_provider_key.clone(),
        connector_credential.clone(),
    ];
    for (n, text) in rig.key_texts().iter().enumerate() {
        if text.len() >= 16 {
            canaries.push(Canary::from_value(&format!("kek_material_{n}"), text));
        }
    }
    if let Some(password) = artefacts::pg_password(&rig.env.url) {
        canaries.push(Canary::from_value("pg_admin_password", &password));
    }

    // --- plant: the secret field and the webhook secret, through the engine's write path ------
    let note_id = rig::plant_note(&started, &site, note_token.value())
        .await
        .unwrap_or_else(|code| panic!("planting the secret field failed: {code}"));
    let hook_id = rig::plant_webhook(&started, &site, webhook_secret.value())
        .await
        .unwrap_or_else(|code| panic!("planting the webhook secret failed: {code}"));

    let mut art = Artefacts::default();

    // --- plant: the four credentials their services host -----------------------------------------
    // scim_credential: a SCIM key issued the way the administration command issues it, then used.
    let (scim_key, scim_replies) = rig::plant_scim(&rig, &started, &admin)
        .await
        .unwrap_or_else(|why| panic!("planting the SCIM credential failed: {why}"));
    canaries.push(Canary::from_value("scim_credential", &scim_key));
    if let Some((_, secret)) = scim_key
        .strip_prefix("spk_")
        .and_then(|rest| rest.split_once('_'))
    {
        canaries.push(Canary::from_value("scim_credential_secret_part", secret));
    }
    for (name, reply) in &scim_replies {
        art.add_reply(&format!("live:{name}"), reply);
    }
    let wrong_scim = Canary::new("wrong_scim_key");
    canaries.push(wrong_scim.clone());
    let reply = call(
        addr,
        &site,
        Some(wrong_scim.value()),
        "GET",
        "/scim/v2/Users",
        None,
    )
    .await;
    art.add_reply("failed:wrong_scim_key", &reply);
    // sso_client_secret: an OpenID Connect provider that names the secret (the value is read from
    // its file only when a sign-in needs it); saml_provider_key: a SAML provider whose service
    // provider signs with the named key.
    let sso_replies = rig::plant_oidc_provider(&rig, &started, &admin)
        .await
        .unwrap_or_else(|why| panic!("planting the OpenID Connect provider failed: {why}"));
    for (name, reply) in &sso_replies {
        art.add_reply(&format!("live:{name}"), reply);
    }

    let loaded = spot_cli::config::load(rig.root(), false).unwrap();
    let site_cfg = loaded.sites.iter().find(|s| s.site == site).unwrap();
    let backend = spot_cli::schema::backend(&loaded, site_cfg).await.unwrap();
    rig::plant_saml_provider(&backend, &site)
        .await
        .unwrap_or_else(|why| panic!("planting the SAML provider failed: {why}"));
    // connector_credential: the connector vault's write-only action.
    rig::plant_connector_credential(&rig, &backend, connector_credential.value())
        .await
        .unwrap_or_else(|why| panic!("planting the connector credential failed: {why}"));

    // --- the bodies of failed requests ---------------------------------------------------------
    let wrong_key = format!("spk_{key_id}_{}", wrong_key_secret.value());
    let reply = common::login(addr, &site, "administrator", wrong_password.value()).await;
    art.add_reply("failed:login_wrong_password", &reply);
    let reply = call(
        addr,
        &site,
        Some(bad_bearer.value()),
        "GET",
        "/api/v1/auth/session",
        None,
    )
    .await;
    art.add_reply("failed:unknown_bearer", &reply);
    let reply = call(
        addr,
        &site,
        Some(&wrong_key),
        "GET",
        "/api/v1/meta/task",
        None,
    )
    .await;
    art.add_reply("failed:wrong_api_key", &reply);
    let truncated = format!("{{\"title\":\"{}\"", rejected_value.value());
    let reply = common::http_raw(
        addr,
        &site,
        Some(&admin),
        "POST",
        "/api/v1/resource/vault_note",
        &[],
        ("application/json", truncated.as_bytes()),
    )
    .await;
    art.add_reply("failed:malformed_body", &reply);
    let reply = call(
        addr,
        &site,
        Some(&admin),
        "POST",
        "/api/v1/resource/vault_note",
        Some(json!({"title": {"nested": rejected_value.value()}, "unknown_field": rejected_value.value()})),
    )
    .await;
    art.add_reply("failed:invalid_fields", &reply);
    let reply = call(
        addr,
        &site,
        Some(&admin),
        "POST",
        "/api/v1/resource/vault_note",
        Some(json!({"title": "x", "token": rejected_value.value()})),
    )
    .await;
    art.add_reply("failed:secret_field_over_http", &reply);
    let reply = call(addr, &site, Some(&member), "GET", "/metrics", None).await;
    art.add_reply("failed:forbidden_metrics", &reply);
    let reply = call(
        addr,
        &site,
        Some(&member),
        "POST",
        "/api/v1/export",
        Some(
            json!({"doctype": "vault_note", "columns": ["title"], "note": rejected_value.value()}),
        ),
    )
    .await;
    art.add_reply("failed:forbidden_export", &reply);

    // --- reads, exports and the event stream ---------------------------------------------------
    for (name, path) in [
        (
            "list_vault_note",
            "/api/v1/resource/vault_note?limit=100".to_owned(),
        ),
        (
            "read_vault_note",
            format!("/api/v1/resource/vault_note/{note_id}"),
        ),
        (
            "list_webhook",
            "/api/v1/resource/webhook?limit=100".to_owned(),
        ),
        (
            "read_webhook",
            format!("/api/v1/resource/webhook/{hook_id}"),
        ),
        (
            "list_api_key",
            "/api/v1/resource/api_key?limit=100".to_owned(),
        ),
        ("list_user", "/api/v1/resource/user?limit=100".to_owned()),
        (
            "list_session",
            "/api/v1/resource/session?limit=100".to_owned(),
        ),
        ("metrics", "/metrics".to_owned()),
        ("health_ready", "/api/health/ready".to_owned()),
    ] {
        let reply = call(addr, &site, Some(&admin), "GET", &path, None).await;
        art.add_reply(&format!("live:{name}"), &reply);
    }
    let mut watermark = None;
    for (name, spec) in [
        (
            "ndjson",
            json!({"doctype": "vault_note", "columns": ["title"], "format": "ndjson"}),
        ),
        (
            "csv_every_column",
            json!({"doctype": "vault_note", "format": "csv"}),
        ),
        (
            "secret_column_refused",
            json!({"doctype": "vault_note", "columns": ["title", "token"], "format": "ndjson"}),
        ),
        ("user", json!({"doctype": "user", "format": "ndjson"})),
        ("api_key", json!({"doctype": "api_key", "format": "ndjson"})),
        ("session", json!({"doctype": "session", "format": "ndjson"})),
        ("webhook", json!({"doctype": "webhook", "format": "ndjson"})),
    ] {
        let reply = call(
            addr,
            &site,
            Some(&admin),
            "POST",
            "/api/v1/export",
            Some(spec),
        )
        .await;
        if name == "ndjson" {
            watermark = reply.header("x-export-watermark").map(str::to_owned);
        }
        art.add_reply(&format!("export:{name}"), &reply);
    }
    if let Some(watermark) = watermark {
        let reply = call(
            addr,
            &site,
            Some(&admin),
            "POST",
            "/api/v1/export/extract",
            Some(json!({"doctype": "vault_note", "watermark": watermark, "limit": 100})),
        )
        .await;
        art.add_reply("export:extract", &reply);
    }
    for (name, entity, mode) in [
        ("canary_ids", "vault_note", "ids_only"),
        ("canary_fields", "vault_note", "fields"),
        ("canary_hooks", "webhook", "fields"),
        ("canary_keys", "api_key", "fields"),
        ("canary_users", "user", "fields"),
    ] {
        let path = format!("/api/v1/events/consumers/{name}");
        let put = call(
            addr,
            &site,
            Some(&admin),
            "PUT",
            &path,
            Some(json!({
                "topics": ["doc_created"],
                "filter": format!("doc.subject.entity == '{entity}'"),
                "partitions": 1,
                "payload_mode": mode,
                "start": {"kind": "from_beginning"},
            })),
        )
        .await;
        art.add_reply(&format!("events:{name}:put"), &put);
        let pull = call(
            addr,
            &site,
            Some(&admin),
            "POST",
            &format!("{path}/pull"),
            Some(json!({"partition": 0, "limit": 100})),
        )
        .await;
        art.add_reply(&format!("events:{name}"), &pull);
    }

    // --- the journals: the secret journal, and the identity audit chain -------------------------
    {
        let live = started.running.live(&site).unwrap();
        let engine = live.engine().unwrap();
        let access = engine.secret_access(SecretCaller::Operator, &rig::invocation(&site));
        let journal = engine.secret_audit_journal(&access).await.unwrap();
        art.add("journal:secret_audit", format!("{journal:#?}").into_bytes());
        art.add(
            "journal:secret_audit_log",
            format!("{:#?}", engine.secret_audit_log()).into_bytes(),
        );
    }
    {
        let client = backend.pool().get().await.unwrap();
        let rows = client
            .query("SELECT record FROM spot_audit_chain ORDER BY seq", &[])
            .await
            .unwrap();
        let chain: Vec<String> = rows.iter().map(|r| r.get::<_, String>(0)).collect();
        art.add("journal:identity_chain", chain.join("\n").into_bytes());
    }

    // --- a control: the capture must see what the host would log --------------------------------
    tracing::info!(target: "secret_canary_sweep", control = %control.value(), "control line");

    started.finish().await;

    // --- the sealed columns: present, non-empty, and not the plaintext --------------------------
    {
        let client = backend.pool().get().await.unwrap();
        for (table, column) in [
            ("vault_note", "token__sealed"),
            ("webhook", "secret__sealed"),
            ("api_key", "digest__sealed"),
            ("spot_connector_secret", "sealed"),
        ] {
            let sql = format!("SELECT {column} FROM {table}");
            let rows = client
                .query(sql.as_str(), &[])
                .await
                .unwrap_or_else(|_| panic!("the sealed column {table}.{column} does not exist"));
            let mut sealed = 0;
            for row in &rows {
                if let Some(bytes) = row.get::<_, Option<Vec<u8>>>(0) {
                    assert!(!bytes.is_empty(), "{table}.{column} holds an empty value");
                    art.add(format!("db:{table}.{column}"), bytes);
                    sealed += 1;
                }
            }
            assert!(sealed > 0, "{table}.{column} holds no sealed value");
        }
    }

    // --- the commands -----------------------------------------------------------------------------
    let verify = rig.spot(&["audit", "verify", "--site", &site]);
    assert!(
        verify.status.success(),
        "audit verify exited with {:?}",
        verify.status.code()
    );
    art.add("cli:audit_verify", rig::printed(&verify));

    let bundle = rig.dir.join("support.zip");
    let out = rig.spot(&["support", "bundle", "--out", bundle.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "support bundle exited with {:?}",
        out.status.code()
    );
    art.add("cli:support_bundle_output", rig::printed(&out));
    art.add("support_bundle:zip", std::fs::read(&bundle).unwrap());
    let entries = artefacts::zip_entries(&bundle);
    assert!(!entries.is_empty(), "the support bundle is empty");
    for (name, bytes) in entries {
        art.add(format!("support_bundle:{name}"), bytes);
    }

    let mut skipped = Vec::new();
    match &rig.bin {
        None => {
            skipped.push("backup and pg_dump (no PostgreSQL client tools: set SPOT_TEST_PG_BIN)")
        }
        Some(bin) => {
            for (n, extra) in [None, Some("--allow-unencrypted")].into_iter().enumerate() {
                let mut args = vec!["backup", "run", "--site", site.as_str()];
                args.extend(extra);
                // A backup is stored in clear only when the site has no key provider
                // (`--allow-unencrypted` alone does not turn the encryption of a site that
                // has one off): the second run is made with the `[services.secrets]` table
                // of the configuration left out, then the configuration is put back.
                let common = rig.root().join("common.toml");
                let original = std::fs::read_to_string(&common).unwrap();
                if extra.is_some() {
                    let without: String = original
                        .split_inclusive('\n')
                        .filter(|line| {
                            !line.starts_with("[services.secrets]")
                                && !line.starts_with("kek_file =")
                        })
                        .collect();
                    assert_ne!(without, original, "the secrets table was not in the config");
                    std::fs::write(&common, without).unwrap();
                }
                let run = rig.spot(&args);
                std::fs::write(&common, original).unwrap();
                assert!(
                    run.status.success(),
                    "backup run #{n} exited with {:?}",
                    run.status.code()
                );
                art.add(format!("cli:backup_run_{n}"), rig::printed(&run));
            }
            let list = rig.spot(&["backup", "list", "--site", &site]);
            art.add("cli:backup_list", rig::printed(&list));
            art.add_tree("backup:target", &rig.backups, &[]);
            let mut packages: Vec<_> = std::fs::read_dir(&rig.backups)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "spotbak"))
                .collect();
            packages.sort();
            let (mut clear, mut sealed) = (0, 0);
            for (n, path) in packages.iter().enumerate() {
                let package = spot_backup::package::open(path).unwrap();
                if package.manifest().encryption.is_some() {
                    sealed += 1;
                    continue;
                }
                clear += 1;
                let dest = rig.dir.join(format!("unpacked_{n}"));
                spot_backup::package::unpack(&package, &dest).unwrap();
                art.add_tree(&format!("backup:unpacked_{n}"), &dest, &[]);
            }
            assert!(
                clear >= 1 && sealed >= 1,
                "the sweep needs one encrypted and one unencrypted package ({sealed} and {clear})"
            );
            match artefacts::pg_dump(bin, &rig.env.url, &site_cfg.database) {
                Ok(dump) => art.add("pg_dump:site_database", dump),
                Err(why) => panic!("{why}"),
            }
        }
    }
    for reason in &skipped {
        println!("SKIPPED artefacts: {reason}");
    }

    // --- the files of the data directory, but not the key file itself ------------------------------
    art.add_tree(
        "files:data",
        &rig.root().join("sites"),
        std::slice::from_ref(&rig.kek),
    );
    art.add_tree("files:state", &rig.root().join("state"), &[]);

    // --- the log of the whole run --------------------------------------------------------------------
    let log_bytes = log.snapshot();
    let control_hits = canary::scan(
        "logs:in_process",
        &log_bytes,
        std::slice::from_ref(&control),
    );
    assert!(
        !control_hits.is_empty(),
        "the capture did not see a logged canary: the sweep could not see a leak"
    );
    art.add("logs:in_process", log_bytes);

    // --- the artefacts that must exist, then the sweep ---------------------------------------------
    let mut required = vec![
        "failed:login_wrong_password",
        "failed:unknown_bearer",
        "failed:malformed_body",
        "live:read_vault_note",
        "live:metrics",
        "export:ndjson",
        "events:canary_fields",
        "journal:secret_audit",
        "journal:identity_chain",
        "cli:audit_verify",
        "cli:support_bundle_output",
        "support_bundle:zip",
        "db:vault_note.token__sealed",
        "db:webhook.secret__sealed",
        "db:spot_connector_secret.sealed",
        "live:sso:provider_create",
        "live:scim:users",
        "failed:wrong_scim_key",
        "logs:in_process",
    ];
    if rig.bin.is_some() {
        required.extend(["pg_dump:site_database", "backup:target/ledger.toml"]);
    }
    let missing = art.missing(&required);
    assert!(
        missing.is_empty(),
        "these artefacts were not collected: {}",
        missing.join(", ")
    );
    println!(
        "secret canary sweep: {} canaries against {} artefacts",
        canaries.len(),
        art.names().len()
    );
    for (name, size) in art.names() {
        println!("  {name}: {size} bytes");
    }

    let hits: BTreeSet<String> = art
        .sweep(&canaries)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        hits.is_empty(),
        "secret canaries reached artefacts ({} findings, labels and artefacts only):\n{}",
        hits.len(),
        hits.into_iter().collect::<Vec<_>>().join("\n")
    );
    // The key file is the one place the key material lives.
    assert!(rig.kek.is_file(), "the key file is gone");
}
