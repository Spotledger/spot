//! Emergency access without a running server: what the `spot access emergency` commands build.
//!
//! A command runs in its own process, directly on the site's database, as an operator. It builds
//! the same [`Emergency`] the server does, over the same store, the same assignment records and
//! the same access-change journal, so it goes through the same policy and writes the same
//! events; it does not bypass anything. What needs a running server is left to the server's
//! sweep, which is why each step is a stored transition: with no process host the command's
//! request waits and the server starts its approval; with no governance gate the command's approval is
//! recorded and the server activates the grant (running the segregation-of-duties gate); the
//! recording of a session a command ended is archived by the server.
//!
//! The operator is named by `--as <user name>` and resolved to the user's record. That names who
//! acts; it is not a credential, and it carries the same trust as every other operator command
//! (direct access to the host and its database).

use std::path::Path;
use std::sync::Arc;

use spot_bpm_engine::InMemoryTimerPort;
use spot_core::Clock;
use spot_platform_app::doctype;
use spot_storage_contract::{Isolation, StorageBackend, Transaction, TxOptions};

use super::flow::{Emergency, Parts, RefusalLog};
use super::model::{CODE_DISABLED, CODE_SETTINGS_INVALID, Settings};
use super::seams::{NoRecording, PgAssignments, RecordIds};
use super::store::{GrantStore, PgKv};
use crate::services::access_questions_model::history::ChainJournal;
use crate::services::governance::GovernanceStore;
use crate::services::scim_store::{find_id, nm};
use crate::{Error, Result};

/// A command's view of one site.
pub struct Offline {
    /// The emergency access of the site.
    pub emergency: Emergency,
    backend: spot_postgres::PgBackend,
    catalog: Arc<spot_storage_contract::Catalog>,
}

impl Offline {
    /// Opens `site` of the host at `root`.
    ///
    /// # Errors
    ///
    /// `emergency_access_host.disabled` when the site does not enable the service,
    /// `emergency_access_host.settings_invalid` when its settings do not validate, or the
    /// configuration, metadata or database error that stopped it.
    pub async fn open(root: &Path, site: &str, clock: Arc<dyn Clock>) -> Result<Offline> {
        let loaded = crate::config::load(root, false)?;
        let site_cfg = loaded
            .sites
            .iter()
            .find(|s| s.site == site)
            .ok_or_else(|| Error(format!("there is no site `{site}`")))?;
        let settings: Settings = match loaded
            .common
            .services
            .services_extra
            .get("emergency_access")
        {
            Some(table) => table.clone().try_into().map_err(|_| {
                Error(format!(
                    "{CODE_SETTINGS_INVALID}: services.emergency_access does not decode"
                ))
            })?,
            None => Settings::default(),
        };
        if !settings.enabled {
            return Err(Error(format!(
                "{CODE_DISABLED}: [services.emergency_access] is not enabled for this host"
            )));
        }
        let policy = settings.validate().map_err(|name| {
            Error(format!(
                "{CODE_SETTINGS_INVALID}: services.emergency_access.{name} is missing or out of range"
            ))
        })?;
        let meta = crate::apps::serving(root, &loaded, site_cfg)?;
        let backend = crate::schema::backend(&loaded, site_cfg).await?;
        let catalog = Arc::new(crate::schema::catalog(&meta)?);
        let kv = PgKv::new(backend.pool(), site);
        kv.prepare()
            .await
            .map_err(|_| Error("the emergency access table could not be prepared".to_owned()))?;
        let kv: Arc<dyn GovernanceStore> = Arc::new(kv);
        let journal = Arc::new(ChainJournal::new(
            spot_audit_store::PgAuditStore::new(backend.pool()),
            site,
        ));
        let emergency = Emergency::new(Parts {
            site: site.to_owned(),
            clock,
            policy,
            store: GrantStore::new(kv, site),
            assignments: Arc::new(PgAssignments::new(
                site,
                backend.clone(),
                Arc::clone(&catalog),
            )),
            guard: None,
            launcher: None,
            journal: Some(journal),
            recording: Arc::new(NoRecording),
            ids: Arc::new(RecordIds),
            timers: Arc::new(InMemoryTimerPort::new()),
            rights: None,
            refusals: Arc::new(RefusalLog::default()),
        });
        Ok(Offline {
            emergency,
            backend,
            catalog,
        })
    }

    /// The record id of the user called `user_name`.
    ///
    /// # Errors
    ///
    /// An error when there is no such user or the database cannot be read.
    pub async fn user_id(&self, user_name: &str) -> Result<String> {
        let options = TxOptions::new(
            nm("emergency_operator").map_err(|_| Error("internal name".to_owned()))?,
            Isolation::ReadCommitted,
        )
        .read_only();
        let mut tx = self
            .backend
            .begin(Arc::clone(&self.catalog), options)
            .await
            .map_err(|_| Error("the database could not be read".to_owned()))?;
        let found = find_id(&mut tx, doctype::USER, "user_name", user_name).await;
        let _ = tx.rollback().await;
        match found {
            Ok(Some(id)) => Ok(id.to_string()),
            Ok(None) => Err(Error("there is no such user".to_owned())),
            Err(_) => Err(Error("the database could not be read".to_owned())),
        }
    }
}
