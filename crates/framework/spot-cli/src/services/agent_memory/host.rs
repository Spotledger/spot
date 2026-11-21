//! Provisioning and opening the tenant's memory database.
//!
//! The memory of one tenant lives in **a database of its own, owned by a role of its own**, on the
//! control server (`spot-agent-memory-pg`'s provisioner): PostgreSQL has no cross-database query,
//! so a connection as the memory role reaches no business table, and the role is granted nothing
//! on any business database. This module is the host's one call into that: derive the tenant from
//! the site, read the role's password from a file the operator keeps, provision **idempotently** at
//! first use (an existing database, role and record are kept and the grants re-applied, so a
//! resume after a crash converges), open the store on the memory database and prepare its tables.
//!
//! Every failure is a [`HostFailure`] whose text names the step and the fix and never a database
//! name, a role, a password or the driver's message.

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use spot_agent_memory::TenantId;
use spot_agent_memory_pg::{
    EXTENSION_MISSING, MemoryPlacement, PgMemoryProvisioner, PgMemoryStore,
};
use spot_core::SiteId;
use spot_postgres::PgConfig;

/// Why the memory database could not be made ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostFailure {
    /// The site name is not a memory tenant id.
    Tenant,
    /// `password_file` is not set, cannot be read, or holds a password the provisioner refuses.
    Password,
    /// The PostgreSQL server has no `vector` extension. Installing it is a privileged step.
    ExtensionMissing,
    /// A provisioning or migration step failed; the string is the crate's own step code.
    Provision(String),
    /// The store could not be opened on the memory database (TLS, pool, runtime).
    Store,
}

impl HostFailure {
    /// The sentence readiness shows: the step, and what to do about it. Never a secret.
    #[must_use]
    pub fn degraded_text(&self) -> String {
        match self {
            Self::Tenant => "the site name is not a valid agent memory tenant id".to_owned(),
            Self::Password => {
                "agent_memory: `password_file` must name a readable file holding the memory \
                 role's password (16 to 128 characters of letters, digits, `.`, `_`, `~`, `-`)"
                    .to_owned()
            }
            Self::ExtensionMissing => "agent_memory: the PostgreSQL server has no `vector` \
                 extension (pgvector). Install the pgvector package on the server (creating an \
                 extension is a privileged step), then restart the site; memory stays off until \
                 then"
                .to_owned(),
            Self::Provision(code) => format!(
                "agent_memory: the memory database could not be provisioned ({code}); check that \
                 the control connection may create databases and roles, then restart the site"
            ),
            Self::Store => "agent_memory: the memory store could not be opened (a database that \
                 requires TLS is not supported by this store)"
                .to_owned(),
        }
    }
}

/// The tenant whose memory a site's database holds.
///
/// # Errors
///
/// [`HostFailure::Tenant`] when the site name is not a valid tenant id (it always is for a valid
/// `SiteId`: lower-case letters, digits and hyphens).
pub fn tenant_of(site: &SiteId) -> Result<TenantId, HostFailure> {
    TenantId::parse(site.as_str()).map_err(|_| HostFailure::Tenant)
}

/// Reads the memory role's password from `path`: the trimmed content of the file.
///
/// # Errors
///
/// [`HostFailure::Password`] when the file cannot be read or is empty. The provisioner checks the
/// alphabet and the length itself, before it uses the value.
pub fn read_password(path: &Path) -> Result<String, HostFailure> {
    let text = std::fs::read_to_string(path).map_err(|_| HostFailure::Password)?;
    let password = text.trim().to_owned();
    if password.is_empty() {
        return Err(HostFailure::Password);
    }
    Ok(password)
}

/// Runtimes the stores block on, kept alive for the life of the process: a store and a provisioner
/// hold an `Arc<Runtime>`, and dropping the last reference inside an async context panics.
fn keep_alive(runtime: &Arc<tokio::runtime::Runtime>) {
    static KEPT: OnceLock<Mutex<Vec<Arc<tokio::runtime::Runtime>>>> = OnceLock::new();
    KEPT.get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .push(Arc::clone(runtime));
}

/// A runtime for the blocking store calls: two idle threads, kept alive for the process.
///
/// # Errors
///
/// [`HostFailure::Store`] when the runtime cannot start.
pub fn runtime() -> Result<Arc<tokio::runtime::Runtime>, HostFailure> {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("spot-agent-memory")
            .enable_all()
            .build()
            .map_err(|_| HostFailure::Store)?,
    );
    keep_alive(&runtime);
    Ok(runtime)
}

/// A ready tenant memory: the placement and the store on its database.
pub struct Hosted {
    /// Where the memory lives (database and role names, no secret).
    pub placement: MemoryPlacement,
    /// The store, bound to the tenant, on the memory database.
    pub store: Arc<PgMemoryStore>,
}

/// Provisions `tenant`'s memory (idempotent) and opens and prepares the store. Blocking: call it
/// from a blocking context (`block_in_place`).
///
/// `admin` is the control connection; the memory role's own connection is derived from it and the
/// password.
///
/// # Errors
///
/// A [`HostFailure`] naming the step.
pub fn provision_and_open(
    admin: &PgConfig,
    collation: &str,
    tenant: &TenantId,
    password: &str,
    now_ms: i64,
    runtime: Arc<tokio::runtime::Runtime>,
) -> Result<Hosted, HostFailure> {
    let provisioner = PgMemoryProvisioner::new(admin.clone(), collation, Arc::clone(&runtime));
    provisioner.prepare_control().map_err(failure)?;
    let placement = provisioner
        .provision(tenant, password, now_ms)
        .map_err(failure)?;
    let store = PgMemoryStore::connect(
        &placement.config(admin, password),
        tenant.clone(),
        Arc::clone(&runtime),
    )
    .map_err(|_| HostFailure::Store)?;
    store.prepare().map_err(failure)?;
    Ok(Hosted {
        placement,
        store: Arc::new(store),
    })
}

/// Maps a crate error onto the host's vocabulary, keeping only the step code.
fn failure(error: spot_app_api::AppError) -> HostFailure {
    if error.code == EXTENSION_MISSING {
        HostFailure::ExtensionMissing
    } else {
        HostFailure::Provision(error.code.to_string())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;

    #[test]
    fn every_failure_says_the_step_and_the_fix_and_nothing_secret() {
        for failure in [
            HostFailure::Tenant,
            HostFailure::Password,
            HostFailure::ExtensionMissing,
            HostFailure::Provision("a_step_code".to_owned()),
            HostFailure::Store,
        ] {
            let text = failure.degraded_text();
            assert!(!text.contains("postgres://"), "{text}");
            assert!(!text.to_lowercase().contains("password:"), "{text}");
        }
        let missing = HostFailure::ExtensionMissing.degraded_text();
        assert!(missing.contains("pgvector") && missing.contains("Install"));
    }

    #[test]
    fn a_site_name_is_a_tenant_and_a_password_file_is_read_trimmed() {
        let site = SiteId::new("acme-ltd").unwrap();
        assert_eq!(tenant_of(&site).unwrap().as_str(), "acme-ltd");
        let path = std::env::temp_dir().join(format!("spot_memory_pw_{}", std::process::id()));
        std::fs::write(&path, "  a-long-enough-password-0001 \n").unwrap();
        assert_eq!(read_password(&path).unwrap(), "a-long-enough-password-0001");
        std::fs::write(&path, "\n").unwrap();
        assert_eq!(read_password(&path), Err(HostFailure::Password));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read_password(&path), Err(HostFailure::Password));
    }
}
