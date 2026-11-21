//! Incoming mail for one site (WP-PLT-11.T13): mailboxes polled by a leader-elected loop,
//! each new message turned into one document of a configured DocType, created as a named user.
//!
//! `spot-mail-intake` is the engine (one poll step per account, a durable UID cursor, dedup by
//! Message-ID, loop and flood protection); this module hosts it. It is **off by default**: a
//! site without `[services.mail_intake] enabled = true` registers nothing. There is no platform
//! DocType; accounts are configuration and the target DocType belongs to an app.
//!
//! # Rules the module keeps
//!
//! - **As a user.** The document is created through the document engine as the `run_as` user, so
//!   that user's create right, field rights, validation and hooks apply. A missing, disabled or
//!   unauthorised user pauses the account (no mail is dropped); a document that fails validation
//!   is a poison message (counted, skipped, the next message still arrives).
//! - **Egress.** Each account's host and port are authorised through the site's egress guard
//!   exactly as SMTP is; a private address needs the host to be written as that address.
//! - **One node polls** an account, under the coordinator lease `mail_intake:<site>:<account>`.
//! - **No secret, address, subject, body or attachment name** reaches a log, a metric label, a
//!   readiness line or an error. The IMAP password is sealed in the settings and opened at poll
//!   time only.
//!
//! # Configuration
//!
//! See `docs/running-spot/mail_intake.md`.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_core::SiteId;
use spot_egress::{
    AllowEntry, EgressPolicy, Guard, InMemorySink, Purpose, SystemResolver, TenantPolicy,
};
use spot_mail_intake::memory::{MemoryCursorStore, MemorySeenStore};
use spot_mail_intake::{CredentialSource, CursorStore, GuardedImapFetcher, IntakeSink, SeenStore};
use spot_mail_intake_pg::{PgCursorStore, PgSeenStore};
use tokio::sync::watch;

use crate::services::files::{FileUpload, Files};
use crate::services::secrets::SecretsHost;
use crate::services::{CheckFuture, Level, RunFuture, Service, SiteCtx};
use crate::{Error, Result};

#[path = "mail_intake/adapters.rs"]
pub mod adapters;
#[path = "mail_intake/attachments.rs"]
pub mod attachments;
#[path = "mail_intake/host.rs"]
pub mod host;
#[path = "mail_intake/settings.rs"]
pub mod settings;
#[path = "mail_intake/sink.rs"]
pub mod sink;
#[path = "mail_intake/status.rs"]
pub mod status;

use adapters::{LiveCreator, LiveUsers, NoCredentials, SealedCredentials};
use host::{Deps, IntakeHost, TracingAudit};
use settings::{Invalid, Ready, Settings, StoreKind, resolve_all};
use sink::{DocumentSink, NoAttachments, SinkParts};
use status::{Status, state};

/// Start order: after the framework's own services and the files service.
pub const ORDER: u32 = 1100;

/// The egress policy of a site: each account's IMAP host and port, and nothing else.
fn policy_for(site: &SiteId, accounts: &[Ready]) -> EgressPolicy {
    let allow = accounts
        .iter()
        .map(|ready| {
            let host = &ready.account.imap.host;
            AllowEntry {
                scheme: None,
                host: host.clone(),
                ports: Some(vec![ready.account.imap.port]),
                purpose: Some(Purpose::Mail.as_str().to_owned()),
                allow_private: host
                    .parse::<std::net::IpAddr>()
                    .ok()
                    .map(|ip| vec![format!("{ip}")]),
            }
        })
        .collect();
    let mut policy = EgressPolicy {
        default: "deny_non_public".to_owned(),
        dangerous_ports: vec![22, 25, 3306, 5432, 6379, 9200, 11211, 2375, 2379, 10250],
        tenants: std::collections::HashMap::new(),
    };
    policy.tenants.insert(
        site.as_str().to_owned(),
        TenantPolicy {
            allow,
            ..TenantPolicy::default()
        },
    );
    policy
}

/// The mail-intake service of one site.
pub struct MailIntake {
    host: Arc<IntakeHost>,
    status: Arc<Status>,
    invalid: Vec<Invalid>,
}

impl MailIntake {
    /// A service over a built host.
    #[must_use]
    pub fn new(host: Arc<IntakeHost>, status: Arc<Status>, invalid: Vec<Invalid>) -> Arc<Self> {
        for entry in &invalid {
            status.set(&entry.name, state::INVALID);
        }
        Arc::new(Self {
            host,
            status,
            invalid,
        })
    }

    /// The accounts' states, for tests and readiness detail.
    #[must_use]
    pub fn status(&self) -> &Arc<Status> {
        &self.status
    }
}

impl Service for MailIntake {
    fn name(&self) -> &'static str {
        "mail_intake"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            // A bad account is reported and does not stop the others; the service is unusable
            // only when no account can run.
            if self.host.accounts().is_empty() {
                let why = self.invalid.first().map_or_else(
                    || "services.mail_intake has no account".to_owned(),
                    |i| format!("account `{}`: {}", i.name, i.reason),
                );
                return Err(why);
            }
            Ok(())
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move { self.host.run(stop).await })
    }
}

/// Registers incoming mail: the service, when the table enables it.
pub async fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let table: Settings = ctx.settings_or_default("mail_intake")?;
    table.validate().map_err(|e| Error(e.0))?;
    // A reload keeps the running service; there is nothing to declare on a router.
    if !table.enabled || !ctx.assembling() {
        return Ok(());
    }
    let env = *ctx.env()?;
    let site_name = env.site.site.clone();
    let site = SiteId::new(&site_name).map_err(|e| Error(e.to_string()))?;
    let clock = Arc::clone(ctx.clock());

    // The files service (ORDER 900) is a port when the site serves files; without it an account
    // that asks for `store_attachments` is reported invalid and the others run.
    let attachments: Arc<dyn sink::AttachmentWriter> = match ctx.port::<Files>() {
        Some(files) => Arc::new(attachments::FilesAttachments::new(
            site.clone(),
            files as Arc<dyn FileUpload>,
        )),
        None => Arc::new(NoAttachments),
    };
    let (mut ready, mut invalid) = resolve_all(&table);
    ready.retain(|entry| {
        if entry.target.store_attachments && !attachments.supported() {
            invalid.push(Invalid {
                name: entry.account.name.clone(),
                reason: "store_attachments needs the files service, which this site does not serve"
                    .to_owned(),
            });
            return false;
        }
        true
    });
    for entry in &invalid {
        tracing::error!(site = %site_name, account = %entry.name, rule = %entry.reason,
            "a mail intake account is invalid and will not be polled");
    }

    let node = format!(
        "mail-intake-{site_name}-{}",
        spot_storage_contract::RecordId::new_v7()
            .to_string()
            .rsplit('-')
            .next()
            .unwrap_or("0")
    );
    let cluster = env.loaded.common.services.profile == "cluster";
    let coordinator = adapters::coordinator(env.backend, cluster, Arc::clone(&clock), &node)
        .await
        .map_err(|e| Error(format!("services.mail_intake: {e}")))?;

    let (cursors, seen): (Arc<dyn CursorStore>, Arc<dyn SeenStore>) = match table.store {
        StoreKind::Postgres => {
            let pool = env.backend.pool();
            let cursors = PgCursorStore::new(pool.clone(), site.clone());
            cursors.prepare().await.map_err(|_| {
                Error("services.mail_intake: the intake tables could not be created".to_owned())
            })?;
            (
                Arc::new(cursors),
                Arc::new(PgSeenStore::new(pool, site.clone())),
            )
        }
        StoreKind::Memory => {
            tracing::warn!(site = %site_name,
                "services.mail_intake.store = memory: a restart forgets the mailbox position and what was imported");
            (
                Arc::new(MemoryCursorStore::new()),
                Arc::new(MemorySeenStore::new()),
            )
        }
    };

    let credentials: Arc<dyn CredentialSource> = match ctx.port::<SecretsHost>() {
        Some(host) => Arc::new(SealedCredentials::new(
            Arc::clone(host.keyring()),
            table.secrets.clone(),
        )),
        None => Arc::new(NoCredentials),
    };

    let guard = Arc::new(Guard::new(
        policy_for(&site, &ready),
        Arc::new(SystemResolver),
        Arc::new(InMemorySink::new()),
        Arc::clone(&clock),
    ));
    let status = Arc::new(Status::new());
    let targets: BTreeMap<String, settings::Target> = ready
        .iter()
        .map(|r| (r.account.name.clone(), r.target.clone()))
        .collect();
    let sink: Arc<dyn IntakeSink> = Arc::new(DocumentSink::new(SinkParts {
        site: site_name.clone(),
        targets,
        users: Arc::new(LiveUsers {
            backend: env.backend.clone(),
            live: Arc::clone(env.live),
        }),
        creator: Arc::new(LiveCreator {
            site: site.clone(),
            live: Arc::clone(env.live),
            clock: Arc::clone(&clock),
        }),
        attachments,
        status: Arc::clone(&status),
        clock: Arc::clone(&clock),
    }));
    let deps = Arc::new(Deps {
        site: site_name.clone(),
        coordinator,
        node,
        lease_ttl: table.lease_ttl(),
        cursors,
        seen,
        sink,
        audit: Arc::new(TracingAudit {
            site: site_name.clone(),
        }),
        credentials,
        fetcher: Arc::new(GuardedImapFetcher::new(guard, site_name)),
        clock,
        status: Arc::clone(&status),
    });
    let accounts = ready.into_iter().map(|r| r.account).collect();
    let host = Arc::new(IntakeHost::new(deps, accounts));
    ctx.add(MailIntake::new(host, status, invalid));
    Ok(())
}
