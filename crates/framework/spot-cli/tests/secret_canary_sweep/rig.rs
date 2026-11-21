//! The site the secret canary sweep runs on (WP-SEC-03.T15): one host over a real PostgreSQL
//! with the secrets service, the event stream, the identity file mirror and a backup folder
//! configured, and the write paths the sweep plants its canaries through.
//!
//! Nothing here prints a secret: a failure names a code or a label, never a value.

use std::path::{Path, PathBuf};

use std::process::{Command, Output, Stdio};

use serde_json::json;
use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::{Name, RecordId, Value};
use spot_core::SiteId;
use spot_document::secrets::SecretCaller;
use spot_secrets::SecretString;

use crate::common::{self, Env, Started};

/// A DocType with one sealed, blind-indexed field, written the way `secrets_host.rs` writes it.
const NOTE: &str = r#"name = "vault_note"
label = "Vault Note"
app = "sample"
module = "projects"

[[protection_groups]]
name = "credential"
label = "Credential"
trace = true
sync_class_hint = "home_only"

[[fields]]
name = "title"
label = "Title"
type = "text"
max_chars = 50

[[fields]]
name = "token"
label = "Token"
type = "secret"
lookup = true
unique = true
protection_group = "credential"
classification = ["secret"]
no_copy = true
"#;

/// The PostgreSQL client tools directory (`SPOT_TEST_PG_BIN`, else the first `pg_dump` on
/// `PATH`), as `backup.rs` finds it.
pub fn pg_tools() -> Option<PathBuf> {
    if let Ok(root) = std::env::var("SPOT_TEST_PG_BIN") {
        let root = PathBuf::from(root);
        return [root.join("bin"), root]
            .into_iter()
            .find(|d| d.join("pg_dump").is_file());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find(|dir| dir.join("pg_dump").is_file())
}

/// A host root, its site, its key file and its backup folder.
pub struct Rig {
    pub env: Env,
    /// A private directory outside the root: the key file, the backup folder, the outputs.
    pub dir: PathBuf,
    pub kek: PathBuf,
    pub backups: PathBuf,
    pub bin: Option<PathBuf>,
    /// The key the site was created with (`new-site --admin-api-key-file`).
    pub admin_api_key: String,
    /// The one-time secret `new-site` printed for the Administrator.
    pub first_secret: String,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(
            &self.backups,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        );
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    /// The site, created with its API key file and its key file; `None` without a PostgreSQL.
    ///
    /// `sso_client_secret` and `saml_provider_key` are written to files of the private directory
    /// (mode 0600) that `[services.sso_identity] client_secrets` refers to, the only way the
    /// platform takes those two values.
    pub async fn new(sso_client_secret: &str, saml_provider_key: &str) -> Option<Rig> {
        let dir = std::env::temp_dir().join(format!("spot_canary_sweep_{}", RecordId::new_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let backups = dir.join("backups");
        std::fs::create_dir_all(&backups).unwrap();
        let oidc_file = dir.join("sso_client_secret");
        let saml_file = dir.join("saml_sp_key");
        for (file, value) in [
            (&oidc_file, sso_client_secret),
            (&saml_file, saml_provider_key),
        ] {
            std::fs::write(file, format!("{value}\n")).unwrap();
            std::fs::set_permissions(file, std::os::unix::fs::PermissionsExt::from_mode(0o600))
                .unwrap();
        }
        let services = format!(
            "[services.secrets]\nkek_file = {:?}\n[services.events]\nenabled = true\n\
             [services.scim]\nenabled = true\nrequests_per_window = 100000\n\
             [services.signin_throttle]\nper_account = {{ capacity = 100000, per_seconds = 900 }}\n\
             per_source = {{ capacity = 100000, per_seconds = 900 }}\n\
             per_tenant = {{ capacity = 10000000, per_seconds = 900 }}\n\
             [services.sso_identity]\nenabled = true\nbase_url = \"https://sweep.example.test\"\n\
             [services.sso_identity.client_secrets]\nsweep-oidc = {:?}\nsweep-saml-sp = {:?}\n\
             [audit]\nidentity_file_mirror = true\n[backup]\ndefault_target = \"main\"\n\
             [backup.targets.main]\nkind = \"folder\"\npath = {:?}\n",
            format!("{}/{{site}}.json", dir.display()),
            format!("file:{}", oidc_file.display()),
            format!("file:{}", saml_file.display()),
            backups.display().to_string()
        );
        let env = Env::new(&services).await?;
        std::fs::write(env.root.0.join("sample/doctypes/vault_note.toml"), NOTE).unwrap();
        let key_file = dir.join("admin.key");
        let out = common::ok(
            &env.root.0,
            &[
                "new-site",
                &env.site,
                "--admin-api-key-file",
                key_file.to_str().unwrap(),
            ],
        );
        let first_secret = common::secret(&out);
        let admin_api_key = std::fs::read_to_string(&key_file)
            .unwrap()
            .trim()
            .to_owned();
        let kek = dir.join(format!("{}.json", env.site));
        spot_cli::services::secrets::init_key_file(&kek, &env.site)
            .await
            .unwrap();
        Some(Rig {
            env,
            dir,
            kek,
            backups,
            bin: pg_tools(),
            admin_api_key,
            first_secret,
        })
    }

    pub fn site(&self) -> &str {
        &self.env.site
    }

    pub fn root(&self) -> &Path {
        &self.env.root.0
    }

    /// The key texts of the key file as stored: the key material the sweep must find nowhere
    /// else.
    pub fn key_texts(&self) -> Vec<String> {
        let text = std::fs::read_to_string(&self.kek).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        json["keys"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|k| k["key"].as_str().map(str::to_owned))
            .collect()
    }

    /// Runs the binary with the sweep's environment: trace-level logs, the client tools and a
    /// private state directory.
    pub fn spot(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_spot"));
        command
            .arg("--root")
            .arg(self.root())
            .args(args)
            .env("SPOT_LOG", "trace")
            .env("XDG_STATE_HOME", self.root().join("state"))
            .stdin(Stdio::null());
        if let Some(bin) = &self.bin {
            command.env("SPOT_PG_BIN", bin);
        }
        command.output().unwrap()
    }

    pub async fn start(&self) -> Started {
        common::start(&self.env).await
    }
}

/// What a process printed, both streams.
pub fn printed(output: &Output) -> Vec<u8> {
    let mut bytes = output.stdout.clone();
    bytes.extend_from_slice(&output.stderr);
    bytes
}

pub fn invocation(site: &str) -> Invocation {
    Invocation {
        site: SiteId::new(site).unwrap(),
        actor: Actor::System {
            component: Name::new("test").unwrap(),
        },
        command_id: None,
        language: None,
        now: time::OffsetDateTime::now_utc(),
    }
}

/// Writes a `vault_note` with its `token` sealed through the engine's write path. Returns the
/// id, or the stable error code.
pub async fn plant_note(started: &Started, site: &str, token: &str) -> Result<RecordId, String> {
    let live = started.running.live(site).ok_or("no live site")?;
    let engine = live.engine().ok_or("no engine")?;
    let access = engine.secret_access(SecretCaller::Seed, &invocation(site));
    let mut doc = spot_app_api::Document::new(Name::new("vault_note").unwrap(), RecordId::new_v7());
    doc.set(Name::new("title").unwrap(), "note");
    let id = doc.id();
    engine
        .create_with_secrets(
            &access,
            doc,
            vec![("token".to_owned(), SecretString::new(token))],
        )
        .await
        .map_err(|e| e.code.clone())?;
    Ok(id)
}

/// Writes a `webhook` with its signing `secret` sealed, through the same engine write path the
/// webhook service reads it back through.
pub async fn plant_webhook(
    started: &Started,
    site: &str,
    secret: &str,
) -> Result<RecordId, String> {
    let live = started.running.live(site).ok_or("no live site")?;
    let engine = live.engine().ok_or("no engine")?;
    let access = engine.secret_access(SecretCaller::Seed, &invocation(site));
    let mut doc = spot_app_api::Document::new(Name::new("webhook").unwrap(), RecordId::new_v7());
    doc.set(Name::new("name").unwrap(), "canary-hook");
    doc.set(
        Name::new("url").unwrap(),
        "https://hooks.example.test/canary",
    );
    doc.set(Name::new("doctype").unwrap(), "vault_note");
    doc.set(
        Name::new("events").unwrap(),
        Value::Json("[\"create\"]".to_owned()),
    );
    let id = doc.id();
    engine
        .create_with_secrets(
            &access,
            doc,
            vec![("secret".to_owned(), SecretString::new(secret))],
        )
        .await
        .map_err(|e| e.code.clone())?;
    Ok(id)
}

/// The client secret name the OpenID Connect provider refers to.
pub const OIDC_SECRET_NAME: &str = "sweep-oidc";
/// The name of the SAML service provider's signing key.
pub const SAML_KEY_NAME: &str = "sweep-saml-sp";

/// Creates a `service` user through the native API and issues a SCIM key owned by it through
/// [`ScimKeyIssuer`] (what an administration command calls), then makes one SCIM call with it.
/// Returns the key text, once, and the replies to keep as artefacts.
pub async fn plant_scim(
    rig: &Rig,
    started: &Started,
    admin: &str,
) -> Result<(String, Vec<(String, common::Reply)>), String> {
    use std::sync::Arc;

    use spot_cli::services::scim::{ScimKeyIssuer, ScimService};
    use spot_cli::services::secrets as svc;
    use spot_core::SystemClock;
    use spot_iam_scim::{Limits, parse_record_id};

    let site = rig.site();
    let created = common::http(
        started.addr,
        site,
        Some(admin),
        "POST",
        "/api/v1/resource/user",
        &[],
        Some(json!({
            "user_name": "scim-bot",
            "full_name": "SCIM bot",
            "user_type": "service",
            "state": "active",
        })),
    )
    .await;
    if created.status != 201 {
        return Err(format!(
            "creating the service user answered {}",
            created.status
        ));
    }
    let owner = created.body["data"]["id"]
        .as_str()
        .ok_or("no id for the service user")?
        .to_owned();
    let loaded = spot_cli::config::load(rig.root(), false).map_err(|e| e.to_string())?;
    let site_cfg = loaded
        .sites
        .iter()
        .find(|s| s.site == site)
        .ok_or("no such site")?;
    let host = svc::open(rig.root(), &loaded, site_cfg)
        .await
        .map_err(|e| e.to_string())?;
    let meta = spot_cli::services::metadata::effective(rig.root(), &loaded, site_cfg)
        .map_err(|e| e.to_string())?;
    let catalog = Arc::new(spot_cli::schema::catalog(&meta).map_err(|e| e.to_string())?);
    let live = started.running.live(site).ok_or("no live site")?;
    let issuer = ScimService::new(
        site.to_owned(),
        live,
        Arc::new(SystemClock::new()),
        catalog,
        host,
        Limits::default(),
    );
    let key = issuer
        .issue(
            parse_record_id(&owner).ok_or("bad owner id")?,
            "sweep",
            None,
        )
        .await?;
    let mut replies = Vec::new();
    for (name, path) in [
        ("scim:users", "/scim/v2/Users?count=10"),
        (
            "scim:service_provider_config",
            "/scim/v2/ServiceProviderConfig",
        ),
    ] {
        let reply = common::http(started.addr, site, Some(&key), "GET", path, &[], None).await;
        replies.push((name.to_owned(), reply));
    }
    Ok((key, replies))
}

/// Creates an OpenID Connect provider through the administration API; the provider names the
/// client secret ([`OIDC_SECRET_NAME`]), never holds it.
pub async fn plant_oidc_provider(
    rig: &Rig,
    started: &Started,
    admin: &str,
) -> Result<Vec<(String, common::Reply)>, String> {
    let site = rig.site();
    let created = common::http(
        started.addr,
        site,
        Some(admin),
        "POST",
        "/api/v1/identity/sso/providers",
        &[("Idempotency-Key", "sweep-oidc-provider-1")],
        Some(json!({
            "kind": "oidc",
            "name": "sweep-oidc",
            "audience": ["staff"],
            "oidc": {
                "issuer": "https://idp.sweep.example.test/",
                "client_id": "sweep-client",
                "client_secret_name": OIDC_SECRET_NAME,
            },
        })),
    )
    .await;
    if created.status != 201 {
        return Err(format!(
            "creating the provider answered {}: {}",
            created.status,
            String::from_utf8_lossy(&created.bytes[..created.bytes.len().min(300)])
        ));
    }
    let mut replies = vec![("sso:provider_create".to_owned(), created)];
    for (name, path) in [
        ("sso:provider_list", "/api/v1/identity/sso/providers"),
        (
            "sso:provider_get",
            "/api/v1/identity/sso/providers/sweep-oidc",
        ),
    ] {
        let reply = common::http(started.addr, site, Some(admin), "GET", path, &[], None).await;
        replies.push((name.to_owned(), reply));
    }
    Ok(replies)
}

/// Stores a SAML provider whose service provider signs with the key named [`SAML_KEY_NAME`]:
/// the administration API never sets a signing key, so this goes through the store the
/// administration API itself writes (`PgSamlStore::put`).
pub async fn plant_saml_provider(
    backend: &spot_postgres::PgBackend,
    site: &str,
) -> Result<(), String> {
    use spot_cli::services::sso::SamlProvider;
    use spot_cli::services::sso_saml_store::PgSamlStore;
    use spot_core::Audience;
    use spot_sso::ProviderId;
    use spot_sso::saml::{IdpConfig, SpConfig};

    let id = ProviderId::new("sweep-saml");
    let sp = SpConfig {
        entity_id: "https://sweep.example.test/api/v1/auth/sso/sweep-saml/saml-metadata".to_owned(),
        acs_url: "https://sweep.example.test/api/v1/auth/sso/sweep-saml/acs".to_owned(),
        signing_key_ref: Some(SAML_KEY_NAME.to_owned()),
        ..SpConfig::default()
    };
    let idp = IdpConfig::new(
        id.clone(),
        "https://idp.sweep.example.test/entity",
        "https://idp.sweep.example.test/sso",
        vec![vec![0x30, 0x82, 0x01, 0x0a]],
    );
    let store = PgSamlStore::new(backend.pool(), site).ok_or("not a tenant name")?;
    store
        .put(
            &id,
            &SamlProvider {
                sp,
                idp,
                name: "Sweep IdP".to_owned(),
                audience: Audience::Staff,
            },
            None,
        )
        .await
        .map(|_| ())
        .map_err(|e| format!("{e:?}"))
}

/// Writes a connector credential through the vault's write-only action (`SecretAdmin::put` of the
/// durable vault the connector service holds), sealed by the site's own key file into the
/// service's own table.
pub async fn plant_connector_credential(
    rig: &Rig,
    backend: &spot_postgres::PgBackend,
    value: &str,
) -> Result<(), String> {
    use std::sync::Arc;

    use spot_cli::services::connectors::vault::{DurableVault, PgSealedStore};
    use spot_cli::services::secrets as svc;

    let loaded = spot_cli::config::load(rig.root(), false).map_err(|e| e.to_string())?;
    let site_cfg = loaded
        .sites
        .iter()
        .find(|s| s.site == rig.site())
        .ok_or("no such site")?;
    let host = svc::open(rig.root(), &loaded, site_cfg)
        .await
        .map_err(|e| e.to_string())?
        .ok_or("no secrets service")?;
    let sealed = PgSealedStore::new(backend.pool());
    sealed.prepare().await?;
    let vault = DurableVault::new(rig.site(), Arc::clone(host.keyring()), Arc::new(sealed));
    vault
        .put("sweep-connector-token", &SecretString::new(value))
        .await
}
