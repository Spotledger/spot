//! Outgoing mail for one site: the `platform:send_mail` job handler and the ports it sends
//! through. Sending is a job (a document's hook adds it to the outbox, so mail leaves only after
//! the transaction commits); this module has no route.
//!
//! # The four ports
//!
//! - **Accounts** ([`Accounts`]): the one account the site's `services.mail` names, called
//!   `default`. An unknown account id is a configuration failure of the job, not a retry.
//! - **Credentials** ([`Credentials`]): the SMTP password from the same settings, behind the
//!   reference `smtp`. It is read when a connection is made, wrapped in `spot-mail`'s redacting
//!   secret, and never logged, returned in an error or stored.
//! - **Transport** ([`Transports`]): SMTP over a connection pinned to the addresses the egress
//!   guard authorised (`sink = "smtp"`), or a transport that records the message in memory
//!   (`sink = "memory"`, tests and local development).
//! - **Sent log**: the site's PostgreSQL `SentLog` when the durable job store is on, so a
//!   restart between the server's acceptance and the job's completion does not send twice; the
//!   in-memory log otherwise.
//!
//! # Egress
//!
//! Every connection is authorised by an egress guard first. The site's own SMTP host and port
//! are the only thing its policy lists: a port the guard calls dangerous (25) is opened for that
//! host alone, and a private address only when the host is written as that address. A host that
//! resolves to a private address is refused, as for every other outbound call.

use std::future::Future;
use std::net::{IpAddr, Ipv4Addr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use spot_core::{Clock, SiteId};
use spot_egress::{
    AllowEntry, EgressPolicy, Guard, InMemorySink, Purpose, Resolver, SystemResolver, TenantPolicy,
};
use spot_files::store::FileStore;
use spot_jobs::QueuePolicy;
use spot_jobs::worker::JobHandlerRegistry;
use spot_mail::{
    CaptureTransport, MailError, MailTransport, Secret, SmtpConfig, TransportSecurity,
};
use spot_mail_queue::{
    AccountDirectory, CredentialError, CredentialResolver, InMemoryBounceSink, InMemorySentLog,
    MailPorts, MailSender, SentLog, SmtpAccount, TransportFactory, Unavailable,
};
use tokio::sync::watch;

use crate::config;
use crate::services::{CheckFuture, Level, RunFuture, Service};

/// The only account id this host serves.
pub const ACCOUNT: &str = "default";

/// The credential reference the account names.
const SMTP_REFERENCE: &str = "smtp";

type Boxed<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The account the site's settings name.
pub struct Accounts {
    account: SmtpAccount,
}

impl AccountDirectory for Accounts {
    fn account<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        _site: &'life1 SiteId,
        id: &'life2 str,
    ) -> Boxed<'async_trait, Result<Option<SmtpAccount>, Unavailable>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        let found = (id == ACCOUNT).then(|| self.account.clone());
        Box::pin(async move { Ok(found) })
    }
}

/// The SMTP password of the site's settings.
pub struct Credentials {
    password: Option<Secret>,
}

impl CredentialResolver for Credentials {
    fn resolve<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        _site: &'life1 SiteId,
        reference: &'life2 str,
    ) -> Boxed<'async_trait, Result<Secret, CredentialError>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        let found = match (&self.password, reference == SMTP_REFERENCE) {
            (Some(secret), true) => Ok(secret.clone()),
            _ => Err(CredentialError::NotFound),
        };
        Box::pin(async move { found })
    }
}

/// Builds the transport a send uses.
pub struct Transports {
    capture: Option<Arc<CaptureTransport>>,
}

impl TransportFactory for Transports {
    fn build(&self, config: SmtpConfig) -> Result<Arc<dyn MailTransport>, MailError> {
        match &self.capture {
            Some(capture) => Ok(Arc::clone(capture) as Arc<dyn MailTransport>),
            None => spot_mail_queue::SmtpTransportFactory.build(config),
        }
    }
}

/// Answers every host with one public address: the memory sink never dials.
struct FixedResolver;

impl Resolver for FixedResolver {
    fn resolve<'life0, 'life1, 'async_trait>(
        &'life0 self,
        _host: &'life1 str,
    ) -> Boxed<'async_trait, Result<Vec<IpAddr>, std::io::Error>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(vec![IpAddr::V4(Ipv4Addr::new(93, 184, 216, 34))]) })
    }
}

fn security(text: &str) -> TransportSecurity {
    match text {
        "plain" => TransportSecurity::Plain,
        "tls" => TransportSecurity::Tls,
        _ => TransportSecurity::StartTls,
    }
}

/// What the mail service of one site is built from.
pub struct Settings {
    /// The site.
    pub site: SiteId,
    /// The `services.mail` settings.
    pub mail: config::Mail,
    /// The bytes of attachments, from the site's file provider.
    pub files: Arc<dyn FileStore>,
    /// The durable sent log, when the durable job store is on.
    pub sent_log: Option<Arc<dyn SentLog>>,
    /// The source of `now`.
    pub clock: Arc<dyn Clock>,
}

/// The mail service: the handler, its ports and the start-up check.
pub struct Mail {
    sender: Arc<MailSender>,
    guard: Arc<Guard>,
    account: SmtpAccount,
    site: SiteId,
    capture: Option<Arc<CaptureTransport>>,
    bounces: Arc<InMemoryBounceSink>,
    enabled: bool,
}

impl Mail {
    /// Builds the sender over the site's settings.
    ///
    /// The queue policy is the engine's default for the `default` queue: the sender needs its
    /// maximum attempts to recognise the last one.
    pub fn new(settings: Settings) -> Arc<Self> {
        let Settings {
            site,
            mail,
            files,
            sent_log,
            clock,
        } = settings;
        let memory = mail.sink == "memory";
        let smtp = mail.smtp.clone();
        let account = SmtpAccount {
            host: smtp.as_ref().map_or_else(
                || concat!("memory", ".invalid").to_owned(),
                |s| s.host.clone(),
            ),
            port: smtp.as_ref().map_or(587, |s| s.port),
            security: smtp
                .as_ref()
                .map_or(TransportSecurity::Tls, |s| security(&s.security)),
            from_address: smtp.as_ref().map_or_else(
                || "noreply@localhost.invalid".to_owned(),
                |s| s.from.clone(),
            ),
            username: smtp.as_ref().and_then(|s| s.username.clone()),
            credential_ref: smtp
                .as_ref()
                .and_then(|s| s.password.as_ref().map(|_| SMTP_REFERENCE.to_owned())),
            timeout: Duration::from_secs(30),
            pool_size: 2,
            root_certificate_pem: None,
        };
        let capture = memory.then(|| Arc::new(CaptureTransport::new()));
        let guard = Arc::new(Guard::new(
            policy_for(&site, &account),
            if memory {
                Arc::new(FixedResolver)
            } else {
                Arc::new(SystemResolver)
            },
            Arc::new(InMemorySink::new()),
            Arc::clone(&clock),
        ));
        let bounces = Arc::new(InMemoryBounceSink::new());
        let ports = MailPorts {
            accounts: Arc::new(Accounts {
                account: account.clone(),
            }),
            credentials: Arc::new(Credentials {
                password: smtp
                    .as_ref()
                    .and_then(|s| s.password.as_ref())
                    .map(|p| Secret::new(p.clone())),
            }),
            factory: Arc::new(Transports {
                capture: capture.clone(),
            }),
            guard: Arc::clone(&guard),
            files,
            sent_log: sent_log.unwrap_or_else(|| Arc::new(InMemorySentLog::new())),
            bounces: Arc::clone(&bounces) as Arc<dyn spot_mail_queue::BounceSink>,
        };
        // The memory sink dials nothing, so there are no pinned addresses to require.
        let sender =
            Arc::new(MailSender::new(ports, &QueuePolicy::default()).with_require_pinned(!memory));
        Arc::new(Self {
            sender,
            guard,
            account,
            site,
            capture,
            bounces,
            enabled: mail.enabled,
        })
    }

    /// Adds the `platform:send_mail` handler to `registry` (nothing when mail is disabled).
    pub fn register(&self, registry: &mut JobHandlerRegistry) {
        if self.enabled {
            self.sender.register(registry);
        }
    }

    /// The messages the memory sink recorded (the MIME bytes of each), for tests and local
    /// development; empty for an SMTP sink.
    pub fn captured(&self) -> Vec<Vec<u8>> {
        self.capture
            .as_ref()
            .map(|capture| capture.messages())
            .unwrap_or_default()
    }

    /// The bounces recorded in this process.
    pub fn bounces(&self) -> Vec<spot_mail_queue::BounceRecord> {
        self.bounces.records()
    }

    /// The hosted service.
    pub fn service(self: &Arc<Self>) -> Arc<dyn Service> {
        Arc::new(MailService {
            mail: Arc::clone(self),
        })
    }
}

/// The egress policy of a site: its own SMTP host and port, and nothing else.
fn policy_for(site: &SiteId, account: &SmtpAccount) -> EgressPolicy {
    let private = account
        .host
        .parse::<IpAddr>()
        .ok()
        .map(|ip| vec![format!("{ip}")]);
    let entry = AllowEntry {
        scheme: None,
        host: account.host.clone(),
        ports: Some(vec![account.port]),
        purpose: Some(Purpose::Mail.as_str().to_owned()),
        allow_private: private,
    };
    let mut policy = EgressPolicy {
        default: "deny_non_public".to_owned(),
        dangerous_ports: vec![22, 25, 3306, 5432, 6379, 9200, 11211, 2375, 2379, 10250],
        tenants: std::collections::HashMap::new(),
    };
    policy.tenants.insert(
        site.as_str().to_owned(),
        TenantPolicy {
            allow: vec![entry],
            ..TenantPolicy::default()
        },
    );
    policy
}

struct MailService {
    mail: Arc<Mail>,
}

impl Service for MailService {
    fn name(&self) -> &'static str {
        "mail"
    }

    fn level(&self) -> Level {
        Level::Optional
    }

    fn check(&self) -> CheckFuture<'_> {
        Box::pin(async move {
            if !self.mail.enabled {
                return Err("sending mail is disabled in services.mail".to_owned());
            }
            // The guard resolves the host and judges every address: a host that cannot be
            // reached under the site's policy is a degradation now, not a bounce later.
            self.mail
                .guard
                .authorize_addr(
                    &self.mail.account.host,
                    self.mail.account.port,
                    Purpose::Mail,
                    self.mail.site.as_str(),
                    Some("mail_queue:check".to_owned()),
                )
                .await
                .map(|_| ())
                .map_err(|e| {
                    format!(
                        "the mail server is not reachable under the egress policy: {}",
                        e.code()
                    )
                })
        })
    }

    fn run(&self, stop: watch::Receiver<bool>) -> RunFuture<'_> {
        Box::pin(async move {
            let mut stop = stop;
            let _ = stop.changed().await;
            Ok(())
        })
    }
}
