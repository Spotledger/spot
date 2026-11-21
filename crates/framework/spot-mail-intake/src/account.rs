//! The account model: where to poll, how often, and the limits that keep one mailbox from
//! flooding a site. An account holds a credential **reference**, never the secret.

use std::fmt;
use std::time::Duration;

use spot_mail::{ImapConfig, ImapSecurity, Secret};

use crate::codes;

/// The shortest poll interval an account may ask for.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// The longest poll interval an account may ask for: one day.
pub const MAX_POLL_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// The largest `max_message_bytes` an account may configure: 25 MiB.
pub const MAX_MESSAGE_BYTES: usize = 25 * 1024 * 1024;
/// The largest `max_messages_per_poll` an account may configure.
pub const MAX_MESSAGES_PER_POLL: usize = 200;
/// The largest `max_messages_per_day` an account may configure.
pub const MAX_MESSAGES_PER_DAY: u32 = 100_000;

/// Why an [`Account`] is not valid. Names the field and the rule, never a value, so the
/// message cannot carry an address or a credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountError {
    /// The field that breaks the rule.
    pub field: &'static str,
    /// The rule it breaks, in a few words.
    pub rule: &'static str,
}

impl AccountError {
    /// The stable catalogue code, `mail_intake.account_invalid`.
    pub fn code(&self) -> &'static str {
        codes::ACCOUNT_INVALID
    }
}

impl fmt::Display for AccountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "mail intake account: {} {}", self.field, self.rule)
    }
}

impl std::error::Error for AccountError {}

/// The IMAP server of an account: [`ImapConfig`] without the secret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImapEndpoint {
    /// The server host, a name or an address.
    pub host: String,
    /// The server port: 993 for `Tls`, 143 for `StartTls`.
    pub port: u16,
    /// How the connection is protected. Plain IMAP does not exist.
    pub security: ImapSecurity,
    /// The LOGIN user name.
    pub username: String,
    /// The timeout of the connection and of every protocol phase.
    pub timeout: Duration,
    /// A root certificate that replaces the platform trust store for this server.
    pub root_certificate_pem: Option<Vec<u8>>,
}

/// What one poll may take in, per account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntakeLimits {
    /// At most this many messages are fetched by one poll (1 to 200).
    pub max_messages_per_poll: usize,
    /// A message over this size reaches the sink as a header-only stub (at most 25 MiB).
    pub max_message_bytes: usize,
    /// At most this many messages reach the sink per UTC day (1 to 100,000); the rest are
    /// dropped and counted, and the cursor still moves.
    pub max_messages_per_day: u32,
}

impl Default for IntakeLimits {
    fn default() -> Self {
        Self {
            max_messages_per_poll: 50,
            max_message_bytes: MAX_MESSAGE_BYTES,
            max_messages_per_day: 10_000,
        }
    }
}

/// One mailbox that a site receives mail from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// The account's name: a storage name (`[a-z][a-z0-9_]*`, at most 63 bytes). It keys the
    /// cursor, the seen set and the lease.
    pub name: String,
    /// The IMAP server.
    pub imap: ImapEndpoint,
    /// The reference [`crate::CredentialSource`] turns into the password.
    pub credential_ref: String,
    /// The mailbox (folder) to poll, for example `INBOX`.
    pub mailbox: String,
    /// The account's own address. A message whose `From` equals it is our own mail and is
    /// dropped (loop protection).
    pub address: String,
    /// A disabled account is never polled.
    pub enabled: bool,
    /// The time between polls after a success (30 s to one day).
    pub poll_interval: Duration,
    /// The per-poll and per-day limits.
    pub limits: IntakeLimits,
}

/// Whether `name` is a storage name: `[a-z][a-z0-9_]*`, at most 63 bytes.
pub fn is_storage_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    name.len() <= 63 && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn bad(field: &'static str, rule: &'static str) -> AccountError {
    AccountError { field, rule }
}

impl Account {
    /// Checks every rule of the model before anything touches the network.
    pub fn validate(&self) -> Result<(), AccountError> {
        if !is_storage_name(&self.name) {
            return Err(bad("name", "must be a storage name"));
        }
        if self.imap.host.trim().is_empty() {
            return Err(bad("imap.host", "must not be empty"));
        }
        if self.imap.port == 0 {
            return Err(bad("imap.port", "must be non-zero"));
        }
        if self.imap.username.is_empty() {
            return Err(bad("imap.username", "must not be empty"));
        }
        if self.imap.timeout.is_zero() {
            return Err(bad("imap.timeout", "must be greater than zero"));
        }
        if self.credential_ref.trim().is_empty() {
            return Err(bad("credential_ref", "must not be empty"));
        }
        if self.mailbox.is_empty() {
            return Err(bad("mailbox", "must not be empty"));
        }
        if !self.address.contains('@') {
            return Err(bad("address", "must be an e-mail address"));
        }
        if self.poll_interval < MIN_POLL_INTERVAL || self.poll_interval > MAX_POLL_INTERVAL {
            return Err(bad(
                "poll_interval",
                "must be between 30 seconds and one day",
            ));
        }
        let limits = &self.limits;
        if limits.max_messages_per_poll == 0 || limits.max_messages_per_poll > MAX_MESSAGES_PER_POLL
        {
            return Err(bad("limits.max_messages_per_poll", "must be 1 to 200"));
        }
        if limits.max_message_bytes == 0 || limits.max_message_bytes > MAX_MESSAGE_BYTES {
            return Err(bad("limits.max_message_bytes", "must be 1 byte to 25 MiB"));
        }
        if limits.max_messages_per_day == 0 || limits.max_messages_per_day > MAX_MESSAGES_PER_DAY {
            return Err(bad("limits.max_messages_per_day", "must be 1 to 100000"));
        }
        Ok(())
    }

    /// The transport configuration with the resolved secret filled in.
    pub fn imap_config(&self, secret: Secret) -> ImapConfig {
        ImapConfig {
            host: self.imap.host.clone(),
            port: self.imap.port,
            security: self.imap.security,
            credentials: spot_mail::Credentials {
                user: self.imap.username.clone(),
                secret,
            },
            mailbox: self.mailbox.clone(),
            timeout: self.imap.timeout,
            root_certificate_pem: self.imap.root_certificate_pem.clone(),
        }
    }
}
