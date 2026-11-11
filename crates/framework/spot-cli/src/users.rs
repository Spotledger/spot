//! Operator user commands. They run as the system principal directly against storage, with the
//! same stores the server uses, and never print a secret more than once.
//!
//! Credential material is kept in `site.toml`: the platform's `user.password_hash` field has no
//! storage column until the secrets service lands (WP-SEC-10).
use crate::config::{self, Credential, Key, Site};
use crate::onetime::Issue;
use crate::{Error, Result, apps, auth, error, schema, sitefile};
use spot_auth::{ApiKey, HashParams, Password, PasswordPolicy, hash_password};
use spot_platform_app::bootstrap::{SeedParams, seed};
use spot_platform_app::{doctype, role};
use spot_postgres::PgBackend;
use spot_storage_contract::{
    Catalog, DocumentData, DocumentStore, Expr, Isolation, Name, Query, QueryExecutor, QueryPath,
    RecordId, Selection, StorageBackend, Transaction, TxOptions, Value,
};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

fn name(text: &str) -> Result<Name> {
    Name::new(text).map_err(error)
}

fn options() -> Result<TxOptions> {
    Ok(TxOptions::new(name("spot_cli")?, Isolation::ReadCommitted))
}

/// Every stored field of a platform DocType set to `Null`, as storage demands full writes.
fn blank(doctype: &str) -> Result<BTreeMap<Name, Value>> {
    let definitions = spot_platform_app::definitions().map_err(error)?;
    let def = definitions
        .iter()
        .find(|d| d.name == doctype)
        .ok_or_else(|| Error(format!("no platform DocType `{doctype}`")))?;
    let mut fields = BTreeMap::new();
    for f in &def.fields {
        if f.ty.is_table() {
            continue;
        }
        if f.ty == spot_meta::FieldType::Secret {
            // A secret has no column of its own, but its engine-managed sealed (and blind
            // index) columns are declared fields, so a full write names them (WP-SEC-03.T3).
            fields.insert(name(&spot_meta::sealed_column(&f.name))?, Value::Null);
            if f.lookup_index() {
                fields.insert(name(&spot_meta::blind_index_column(&f.name))?, Value::Null);
            }
            continue;
        }
        fields.insert(name(&f.name)?, Value::Null);
    }
    Ok(fields)
}

fn set(fields: &mut BTreeMap<Name, Value>, field: &str, value: impl Into<Value>) -> Result<()> {
    let key = name(field)?;
    if !fields.contains_key(&key) {
        return Err(Error(format!("no stored field `{field}`")));
    }
    fields.insert(key, value.into());
    Ok(())
}

fn document(entity: &str, id: RecordId, fields: BTreeMap<Name, Value>) -> Result<DocumentData> {
    Ok(DocumentData {
        entity: name(entity)?,
        id,
        fields,
        children: BTreeMap::new(),
    })
}

/// Writes the Administrator, the system roles and the Administrator's role assignment.
/// Returns the Administrator's id.
pub async fn seed_platform(
    backend: &PgBackend,
    meta: &spot_meta::EffectiveMeta,
) -> Result<RecordId> {
    let catalog = Arc::new(schema::catalog(meta)?);
    let mut tx = backend.begin(catalog, options()?).await.map_err(error)?;
    let outcome = match seed(&mut tx, &SeedParams::default()).await {
        Ok(outcome) => outcome,
        Err(e) => {
            let _ = tx.rollback().await;
            return Err(error(e));
        }
    };
    tx.commit().await.map_err(error)?;
    Ok(outcome.administrator)
}

/// The id of the document of `entity` whose `field` equals `value`.
async fn find(
    tx: &mut impl QueryExecutor,
    entity: &str,
    field: &str,
    value: &str,
) -> Result<Option<RecordId>> {
    let alias = name("t")?;
    let mut query = Query::from(&name(entity)?, &alias, QueryPath::Report);
    query.select = vec![Selection {
        expr: Expr::field(&alias, &name("id")?),
        alias: name("v")?,
    }];
    query.filter = Some(Expr::Cmp(
        Box::new(Expr::field(&alias, &name(field)?)),
        spot_storage_contract::CmpOp::Eq,
        Box::new(Expr::Value(Value::Text(value.to_owned()))),
    ));
    query.limit = Some(1);
    let rows = tx.query(&query).await.map_err(error)?;
    Ok(rows.rows.into_iter().flatten().find_map(|v| {
        if let Value::Id(id) = v {
            Some(id)
        } else {
            None
        }
    }))
}

/// The Administrator's record id on a site that has the platform app.
pub async fn administrator(backend: &PgBackend, catalog: Arc<Catalog>) -> Result<RecordId> {
    let mut tx = backend.begin(catalog, options()?).await.map_err(error)?;
    let found = find(&mut tx, doctype::USER, "user_name", "administrator").await;
    let _ = tx.rollback().await;
    found?.ok_or_else(|| Error("the Administrator record does not exist on this site".into()))
}

/// Everything a user command needs: the site, its backend and the catalogue.
struct Target {
    site: Site,
    backend: PgBackend,
    catalog: Arc<Catalog>,
}

async fn target(root: &Path, site: &str) -> Result<Target> {
    let loaded = config::load(root, false)?;
    let site = loaded
        .sites
        .iter()
        .position(|s| s.site == site)
        .map(|i| &loaded.sites[i])
        .ok_or_else(|| Error(format!("no site `{site}`")))?;
    if site.apps.first().map(String::as_str) != Some(crate::appdir::PLATFORM) {
        return Err(Error(format!(
            "site `{}` has no platform app, so it has no users; create sites for user \
             management with `spot new-site --bare`",
            site.site
        )));
    }
    let meta = apps::serving(root, &loaded, site)?;
    let backend = schema::backend(&loaded, site).await?;
    let catalog = Arc::new(schema::catalog(&meta)?);
    let site = sitefile::read(root, &site.site)?;
    Ok(Target {
        site,
        backend,
        catalog,
    })
}

fn email_ok(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && email.len() <= 254
        && !email
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
        && email.matches('@').count() == 1
}

fn new_key(user: RecordId) -> (ApiKey, Key) {
    let (key, stored) = ApiKey::generate();
    (
        key,
        Key {
            key_id: stored.key_id,
            digest: auth::encode_digest(stored.digest.as_bytes()),
            user: user.to_string(),
            expires_at: None,
        },
    )
}

/// A fresh one-time first sign-in secret for `user`. The text is the sign-in password and is
/// stored only as an Argon2id verifier marked `first_sign_in`, valid until `expires_at` (Unix
/// seconds, from the caller's clock). It is **not** an API key: sent as a bearer credential it
/// is refused, and a session started with it can only change the password (the gate).
pub fn issue_first_sign_in(user: RecordId, expires_at: i64) -> (ApiKey, Credential) {
    let (secret, _) = ApiKey::generate();
    let hash = hash_password(&Password::new(&secret.text()), &HashParams::default());
    (
        secret,
        Credential {
            user: user.to_string(),
            password_hash: hash.phc_string().to_owned(),
            first_sign_in: true,
            expires_at: Some(expires_at),
        },
    )
}

/// A separately generated API key for scripted bootstrap, valid until `expires_at`. Only its
/// digest is kept; the caller writes the secret to a protected file and shows it nowhere else.
pub fn issue_bootstrap_key(user: RecordId, expires_at: i64) -> (ApiKey, Key) {
    let (secret, mut key) = new_key(user);
    key.expires_at = Some(expires_at);
    (secret, key)
}

/// Marks the user `active` in storage, as accepting an invitation does. Runs as the system
/// principal through the storage document API (no lifecycle hooks needed for a state flag).
async fn activate(t: &Target, user: RecordId) -> Result<()> {
    let mut tx = t
        .backend
        .begin(t.catalog.clone(), options()?)
        .await
        .map_err(error)?;
    let entity = name(doctype::USER)?;
    let loaded = match tx
        .load(&entity, user, spot_storage_contract::RowLock::None)
        .await
    {
        Ok(Some(doc)) => doc,
        Ok(None) => {
            let _ = tx.rollback().await;
            return Err(Error("the user record no longer exists".into()));
        }
        Err(e) => {
            let _ = tx.rollback().await;
            return Err(error(e));
        }
    };
    let mut data = loaded.data;
    data.fields
        .insert(name("state")?, Value::Text("active".into()));
    if let Err(e) = tx.update(&data, loaded.version).await {
        let _ = tx.rollback().await;
        return Err(error(e));
    }
    tx.commit().await.map_err(error)
}

/// What `add-user` returns: the new user and its one-time first sign-in secret.
pub struct Invitation {
    /// The user's record id.
    pub user: RecordId,
    /// The first sign-in secret. Shown once by the caller; only its digest is stored.
    pub secret: ApiKey,
}

/// Creates an invited user with optional roles and a one-time first sign-in secret (a
/// password-change-only credential that expires; it is not an API key).
pub async fn add_user(
    root: &Path,
    site: &str,
    email: &str,
    full_name: Option<&str>,
    roles: &[String],
) -> Result<Invitation> {
    add_user_with(root, site, email, full_name, roles, &Issue::system()).await
}

/// Like [`add_user`], with the issue instant and the secret's lifetime given.
pub async fn add_user_with(
    root: &Path,
    site: &str,
    email: &str,
    full_name: Option<&str>,
    roles: &[String],
    issue: &Issue,
) -> Result<Invitation> {
    let email = email.trim().to_lowercase();
    if !email_ok(&email) {
        return Err(Error("the e-mail address is not valid".into()));
    }
    let t = target(root, site).await?;
    let mut tx = t
        .backend
        .begin(t.catalog.clone(), options()?)
        .await
        .map_err(error)?;
    let created = write_user(&mut tx, &email, full_name, roles).await;
    let user = match created {
        Ok(user) => user,
        Err(e) => {
            let _ = tx.rollback().await;
            return Err(e);
        }
    };
    if let Err(e) = audit_event(root, &t.site.site, "add-user", user) {
        let _ = tx.rollback().await;
        return Err(e);
    }
    let (secret, credential) = issue_first_sign_in(user, issue.secret_expires_at());
    if let Err(e) = sitefile::update(root, &t.site.site, |s| {
        s.credentials.push(credential);
        Ok(())
    }) {
        let _ = tx.rollback().await;
        return Err(e);
    }
    tx.commit().await.map_err(error)?;
    refresh_running_host(root, site);
    Ok(Invitation { user, secret })
}

async fn write_user<T: Transaction>(
    tx: &mut T,
    email: &str,
    full_name: Option<&str>,
    roles: &[String],
) -> Result<RecordId> {
    if find(tx, doctype::USER, "user_name", email).await?.is_some() {
        return Err(Error(format!("user `{email}` already exists")));
    }
    let mut role_ids = Vec::new();
    let mut unknown = Vec::new();
    for wanted in roles {
        match find(tx, doctype::ROLE, "role_name", wanted).await? {
            Some(id) => role_ids.push(id),
            None => unknown.push(wanted.as_str()),
        }
    }
    if !unknown.is_empty() {
        return Err(Error(format!("unknown role(s): {}", unknown.join(", "))));
    }
    let id = RecordId::new_v7();
    let local = email.split('@').next().unwrap_or(email);
    let mut f = blank(doctype::USER)?;
    set(&mut f, "user_name", email)?;
    set(&mut f, "email", email)?;
    set(&mut f, "full_name", full_name.unwrap_or(local))?;
    set(&mut f, "user_type", "internal")?;
    set(&mut f, "state", "invited")?;
    set(&mut f, "language", "en")?;
    set(&mut f, "time_zone", "UTC")?;
    set(&mut f, "failed_attempts", 0i64)?;
    tx.insert(&document(doctype::USER, id, f)?)
        .await
        .map_err(error)?;
    for role in role_ids {
        let mut f = blank(doctype::ROLE_ASSIGNMENT)?;
        set(&mut f, "user", id)?;
        set(&mut f, "role", role)?;
        set(&mut f, "source", "manual")?;
        set(&mut f, "state", "active")?;
        set(&mut f, "reason", "Assigned by the site operator.")?;
        tx.insert(&document(doctype::ROLE_ASSIGNMENT, RecordId::new_v7(), f)?)
            .await
            .map_err(error)?;
    }
    Ok(id)
}

/// Reads a new password from the terminal with echo off (asked twice), or one line of stdin.
pub fn read_password(from_stdin: bool) -> Result<Password> {
    if from_stdin {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Err(Error("no password was provided on standard input".into()));
        }
        return Ok(Password::new(line));
    }
    let first = prompt("New password: ")?;
    let second = prompt("Repeat the password: ")?;
    if first != second {
        return Err(Error("the two passwords differ".into()));
    }
    Ok(Password::new(&first))
}

/// Echo off through `stty` on the controlling terminal; platforms without it must use stdin.
fn prompt(label: &str) -> Result<String> {
    #[cfg(unix)]
    {
        use std::io::{BufRead, BufReader};
        let tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|_| Error("no terminal; use --password-stdin".into()))?;
        let stty = |args: &[&str]| -> bool {
            std::process::Command::new("stty")
                .args(args)
                .stdin(
                    tty.try_clone()
                        .map_or(std::process::Stdio::null(), Into::into),
                )
                .status()
                .is_ok_and(|s| s.success())
        };
        let mut out = tty.try_clone()?;
        write!(out, "{label}")?;
        out.flush()?;
        if !stty(&["-echo"]) {
            return Err(Error(
                "cannot turn terminal echo off; use --password-stdin".into(),
            ));
        }
        let mut line = String::new();
        let read = BufReader::new(tty.try_clone()?).read_line(&mut line);
        stty(&["echo"]);
        writeln!(out)?;
        read?;
        Ok(line.trim_end_matches(['\r', '\n']).to_owned())
    }
    #[cfg(not(unix))]
    {
        let _ = label;
        Err(Error(
            "no echo-free terminal input here; use --password-stdin".into(),
        ))
    }
}

/// Checks the policy and stores an Argon2id verifier for `user` (a user name or e-mail).
/// Returns the violated rules as the error text when the policy refuses.
pub async fn set_password(root: &Path, site: &str, user: &str, password: &Password) -> Result<()> {
    let t = target(root, site).await?;
    let identifier = user.trim().to_lowercase();
    let mut tx = t
        .backend
        .begin(t.catalog.clone(), options()?)
        .await
        .map_err(error)?;
    let found = match find(&mut tx, doctype::USER, "user_name", &identifier).await {
        Ok(None) => find(&mut tx, doctype::USER, "email", &identifier).await,
        other => other,
    };
    let _ = tx.rollback().await;
    let Some(id) = found? else {
        return Err(Error(format!("no user `{identifier}`")));
    };
    let local = identifier.split('@').next().unwrap_or(&identifier);
    if let Err(violations) =
        PasswordPolicy::default().check(password, &[&identifier, local, &t.site.site])
    {
        return Err(Error(format!(
            "the password does not satisfy the policy:\n{}",
            violations
                .iter()
                .map(|v| format!("  {}: {}", v.code, v.message))
                .collect::<Vec<_>>()
                .join("\n")
        )));
    }
    audit_event(root, &t.site.site, "set-password", id)?;
    let hash = hash_password(password, &HashParams::default());
    let phc = hash.phc_string().to_owned();
    sitefile::update(root, &t.site.site, |s| {
        s.credentials.retain(|c| c.user != id.to_string());
        s.credentials.push(Credential {
            user: id.to_string(),
            password_hash: phc,
            first_sign_in: false,
            expires_at: None,
        });
        Ok(())
    })?;
    activate(&t, id).await?;
    refresh_running_host(root, site);
    Ok(())
}

/// Asks a running host to reload the site, so a changed or revoked credential takes effect at
/// once instead of at the next restart. Harmless when no host runs (the marker is only read by a
/// host that is serving).
fn refresh_running_host(root: &Path, site: &str) {
    let _ = crate::request_reload(root, site);
}

/// Replaces every credential of the Administrator with one new one-time secret, and records
/// the reset in the site's audit file. The caller has already checked the operator flag.
pub async fn reset_admin(root: &Path, site: &str) -> Result<ApiKey> {
    reset_admin_with(root, site, &Issue::system()).await
}

/// Like [`reset_admin`], with the issue instant and the secret's lifetime given.
pub async fn reset_admin_with(root: &Path, site: &str, issue: &Issue) -> Result<ApiKey> {
    reset_admin_stages(root, site, issue)
        .await
        .map_err(|(_, error)| error)
}

/// The reason codes of a refused or failed `reset-admin`, as the site audit file records them.
pub const RESET_REFUSED_CONFIRMATION: &str = "operator_confirmation_missing";
/// The site is absent.
pub const RESET_REFUSED_SITE: &str = "site_absent";
/// The site has no Administrator record.
pub const RESET_REFUSED_ADMINISTRATOR: &str = "administrator_missing";
/// The credential store (the site's database or its site file) refused the change.
pub const RESET_REFUSED_STORE: &str = "credential_store_refused";

/// Resets the Administrator like [`reset_admin_with`] and writes every refusal or failure to the
/// site audit file with `outcome = "refused"` and a reason code: the operator
/// confirmation missing, the site absent, the credential store refusing. `confirmed` is the
/// operator flag the command line checked. A refusal record that cannot be written is a warning;
/// the refusal stands. The reset itself still records its own success line.
pub async fn reset_admin_checked(
    root: &Path,
    site: &str,
    issue: &Issue,
    confirmed: bool,
) -> Result<ApiKey> {
    let failure = if confirmed {
        match reset_admin_stages(root, site, issue).await {
            Ok(secret) => return Ok(secret),
            Err(failure) => failure,
        }
    } else {
        (
            RESET_REFUSED_CONFIRMATION,
            Error(
                "reset-admin replaces the Administrator's credentials; confirm that you are \
                 the site operator with --yes-i-am-the-operator"
                    .into(),
            ),
        )
    };
    let (reason, error) = failure;
    let body = serde_json::json!({
        "action": "reset-admin",
        "outcome": "refused",
        "reason": reason,
        "by": "operator",
    });
    if audit(root, site, &body.to_string()).is_err() {
        eprintln!("warning: the refusal could not be recorded in the site audit file");
    }
    Err(error)
}

async fn reset_admin_stages(
    root: &Path,
    site: &str,
    issue: &Issue,
) -> std::result::Result<ApiKey, (&'static str, Error)> {
    sitefile::read(root, site).map_err(|e| (RESET_REFUSED_SITE, e))?;
    let t = target(root, site)
        .await
        .map_err(|e| (RESET_REFUSED_STORE, e))?;
    let mut tx = t
        .backend
        .begin(
            t.catalog.clone(),
            options().map_err(|e| (RESET_REFUSED_STORE, e))?,
        )
        .await
        .map_err(|e| (RESET_REFUSED_STORE, error(e)))?;
    let found = find(&mut tx, doctype::USER, "user_name", "administrator").await;
    let _ = tx.rollback().await;
    let Some(admin) = found.map_err(|e| (RESET_REFUSED_STORE, e))? else {
        return Err((
            RESET_REFUSED_ADMINISTRATOR,
            Error("the Administrator record does not exist on this site".into()),
        ));
    };
    let (secret, credential) = issue_first_sign_in(admin, issue.secret_expires_at());
    let revoked = sitefile::update(root, &t.site.site, |s| {
        let id = admin.to_string();
        let before_keys = s.api_keys.len();
        let before_credentials = s.credentials.len();
        s.api_keys.retain(|k| k.user != id);
        s.credentials.retain(|c| c.user != id);
        let revoked = (before_keys - s.api_keys.len()) + (before_credentials - s.credentials.len());
        s.credentials.push(credential);
        Ok(revoked)
    })
    .map_err(|e| (RESET_REFUSED_STORE, e))?;
    audit(
        root,
        &t.site.site,
        &format!(
            "{{\"action\":\"reset-admin\",\"role\":\"{}\",\"user\":\"{admin}\",\"by\":\"operator\",\"revoked\":{revoked}}}",
            role::TENANT_ADMINISTRATOR
        ),
    )
    .map_err(|e| (RESET_REFUSED_STORE, e))?;
    refresh_running_host(root, site);
    Ok(secret)
}

/// Records an operator action on one user (ids only: no e-mail address, no secret).
fn audit_event(root: &Path, site: &str, action: &str, user: RecordId) -> Result<()> {
    let body = serde_json::json!({ "action": action, "user": user.to_string(), "by": "operator" });
    audit(root, site, &body.to_string())
}

/// Appends one line to the site's audit file (mode 0600, append-only by convention).
pub(crate) fn audit(root: &Path, site: &str, body: &str) -> Result<()> {
    let at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(error)?;
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(
        root.join("sites")
            .join(site)
            .join(concat!("audit", ".jsonl")),
    )?;
    let body = body.strip_prefix('{').unwrap_or(body);
    writeln!(file, "{{\"at\":\"{at}\",{body}")?;
    file.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::email_ok;

    #[test]
    fn email_addresses_are_checked_loosely_but_safely() {
        for good in ["ama@example.com", "a.b+c@sub.example.org"] {
            assert!(email_ok(good), "{good}");
        }
        for bad in [
            "",
            "ama",
            "ama@",
            "@example.com",
            "a@b",
            "a b@example.com",
            "a@@example.com",
            "a@example..",
            "a@.com",
        ] {
            assert!(!email_ok(bad), "{bad}");
        }
    }
}
