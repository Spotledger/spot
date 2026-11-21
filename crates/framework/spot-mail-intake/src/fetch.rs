//! The production [`MailFetcher`]: the IMAP host and port go through the egress guard exactly as
//! SMTP does, and only the addresses it authorised are dialled.

use std::sync::Arc;

use async_trait::async_trait;
use spot_egress::{EgressError, Guard, Purpose};
use spot_mail::{
    FetchBatch, FetchLimits, MailError, MailboxCursor, PermanentFailure, Secret, TransientFailure,
    fetch_new_pinned,
};

use crate::account::Account;
use crate::ports::MailFetcher;

/// Authorises the account's server through a [`Guard`], then calls `fetch_new_pinned`.
pub struct GuardedImapFetcher {
    guard: Arc<Guard>,
    tenant: String,
}

impl GuardedImapFetcher {
    /// A fetcher for the site `tenant`, authorising through `guard`.
    pub fn new(guard: Arc<Guard>, tenant: impl Into<String>) -> Self {
        Self {
            guard,
            tenant: tenant.into(),
        }
    }
}

#[async_trait]
impl MailFetcher for GuardedImapFetcher {
    async fn fetch(
        &self,
        account: &Account,
        secret: Secret,
        cursor: Option<MailboxCursor>,
        limits: FetchLimits,
    ) -> Result<FetchBatch, MailError> {
        let actor = Some(format!("mail_intake:{}", account.name));
        let authorized = match self
            .guard
            .authorize_addr(
                &account.imap.host,
                account.imap.port,
                Purpose::Mail,
                &self.tenant,
                actor,
            )
            .await
        {
            Ok(authorized) => authorized,
            // A lookup that failed may work next time; every denial is a configuration matter.
            Err(EgressError::ResolutionFailed) => {
                return Err(TransientFailure::ConnectionRefused.into());
            }
            Err(denied) => {
                return Err(PermanentFailure::Client(format!(
                    "egress refused the server ({})",
                    denied.code()
                ))
                .into());
            }
        };
        let config = account.imap_config(secret);
        fetch_new_pinned(&config, authorized.socket_addrs(), cursor, limits).await
    }
}
