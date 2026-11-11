//! WP-SEC-03.T9: the secrets service is hosted. A key file configured in `common.toml` becomes
//! the vault behind the engine's `secret` fields; a bad one refuses to start the host; `spot
//! secrets` rotates the key while a writer runs. Real PostgreSQL (skipped without
//! SPOT_TEST_PG_URL).
#![allow(
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    dead_code,
    unused_imports
)]

mod common;

/// The command registry, driven without a process (the technique of `commands_registry.rs`).
#[path = "../src/commands/mod.rs"]
mod commands;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use commands::{CommandCtx, Exit};
use common::{Env, Started};
use spot_app_api::context::{Actor, Invocation};
use spot_app_api::prelude::{Name, RecordId};
use spot_cli::services::secrets as svc;
use spot_core::{ManualClock, SiteId};
use spot_document::secrets::SecretCaller;
use spot_secrets::SecretString;
use time::macros::datetime;

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

// --- a shared writer -------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<u8>>>);
impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Sink {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

/// Runs `spot secrets <args>` and returns its exit and everything it printed.
async fn secrets_cmd(root: &Path, args: &[&str]) -> (Exit, String) {
    let (out, err) = (Sink::default(), Sink::default());
    let clock = Arc::new(ManualClock::new(datetime!(2026-11-06 09:00:00 UTC)));
    let mut ctx = CommandCtx::new(
        root.to_path_buf(),
        clock,
        Box::new(out.clone()),
        Box::new(err.clone()),
    );
    let registration = commands::COMMANDS
        .iter()
        .find(|r| r.name == "secrets")
        .expect("the secrets command registers itself");
    let mut argv = vec!["secrets"];
    argv.extend_from_slice(args);
    let matches = (registration.command)()
        .try_get_matches_from(argv)
        .expect("the arguments parse");
    let exit = match (registration.run)(&matches, &mut ctx).await {
        Ok(exit) => exit,
        Err(e) => {
            let _ = writeln!(ctx.err(), "{e}");
            Exit::Failed
        }
    };
    (exit, format!("{}{}", out.text(), err.text()))
}

// --- the rig ---------------------------------------------------------------------------------

struct Rig {
    env: Env,
    dir: PathBuf,
    kek: PathBuf,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Rig {
    /// A host with the `vault_note` DocType and a key file configured for its site.
    async fn new() -> Option<Rig> {
        let dir = std::env::temp_dir().join(format!("spot_secrets_t9_{}", RecordId::new_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let services = format!(
            "[services.secrets]\nkek_file = {:?}\n",
            format!("{}/{{site}}.json", dir.display())
        );
        let env = Env::new(&services).await?;
        std::fs::write(env.root.0.join("sample/doctypes/vault_note.toml"), NOTE).unwrap();
        env.site();
        let kek = dir.join(format!("{}.json", env.site));
        Some(Rig { env, dir, kek })
    }

    async fn init_key_file(&self) {
        svc::init_key_file(&self.kek, &self.env.site).await.unwrap();
    }

    /// The key texts of the file as stored, to prove none of them is printed anywhere.
    fn key_texts(&self) -> Vec<String> {
        let text = std::fs::read_to_string(&self.kek).unwrap();
        let json: serde_json::Value = serde_json::from_str(&text).unwrap();
        json["keys"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|k| k["key"].as_str().map(str::to_owned))
            .collect()
    }

    async fn try_start(&self) -> Result<Started, spot_cli::Error> {
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let running =
            spot_cli::start_with(&self.env.root.0, false, common::fast_options(), async {
                let _ = stopped.await;
            })
            .await?;
        let addr = running.local_addrs()[0];
        Ok(Started {
            running,
            addr,
            stop,
        })
    }

    async fn start(&self) -> Started {
        self.try_start().await.unwrap_or_else(|e| panic!("{e}"))
    }

    fn secrets_cmd_root(&self) -> &Path {
        &self.env.root.0
    }
}

/// The text with which the host refuses the site's key file: what `spot serve` prints when the
/// site is unavailable. Panics when the key file is accepted.
async fn refusal(rig: &Rig) -> String {
    let root = &rig.env.root.0;
    let loaded = spot_cli::config::load(root, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == rig.env.site)
        .unwrap();
    match svc::open(root, &loaded, site).await {
        Err(e) => e.0,
        Ok(None) => panic!("the host configures no secrets service"),
        Ok(Some(host)) => {
            let backend = spot_cli::schema::backend(&loaded, site).await.unwrap();
            let meta = spot_cli::services::metadata::effective(root, &loaded, site).unwrap();
            let catalog = Arc::new(spot_cli::schema::catalog(&meta).unwrap());
            svc::verify_values(&host, &backend, &catalog, &meta)
                .await
                .expect_err("the key file is refused")
                .0
        }
    }
}

/// The host starts, but the site answers 503 `server.unavailable` to everything: it serves
/// nothing, with no plaintext fallback.
async fn assert_site_unavailable(rig: &Rig) {
    let started = rig.start().await;
    let reply = common::http(
        started.addr,
        &rig.env.site,
        None,
        "GET",
        "/api/v1/auth/session",
        &[],
        None,
    )
    .await;
    let text = String::from_utf8_lossy(&reply.bytes).into_owned();
    assert_eq!(reply.status, 503, "{text}");
    assert!(text.contains("server.unavailable"), "{text}");
    started.finish().await;
}

/// The head of the site's identity audit chain, and whether the chain verifies.
async fn chain_head(rig: &Rig) -> u64 {
    let root = &rig.env.root.0;
    let loaded = spot_cli::config::load(root, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == rig.env.site)
        .unwrap();
    let backend = spot_cli::schema::backend(&loaded, site).await.unwrap();
    let chain = spot_cli::audit::chain::IdentityChain::new(
        &backend,
        &spot_cli::audit::chain::scope_of(&rig.env.site),
    );
    assert!(
        chain.verify().await.unwrap().is_ok(),
        "the audit chain does not verify"
    );
    chain.head().await.unwrap()
}

fn invocation(site: &str) -> Invocation {
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

/// Writes one `vault_note` with its token sealed. Returns the id.
async fn write_note(started: &Started, site: &str, token: &str) -> Result<RecordId, String> {
    let live = started.running.live(site).unwrap();
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

async fn read_token(started: &Started, site: &str, id: RecordId) -> Option<String> {
    let live = started.running.live(site).unwrap();
    let engine = live.engine().unwrap();
    let access = engine.secret_access(SecretCaller::Seed, &invocation(site));
    engine
        .open_secret(&access, "vault_note", id, "token")
        .await
        .unwrap()
        .map(|s| s.expose().to_owned())
}

// --- tests -----------------------------------------------------------------------------------

#[tokio::test]
async fn a_sealed_value_written_over_the_engine_survives_a_restart() {
    let Some(rig) = Rig::new().await else { return };
    rig.init_key_file().await;
    let site = rig.env.site.clone();
    let token = "tok_live_8f3a9c1e5d7b2046-restart";

    let started = rig.start().await;
    let id = write_note(&started, &site, token).await.unwrap();
    assert_eq!(
        read_token(&started, &site, id).await.as_deref(),
        Some(token)
    );
    started.finish().await;

    // The value is not in the database in the clear.
    let loaded = spot_cli::config::load(&rig.env.root.0, false).unwrap();
    let site_cfg = loaded.sites.iter().find(|s| s.site == site).unwrap();
    let backend = spot_cli::schema::backend(&loaded, site_cfg).await.unwrap();
    let client = backend.pool().get().await.unwrap();
    let row = client
        .query_one("SELECT token__sealed FROM vault_note", &[])
        .await
        .unwrap();
    let sealed: Vec<u8> = row.get(0);
    assert!(
        !sealed.windows(token.len()).any(|w| w == token.as_bytes()),
        "the sealed column holds the plain value"
    );

    // A fresh process reads it back, and finds it by the blind index.
    let again = rig.start().await;
    assert_eq!(read_token(&again, &site, id).await.as_deref(), Some(token));
    let live = again.running.live(&site).unwrap();
    let engine = live.engine().unwrap();
    let access = engine.secret_access(SecretCaller::Seed, &invocation(&site));
    assert_eq!(
        engine
            .find_by_secret(&access, "vault_note", "token", token.as_bytes())
            .await
            .unwrap(),
        Some(id)
    );
    again.finish().await;
}

#[tokio::test]
async fn a_bad_key_file_refuses_to_start_and_names_the_file_not_the_key() {
    let Some(rig) = Rig::new().await else { return };
    rig.init_key_file().await;
    let keys = rig.key_texts();
    assert!(!keys.is_empty());
    let good = std::fs::read(&rig.kek).unwrap();
    let path = rig.kek.display().to_string();
    let mut messages = Vec::new();

    // A usable file starts, and the readiness check lists the required service.
    let started = rig.start().await;
    let reply = common::http(
        started.addr,
        &rig.env.site,
        None,
        "GET",
        "/api/health/ready",
        &[],
        None,
    )
    .await;
    messages.push(String::from_utf8_lossy(&reply.bytes).into_owned());
    started.finish().await;

    // Missing.
    std::fs::remove_file(&rig.kek).unwrap();
    let e = refusal(&rig).await;
    assert!(
        e.contains("secrets_host.kek_file_unusable") && e.contains(&path),
        "{e}"
    );
    messages.push(e);

    // Empty.
    std::fs::write(&rig.kek, b"").unwrap();
    set_mode(&rig.kek, 0o600);
    let e = refusal(&rig).await;
    assert!(e.contains("is empty") && e.contains(&path), "{e}");
    messages.push(e);

    // Not a key file.
    std::fs::write(&rig.kek, b"this is not json at all").unwrap();
    set_mode(&rig.kek, 0o600);
    let e = refusal(&rig).await;
    assert!(
        e.contains("secrets_host.kek_file_unusable") && e.contains(&path),
        "{e}"
    );
    messages.push(e);

    // A good file that group and others can read.
    std::fs::write(&rig.kek, &good).unwrap();
    set_mode(&rig.kek, 0o644);
    let e = refusal(&rig).await;
    assert!(
        e.contains("secrets_host.kek_file_unsafe") && e.contains(&path),
        "{e}"
    );
    messages.push(e);
    set_mode(&rig.kek, 0o600);

    // The table without a key file, and a relative one.
    for (services, code) in [
        ("[services.secrets]\n", "secrets_host.kek_unset"),
        (
            "[services.secrets]\nkek_file = 'keys/{site}.json'\n",
            "secrets_host.kek_path_refused",
        ),
        (
            "[services.secrets]\nkek_file = '/tmp/x.json'\nspare = 1\n",
            "secrets_host.settings_invalid",
        ),
    ] {
        rig.env.write_common(&["'sample'"], services);
        let e = refusal(&rig).await;
        assert!(e.contains(code), "{e}");
        messages.push(e);
    }
    // A key file inside the site folders.
    let inside = format!(
        "[services.secrets]\nkek_file = {:?}\n",
        format!("{}/sites/{{site}}/kek.json", rig.env.root.0.display())
    );
    rig.env.write_common(&["'sample'"], &inside);
    let e = refusal(&rig).await;
    assert!(e.contains("secrets_host.kek_path_refused"), "{e}");
    messages.push(e);

    // The restored configuration starts again: nothing was changed by the refusals.
    rig.env.write_common(
        &["'sample'"],
        &format!(
            "[services.secrets]\nkek_file = {:?}\n",
            format!("{}/{{site}}.json", rig.dir.display())
        ),
    );
    let again = rig.start().await;
    let reply = common::http(
        again.addr,
        &rig.env.site,
        None,
        "GET",
        "/api/v1/auth/session",
        &[],
        None,
    )
    .await;
    assert_ne!(reply.status, 503, "the good file serves again");
    again.finish().await;

    for text in &messages {
        for key in &keys {
            assert!(!text.contains(key), "a key leaked into: {text}");
        }
    }
}

#[tokio::test]
async fn a_host_without_the_table_starts_with_no_vault_and_a_secret_is_refused() {
    let Some(rig) = Rig::new().await else { return };
    rig.env.write_common(&["'sample'"], "");
    let started = rig.start().await;
    let err = write_note(&started, &rig.env.site, "tok_nothing")
        .await
        .unwrap_err();
    assert_eq!(err, "secretfield.not_configured");
    started.finish().await;
}

#[tokio::test]
async fn an_older_copy_of_the_key_file_is_refused_after_a_rotation() {
    let Some(rig) = Rig::new().await else { return };
    rig.init_key_file().await;
    let site = rig.env.site.clone();
    let started = rig.start().await;
    let id = write_note(&started, &site, "tok_older_copy").await.unwrap();
    started.finish().await;
    let old_copy = std::fs::read(&rig.kek).unwrap();

    let (exit, text) = secrets_cmd(
        rig.secrets_cmd_root(),
        &[
            "rotate-kek",
            "--site",
            &site,
            "--apply",
            "--settle-seconds",
            "0",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{text}");

    // The operator restores the file from before the rotation.
    let keys_now = rig.key_texts();
    std::fs::write(&rig.kek, &old_copy).unwrap();
    set_mode(&rig.kek, 0o600);
    let e = refusal(&rig).await;
    assert!(e.contains("secrets_host.key_version_missing"), "{e}");
    assert!(e.contains(&rig.kek.display().to_string()), "{e}");
    assert_site_unavailable(&rig).await;
    for key in keys_now.iter().chain(rig.key_texts().iter()) {
        assert!(!e.contains(key));
    }
    let _ = id;
}

#[tokio::test]
async fn rotation_with_a_writer_running_loses_no_write_and_ends_with_no_old_rows() {
    let Some(rig) = Rig::new().await else { return };
    rig.init_key_file().await;
    let site = rig.env.site.clone();
    let started = Arc::new(rig.start().await);

    // Rows sealed under version 1 before the rotation.
    let mut written: Vec<(RecordId, String)> = Vec::new();
    for i in 0..30 {
        let token = format!("tok_before_{i}_c0ffee");
        let id = write_note(&started, &site, &token).await.unwrap();
        written.push((id, token));
    }

    // The status before: version 1 only.
    let (exit, before) = secrets_cmd(rig.secrets_cmd_root(), &["status", "--site", &site]).await;
    assert_eq!(exit, Exit::Ok, "{before}");
    assert!(
        before.contains("vault_note.token: version 1: 30"),
        "{before}"
    );
    assert!(before.contains("current version: 1"), "{before}");

    // A writer that never stops while the key rotates.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (started, site, stop) = (started.clone(), site.clone(), stop.clone());
        tokio::spawn(async move {
            let mut done = Vec::new();
            let mut failures = Vec::new();
            let mut i = 0u32;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let token = format!("tok_during_{i}_decaf0");
                match write_note(&started, &site, &token).await {
                    Ok(id) => done.push((id, token)),
                    Err(code) => failures.push(code),
                }
                i += 1;
                tokio::time::sleep(std::time::Duration::from_millis(15)).await;
            }
            (done, failures)
        })
    };

    // The plan changes nothing.
    let (exit, plan) = secrets_cmd(rig.secrets_cmd_root(), &["rotate-kek", "--site", &site]).await;
    assert_eq!(exit, Exit::Ok, "{plan}");
    assert!(plan.contains("nothing changed"), "{plan}");
    assert!(rig.key_texts().len() == 1);

    let keys_before = rig.key_texts();
    let (exit, applied) = secrets_cmd(
        rig.secrets_cmd_root(),
        &["rotate-kek", "--site", &site, "--apply", "--batch", "7"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{applied}");
    assert!(applied.contains("added key version 2"), "{applied}");
    assert!(applied.contains("done:"), "{applied}");

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let (during, failures) = writer.await.unwrap();
    assert!(
        failures.is_empty(),
        "writes failed during the rotation: {failures:?}"
    );
    assert!(!during.is_empty(), "the writer never ran");
    written.extend(during);

    // Every value, old and new, opens; none is under version 1 any more.
    for (id, token) in &written {
        assert_eq!(
            read_token(&started, &site, *id).await.as_deref(),
            Some(token.as_str())
        );
    }
    let (exit, after) = secrets_cmd(rig.secrets_cmd_root(), &["status", "--site", &site]).await;
    assert_eq!(exit, Exit::Ok, "{after}");
    assert!(after.contains("current version: 2"), "{after}");
    assert!(
        after.contains("stored values under an old version: 0"),
        "{after}"
    );
    assert!(
        after.contains("versions with key material: 1, 2"),
        "{after}"
    );

    // A second plain rotation is refused only while a rotation is unfinished; this one is
    // finished, so the plan is accepted, and retiring version 1 is now allowed.
    let (exit, retire_plan) =
        secrets_cmd(rig.secrets_cmd_root(), &["retire-kek", "--site", &site]).await;
    assert_eq!(exit, Exit::Ok, "{retire_plan}");
    assert!(retire_plan.contains("plan: destroy"), "{retire_plan}");
    let (exit, retired) = secrets_cmd(
        rig.secrets_cmd_root(),
        &[
            "retire-kek",
            "--site",
            &site,
            "--apply",
            "--confirm-all-servers-reloaded",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{retired}");
    assert!(retired.contains("retired key version 1"), "{retired}");

    // The readiness of the host names the service and no key.
    let reply = common::http(
        started.addr,
        &site,
        None,
        "GET",
        "/api/health/ready",
        &[],
        None,
    )
    .await;
    let ready = String::from_utf8_lossy(&reply.bytes).into_owned();

    // No output of any command, and no readiness text, holds a key.
    for key in keys_before.iter().chain(rig.key_texts().iter()) {
        for text in [
            &before,
            &plan,
            &applied,
            &after,
            &retire_plan,
            &retired,
            &ready,
        ] {
            assert!(!text.contains(key), "a key leaked into: {text}");
        }
    }
    // Started, finished, retire started and retired: four records, and the chain verifies.
    assert!(chain_head(&rig).await >= 4, "the rotation was not audited");
    let started = Arc::try_unwrap(started).ok().expect("the writer let go");
    started.finish().await;
}

#[tokio::test]
async fn retire_apply_without_the_confirmation_flag_is_refused_and_changes_nothing() {
    let Some(rig) = Rig::new().await else { return };
    rig.init_key_file().await;
    let site = rig.env.site.clone();
    let (exit, text) = secrets_cmd(
        rig.secrets_cmd_root(),
        &[
            "rotate-kek",
            "--site",
            &site,
            "--apply",
            "--settle-seconds",
            "0",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{text}");
    assert_eq!(rig.key_texts().len(), 2);

    let keys_before = rig.key_texts();
    let marker = rig.env.root.0.join("sites").join(&site).join("reload");
    let marker_before = std::fs::read(&marker).ok();

    // The plan says the flag will be required, and changes nothing.
    let (exit, plan) = secrets_cmd(rig.secrets_cmd_root(), &["retire-kek", "--site", &site]).await;
    assert_eq!(exit, Exit::Ok, "{plan}");
    assert!(plan.contains("--confirm-all-servers-reloaded"), "{plan}");
    assert_eq!(rig.key_texts(), keys_before);

    let head = chain_head(&rig).await;
    let (exit, text) = secrets_cmd(
        rig.secrets_cmd_root(),
        &["retire-kek", "--site", &site, "--apply"],
    )
    .await;
    assert_eq!(exit, Exit::Refused, "{text}");
    assert!(text.contains("secrets_host.retire_unconfirmed"), "{text}");
    assert!(!text.contains("retired key version"), "{text}");
    assert_eq!(rig.key_texts(), keys_before, "the key file changed");
    assert_eq!(
        std::fs::read(&marker).ok(),
        marker_before,
        "the reload marker moved"
    );
    for key in keys_before.iter() {
        assert!(!text.contains(key), "a key leaked into: {text}");
    }
    assert_eq!(
        chain_head(&rig).await,
        head + 1,
        "the refusal was not audited"
    );
}

#[tokio::test]
async fn a_rotation_that_cannot_take_the_lock_is_refused_and_audited() {
    let Some(rig) = Rig::new().await else { return };
    rig.init_key_file().await;
    let site = rig.env.site.clone();
    let before = chain_head(&rig).await;
    let lock = svc::RotationLock::take(&rig.kek).unwrap();
    let (exit, text) = secrets_cmd(
        rig.secrets_cmd_root(),
        &[
            "rotate-kek",
            "--site",
            &site,
            "--apply",
            "--settle-seconds",
            "0",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Refused, "{text}");
    assert!(text.contains("secrets_host.rotation_running"), "{text}");
    assert_eq!(
        rig.key_texts().len(),
        1,
        "a refused rotation added a version"
    );
    assert_eq!(
        chain_head(&rig).await,
        before + 1,
        "the refusal was not audited"
    );
    drop(lock);
    let (exit, text) = secrets_cmd(
        rig.secrets_cmd_root(),
        &[
            "rotate-kek",
            "--site",
            &site,
            "--apply",
            "--settle-seconds",
            "0",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{text}");
    assert_eq!(rig.key_texts().len(), 2);
}

#[tokio::test]
async fn rotate_refuses_while_values_are_under_an_older_version_and_resume_finishes_it() {
    let Some(rig) = Rig::new().await else { return };
    rig.init_key_file().await;
    let site = rig.env.site.clone();
    let started = rig.start().await;
    let mut ids = Vec::new();
    for i in 0..5 {
        let token = format!("tok_resume_{i}");
        ids.push((write_note(&started, &site, &token).await.unwrap(), token));
    }
    started.finish().await;

    // Add a version by hand, as an interrupted rotation would have, without moving anything.
    let loaded = spot_cli::config::load(&rig.env.root.0, false).unwrap();
    let site_cfg = loaded.sites.iter().find(|s| s.site == site).unwrap();
    let host = svc::open(&rig.env.root.0, &loaded, site_cfg)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(svc::add_version(&host).await.unwrap(), 2);

    let (exit, text) = secrets_cmd(
        rig.secrets_cmd_root(),
        &[
            "rotate-kek",
            "--site",
            &site,
            "--apply",
            "--settle-seconds",
            "0",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Refused, "{text}");
    assert!(text.contains("--resume"), "{text}");
    assert_eq!(rig.key_texts().len(), 2, "a refused run added no version");

    // Retiring version 1 is refused while rows are under it.
    let (exit, text) = secrets_cmd(
        rig.secrets_cmd_root(),
        &[
            "retire-kek",
            "--site",
            &site,
            "--apply",
            "--confirm-all-servers-reloaded",
        ],
    )
    .await;
    assert_eq!(exit, Exit::Refused, "{text}");
    assert!(text.contains("secrets_host.retire_refused"), "{text}");

    let (exit, text) = secrets_cmd(
        rig.secrets_cmd_root(),
        &["rotate-kek", "--site", &site, "--resume", "--apply"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{text}");
    assert_eq!(rig.key_texts().len(), 2, "resume added no version");
    let (_, status) = secrets_cmd(rig.secrets_cmd_root(), &["status", "--site", &site]).await;
    assert!(
        status.contains("stored values under an old version: 0"),
        "{status}"
    );

    let started = rig.start().await;
    for (id, token) in &ids {
        assert_eq!(
            read_token(&started, &site, *id).await.as_deref(),
            Some(token.as_str())
        );
    }
    started.finish().await;
}

#[tokio::test]
async fn init_creates_a_private_key_file_once() {
    let Some(rig) = Rig::new().await else { return };
    let site = rig.env.site.clone();
    let (exit, plan) = secrets_cmd(rig.secrets_cmd_root(), &["init", "--site", &site]).await;
    assert_eq!(exit, Exit::Ok, "{plan}");
    assert!(!rig.kek.exists(), "the plan created the file");
    let (exit, done) = secrets_cmd(
        rig.secrets_cmd_root(),
        &["init", "--site", &site, "--apply"],
    )
    .await;
    assert_eq!(exit, Exit::Ok, "{done}");
    assert!(rig.kek.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&rig.kek).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
    let (exit, again) = secrets_cmd(
        rig.secrets_cmd_root(),
        &["init", "--site", &site, "--apply"],
    )
    .await;
    assert_eq!(exit, Exit::Refused, "{again}");
    for key in rig.key_texts() {
        assert!(!done.contains(&key) && !again.contains(&key));
    }
}

#[tokio::test]
async fn the_command_refuses_a_site_with_no_secrets_table_and_an_unknown_site() {
    let Some(rig) = Rig::new().await else { return };
    let site = rig.env.site.clone();
    let (exit, text) = secrets_cmd(
        rig.secrets_cmd_root(),
        &["status", "--site", "no-such-site"],
    )
    .await;
    assert_eq!(exit, Exit::Usage, "{text}");
    rig.env.write_common(&["'sample'"], "");
    let (exit, text) = secrets_cmd(rig.secrets_cmd_root(), &["status", "--site", &site]).await;
    assert_eq!(exit, Exit::Refused, "{text}");
    assert!(text.contains("[services.secrets]"), "{text}");
}

fn set_mode(path: &Path, mode: u32) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
}
