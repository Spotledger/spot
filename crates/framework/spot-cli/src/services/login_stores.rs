//! The sign-in stores: sessions, throttle counters and credentials in the site database
//! (WP-PLT-02.T8).
//!
//! Before this module the server signed people in through an in-memory session store, a bounded
//! per-process throttle and password verifiers kept in `site.toml` (its own doc: "a restart signs
//! everyone out and forgets every lockout", a recorded security finding). The storage-backed stores of
//! `spot-login` (WP-PLT-02.T4, with the gaps closed by WP-PLT-02.T10) hold all of it in records
//! and seal every credential through the secrets service. This module hosts them for a site:
//!
//! - it builds the site's [`SiteStorage`] over the running engine and provides it as a port, so
//!   the host's sign-in assembly (`crate::login::SiteAuth::new`) reads it with
//!   `ctx.port::<SiteStorage>()`;
//! - it **imports the staged credentials once** (at every start and reload, idempotently): each
//!   chosen-password verifier and API key digest an operator command staged in `site.toml`
//!   (`spot set-password`, `spot create-user`) is written into the sealed fields, one write per
//!   user, and then removed from the file, which is replaced atomically with mode 0600. A crash
//!   between the write and the rewrite is safe: the next start writes the same values again. A
//!   record that cannot be imported refuses the start and names the user id (never the secret);
//! - it checks at start that the stores and the vault answer.
//!
//! # What stays in the file
//!
//! One-time first sign-in secrets (`first_sign_in = true`): they are short-lived, hold only an
//! Argon2id verifier of a secret nobody chose, and end with the user's first chosen password
//! (`crate::login::Credentials`). Nothing else with a verifier or a digest remains in the file
//! after an import.
//!
//! # Level
//!
//! **Required.** Without the stores nobody can sign in, so a site that cannot import, or whose
//! stores do not answer, refuses to start (`register` fails) or is reported stopped (the check
//! fails). Settings: `[services.login_stores]` (`import_site_file`, default `true`; unknown keys
//! are refused).

use std::path::Path;
use std::sync::Arc;

use serde::Deserialize;
use spot_app_api::Document;
use spot_app_api::context::{Actor, Invocation};
use spot_auth::PasswordHash;
use spot_core::Clock;
use spot_document::engine::Engine;
use spot_document::secrets::SecretCaller;
use spot_login::{KeyStore, UserDirectory};
use spot_platform_app::doctype;
use spot_postgres::PgBackend;
use spot_secrets::SecretString;
use spot_storage_contract::{Name, RecordId, Value};

use crate::config::{Credential, Key, Site};
use crate::login::SiteStorage;
use crate::services::{CheckFuture, Idle, Level, SiteCtx};
use crate::{Error, Result, sitefile};

/// Start order: after the secrets service (1000), before the modules that authenticate.
pub const ORDER: u32 = 1005;

/// `[services.login_stores]`. Unknown keys are refused.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether staged credentials in `site.toml` are imported at start and reload.
    pub import_site_file: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            import_site_file: true,
        }
    }
}

/// What one import did (counts only: never a name, a digest or a verifier).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Imported {
    /// Chosen-password verifiers written to the sealed `user.password_hash`.
    pub passwords: usize,
    /// API key digests written to `api_key` records (an existing key id is skipped).
    pub keys: usize,
}

/// The staged records of a site file: what an import moves.
pub fn staged(site: &Site) -> (Vec<&Credential>, Vec<&Key>) {
    (
        site.credentials
            .iter()
            .filter(|c| !c.first_sign_in)
            .collect(),
        site.api_keys.iter().collect(),
    )
}

fn refuse(user: &str, why: &str) -> Error {
    // The user id is a record id, not a secret; the verifier and the digest are never named.
    Error(format!(
        "the site file holds a credential for user {user} that cannot be imported: {why}"
    ))
}

fn import_invocation(site: &str, clock: &dyn Clock) -> Result<Invocation> {
    Ok(Invocation {
        site: spot_core::SiteId::new(site).map_err(crate::error)?,
        actor: Actor::System {
            component: Name::new("login_import").map_err(crate::error)?,
        },
        command_id: None,
        language: None,
        now: clock.now_utc(),
    })
}

/// Imports the staged credentials of `site_name` into the sealed fields and removes them from
/// the file. Idempotent; safe to interrupt.
///
/// # Errors
///
/// An error naming the user id of the first record that cannot be imported (a user that no
/// longer exists, a malformed verifier or digest); nothing is removed from the file then.
pub async fn import_site_file(
    root: &Path,
    site_name: &str,
    storage: &SiteStorage,
    engine: &Engine<PgBackend>,
    clock: &Arc<dyn Clock>,
) -> Result<Imported> {
    let site = sitefile::read(root, site_name)?;
    let (passwords, keys) = staged(&site);
    if passwords.is_empty() && keys.is_empty() {
        return Ok(Imported::default());
    }
    let directory = storage.stores.user_directory();
    let key_store = storage.stores.key_store();
    let mut done_users: Vec<String> = Vec::new();
    let mut done_keys: Vec<String> = Vec::new();
    let mut imported = Imported::default();

    for credential in passwords {
        let id = spot_http::api::codec::parse_record_id(&credential.user)
            .ok_or_else(|| refuse(&credential.user, "the user id is not a record id"))?;
        let hash = PasswordHash::parse(&credential.password_hash)
            .map_err(|_| refuse(&credential.user, "the verifier is malformed"))?;
        // One sealed write per user. The staged value wins over what is stored: an operator
        // who ran `spot set-password` meant to replace it.
        directory
            .set_password_hash(id, hash)
            .await
            .map_err(|e| refuse(&credential.user, e.code()))?;
        done_users.push(credential.user.clone());
        imported.passwords += 1;
    }

    let invocation = import_invocation(site_name, clock.as_ref())?;
    let access = engine.secret_access(SecretCaller::LoginService, &invocation);
    for key in keys {
        let owner = spot_http::api::codec::parse_record_id(&key.user)
            .ok_or_else(|| refuse(&key.user, "the key owner is not a record id"))?;
        crate::auth::stored(key).map_err(|_| refuse(&key.user, "the key verifier is malformed"))?;
        let exists = key_store
            .find_by_key_id(&key.key_id)
            .await
            .map_err(|e| refuse(&key.user, e.code()))?
            .is_some();
        if !exists {
            create_key(engine, &access, key, owner)
                .await
                .map_err(|why| refuse(&key.user, &why))?;
            imported.keys += 1;
        }
        done_keys.push(key.key_id.clone());
    }

    // Only now does the file lose them: the rewrite is atomic (0600), and what it removes is
    // exactly what was written above.
    sitefile::update(root, site_name, |file| {
        file.credentials
            .retain(|c| c.first_sign_in || !done_users.contains(&c.user));
        file.api_keys.retain(|k| !done_keys.contains(&k.key_id));
        Ok(())
    })?;
    Ok(imported)
}

/// Creates the `api_key` record of one staged key. The reason is a stable code, never a value.
async fn create_key(
    engine: &Engine<PgBackend>,
    access: &spot_document::secrets::SecretAccess,
    key: &Key,
    owner: RecordId,
) -> std::result::Result<(), String> {
    let name = |text: &str| Name::new(text).map_err(|_| "invalid field name".to_owned());
    let mut doc = Document::new(name(doctype::API_KEY)?, RecordId::new_v7());
    doc.set(name("key_id")?, Value::Text(key.key_id.clone()));
    doc.set(name("owner")?, Value::Id(owner));
    doc.set(name("state")?, Value::Text("active".to_owned()));
    doc.set(
        name("label")?,
        Value::Text("imported from site.toml".to_owned()),
    );
    if let Some(at) = key.expires_at {
        let instant = time::OffsetDateTime::from_unix_timestamp(at)
            .map_err(|_| "the key expiry is not an instant".to_owned())?;
        doc.set(name("expires_at")?, Value::DateTime(instant));
    }
    engine
        .create_with_secrets(
            access,
            doc,
            vec![("digest".to_owned(), SecretString::new(key.digest.clone()))],
        )
        .await
        .map(|_| ())
        .map_err(|e| e.code.clone())
}

/// Joins the site: builds [`SiteStorage`], imports the staged credentials, provides the storage
/// as a port and adds the required check.
///
/// # Errors
///
/// A setting that does not decode; the engine not running; a staged record that cannot be
/// imported (naming the user id).
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings: Settings = ctx.settings_or_default("login_stores")?;
    let (root, site, live, catalog) = {
        let env = ctx.env()?;
        (
            env.root.to_path_buf(),
            env.site.site.clone(),
            Arc::clone(env.live),
            env.permission.catalog.clone(),
        )
    };
    let engine = live.engine().ok_or_else(|| {
        Error("the login stores need the site's engine, which is not running yet".into())
    })?;
    let clock = Arc::clone(ctx.clock());
    let storage = Arc::new(SiteStorage::new(
        Arc::clone(&engine),
        catalog,
        &site,
        Arc::clone(&clock),
    )?);
    if settings.import_site_file {
        let imported = import_site_file(&root, &site, &storage, &engine, &clock).await?;
        if imported != Imported::default() {
            tracing::info!(
                site = %site,
                passwords = imported.passwords,
                keys = imported.keys,
                "staged credentials were imported into the site database and removed from site.toml"
            );
        }
    }
    if ctx.assembling() {
        let probe = Arc::clone(&storage);
        ctx.add(Idle::new(
            "login_stores",
            Level::Required,
            move || -> CheckFuture<'static> {
                let probe = Arc::clone(&probe);
                Box::pin(async move {
                    // A read through the whole path: the query, and the vault for a user's
                    // credentials. No such user exists, so nothing is opened.
                    probe
                        .stores
                        .user_directory()
                        .find_by_identifier("spot-login-stores-probe")
                        .await
                        .map(|_| ())
                        .map_err(|e| {
                            format!(
                                "the sign-in stores do not answer ({}); nobody can sign in",
                                e.code()
                            )
                        })
                })
            },
        ));
    }
    ctx.provide(storage)?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn site() -> Site {
        let mut site = Site::named("acme.test", "acme", "und", Vec::new());
        site.credentials = vec![
            Credential {
                user: "u1".into(),
                password_hash: "x".into(),
                first_sign_in: false,
                expires_at: None,
            },
            Credential {
                user: "u2".into(),
                password_hash: "y".into(),
                first_sign_in: true,
                expires_at: Some(1),
            },
        ];
        site.api_keys = vec![Key {
            key_id: "abcdefghijkl".into(),
            digest: "00".repeat(32),
            user: "u1".into(),
            expires_at: None,
        }];
        site
    }

    #[test]
    fn a_one_time_secret_is_never_staged_for_import() {
        let site = site();
        let (passwords, keys) = staged(&site);
        assert_eq!(passwords.len(), 1);
        assert_eq!(passwords[0].user, "u1");
        assert_eq!(keys.len(), 1);
    }

    #[test]
    fn a_refusal_names_the_user_and_not_the_verifier() {
        let text = refuse("u1", "the verifier is malformed").to_string();
        assert!(text.contains("u1"));
        assert!(!text.contains("argon2"));
    }

    #[test]
    fn the_setting_refuses_unknown_keys() {
        let ok: std::result::Result<Settings, _> = toml::from_str("import_site_file = false");
        assert!(!ok.unwrap().import_site_file);
        let bad: std::result::Result<Settings, _> = toml::from_str("import_site = true");
        assert!(bad.is_err());
        assert!(Settings::default().import_site_file);
    }
}
