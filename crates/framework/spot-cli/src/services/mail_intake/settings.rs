//! `[services.mail_intake]`: the settings of incoming mail, and their validation.
//!
//! A malformed table (an unknown key, a wrong type) stops the start, as for every service. A
//! well-formed account that breaks a rule of the model is **not** a start failure: it is kept
//! as an [`Invalid`] entry so the host can report it and run the others.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;
use spot_mail::ImapSecurity;
use spot_mail_intake::{Account, ImapEndpoint, IntakeLimits};

use crate::services::process_triggers::SealedSecret;

/// File extensions an incoming attachment may not carry when `blocked_extensions` is not set:
/// executables and scripts.
pub const DEFAULT_BLOCKED_EXTENSIONS: &[&str] = &[
    "exe", "dll", "bat", "cmd", "com", "msi", "scr", "ps1", "psm1", "vbs", "vbe", "js", "jse",
    "wsf", "wsh", "jar", "sh", "bash", "apk", "app", "dmg", "pif", "lnk", "reg", "hta", "cpl",
];

/// The most attachments `max_attachments` may allow.
pub const MAX_ATTACHMENTS: usize = 100;

/// `[services.mail_intake]`. Unknown keys are refused; a value is never echoed in an error.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Settings {
    /// Whether the site receives mail. Off by default: a site without the table registers
    /// nothing.
    pub enabled: bool,
    /// `postgres` (the default) keeps the cursor and the seen set in the site's database;
    /// `memory` keeps them in the process and a restart forgets them (a warning is logged once).
    pub store: StoreKind,
    /// How long one node's lease on an account lasts, in seconds (30 to 3600).
    pub lease_seconds: u64,
    /// The mailboxes.
    pub accounts: Vec<AccountSettings>,
    /// Sealed IMAP passwords by reference (`password_ref`), sealed under the site's
    /// `[services.secrets]` key. The configuration names a secret and never holds one in clear.
    pub secrets: BTreeMap<String, SealedSecret>,
}

/// Where the cursor and the seen set live.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoreKind {
    /// The site's PostgreSQL database.
    #[default]
    Postgres,
    /// This process only.
    Memory,
}

/// One `[[services.mail_intake.accounts]]` entry.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountSettings {
    /// The account's name (a storage name).
    pub name: String,
    /// The IMAP server host.
    pub host: String,
    /// The IMAP port (default 993).
    #[serde(default = "default_port")]
    pub port: u16,
    /// `tls` (implicit, the default) or `starttls`. Plain IMAP does not exist.
    #[serde(default = "default_security")]
    pub security: String,
    /// The LOGIN user name.
    pub username: String,
    /// The account's own address: mail from it is our own and is dropped.
    pub address: String,
    /// The name of the secret in `secrets` that holds the password.
    pub password_ref: String,
    /// The folder to poll.
    #[serde(default = "default_mailbox")]
    pub mailbox: String,
    /// A disabled account is not polled.
    #[serde(default = "yes")]
    pub enabled: bool,
    /// Seconds between polls (30 to 86400).
    #[serde(default = "default_interval")]
    pub poll_interval_seconds: u64,
    /// Seconds before a connection or an IMAP phase times out.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    /// At most this many messages per poll (1 to 200).
    #[serde(default = "default_per_poll")]
    pub max_messages_per_poll: usize,
    /// A message over this many bytes arrives as a header-only stub (at most 25 MiB).
    #[serde(default = "default_max_bytes")]
    pub max_message_bytes: usize,
    /// At most this many messages per UTC day reach the sink (1 to 100000).
    #[serde(default = "default_per_day")]
    pub max_messages_per_day: u32,
    /// The DocType a message becomes (a storage name).
    pub target_doctype: String,
    /// The user the documents are created as: a `user_name` of the site's users. Required.
    pub run_as: String,
    /// Which DocType fields receive which part of a message.
    pub field_map: FieldMap,
    /// Whether attachments are stored as private files attached to the document.
    #[serde(default)]
    pub store_attachments: bool,
    /// At most this many attachments are stored per message; the rest are counted and skipped.
    #[serde(default = "default_max_attachments")]
    pub max_attachments: usize,
    /// Extensions (without the dot, any case) whose attachments are skipped. Defaults to
    /// executable and script types.
    #[serde(default = "default_blocked")]
    pub blocked_extensions: Vec<String>,
}

/// The names of the DocType fields that receive the parts of a message. `from` and `subject`
/// are required.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldMap {
    /// Receives the sender, `Name <address>` or the bare address.
    pub from: String,
    /// Receives the subject.
    pub subject: String,
    /// Receives the recipients (To), comma separated.
    #[serde(default)]
    pub to: Option<String>,
    /// Receives the plain-text body.
    #[serde(default)]
    pub text: Option<String>,
    /// Receives the HTML body.
    #[serde(default)]
    pub html: Option<String>,
    /// Receives the message's date as an instant (a datetime field).
    #[serde(default)]
    pub received_at: Option<String>,
    /// Receives the conversation key (the thread root's Message-ID).
    #[serde(default)]
    pub thread_key: Option<String>,
    /// Receives the Message-ID.
    #[serde(default)]
    pub message_id: Option<String>,
}

fn default_port() -> u16 {
    993
}
fn default_security() -> String {
    "tls".to_owned()
}
fn default_mailbox() -> String {
    "INBOX".to_owned()
}
fn yes() -> bool {
    true
}
fn default_interval() -> u64 {
    300
}
fn default_timeout() -> u64 {
    30
}
fn default_per_poll() -> usize {
    50
}
fn default_max_bytes() -> usize {
    10 * 1024 * 1024
}
fn default_per_day() -> u32 {
    10_000
}
fn default_max_attachments() -> usize {
    10
}
fn default_blocked() -> Vec<String> {
    DEFAULT_BLOCKED_EXTENSIONS
        .iter()
        .map(|e| (*e).to_owned())
        .collect()
}

/// What `validate` found wrong with the table as a whole (not with one account).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TableError(pub String);

impl Settings {
    /// Checks the rules that concern the table, not one account.
    ///
    /// # Errors
    /// The table breaks a rule; the text names the key, never a value.
    pub fn validate(&self) -> Result<(), TableError> {
        if self.lease_seconds != 0 && !(30..=3600).contains(&self.lease_seconds) {
            return Err(TableError(
                "services.mail_intake.lease_seconds must be 30 to 3600".to_owned(),
            ));
        }
        let mut seen = std::collections::BTreeSet::new();
        for account in &self.accounts {
            if !seen.insert(account.name.as_str()) {
                return Err(TableError(
                    "services.mail_intake.accounts: two accounts share one name".to_owned(),
                ));
            }
        }
        Ok(())
    }

    /// The lease time to live: the configured value, or five minutes.
    #[must_use]
    pub fn lease_ttl(&self) -> Duration {
        Duration::from_secs(if self.lease_seconds == 0 {
            300
        } else {
            self.lease_seconds
        })
    }
}

/// An account that passed every rule: the engine's model and what the sink needs.
#[derive(Clone, Debug)]
pub struct Ready {
    /// The engine's account.
    pub account: Account,
    /// The sink's view of the entry.
    pub target: Target,
}

/// What the sink needs of an account's entry.
#[derive(Clone, Debug)]
pub struct Target {
    /// The DocType.
    pub doctype: String,
    /// The user name the documents are created as.
    pub run_as: String,
    /// The field map.
    pub fields: FieldMap,
    /// Whether attachments are stored.
    pub store_attachments: bool,
    /// The attachment cap per message.
    pub max_attachments: usize,
    /// Lower-case blocked extensions.
    pub blocked: Vec<String>,
}

/// An account the host reports and does not run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invalid {
    /// The account's name (as written; it is a name, not a secret).
    pub name: String,
    /// The rule it breaks, naming the key and no value.
    pub reason: String,
}

fn is_storage_name(name: &str) -> bool {
    spot_mail_intake::is_storage_name(name)
}

fn bad(name: &str, key: &str, rule: &str) -> Invalid {
    Invalid {
        name: name.to_owned(),
        reason: format!("{key} {rule}"),
    }
}

impl AccountSettings {
    /// Checks every rule of this entry and builds the engine's account.
    ///
    /// # Errors
    /// The first rule the entry breaks, naming the key and no value.
    pub fn resolve(&self, secrets: &BTreeMap<String, SealedSecret>) -> Result<Ready, Invalid> {
        let n = self.name.as_str();
        if !is_storage_name(n) {
            return Err(bad(n, "name", "must be a storage name"));
        }
        if !is_storage_name(&self.target_doctype) {
            return Err(bad(n, "target_doctype", "must be a storage name"));
        }
        if self.run_as.trim().is_empty() {
            return Err(bad(n, "run_as", "is required"));
        }
        if self.field_map.from.trim().is_empty() || self.field_map.subject.trim().is_empty() {
            return Err(bad(n, "field_map", "needs `from` and `subject`"));
        }
        let mut mapped = vec![&self.field_map.from, &self.field_map.subject];
        for optional in [
            &self.field_map.to,
            &self.field_map.text,
            &self.field_map.html,
            &self.field_map.received_at,
            &self.field_map.thread_key,
            &self.field_map.message_id,
        ]
        .into_iter()
        .flatten()
        {
            mapped.push(optional);
        }
        for field in &mapped {
            if !is_storage_name(field) {
                return Err(bad(
                    n,
                    "field_map",
                    "names a field that is not a storage name",
                ));
            }
        }
        let distinct: std::collections::BTreeSet<_> = mapped.iter().collect();
        if distinct.len() != mapped.len() {
            return Err(bad(n, "field_map", "maps two parts to one field"));
        }
        if !secrets.contains_key(&self.password_ref) {
            return Err(bad(
                n,
                "password_ref",
                "names no entry of services.mail_intake.secrets",
            ));
        }
        if self.max_attachments > MAX_ATTACHMENTS {
            return Err(bad(n, "max_attachments", "must be at most 100"));
        }
        let security = match self.security.as_str() {
            "tls" => ImapSecurity::Tls,
            "starttls" => ImapSecurity::StartTls,
            _ => return Err(bad(n, "security", "must be tls or starttls")),
        };
        let account = Account {
            name: self.name.clone(),
            imap: ImapEndpoint {
                host: self.host.clone(),
                port: self.port,
                security,
                username: self.username.clone(),
                timeout: Duration::from_secs(self.timeout_seconds),
                root_certificate_pem: None,
            },
            credential_ref: self.password_ref.clone(),
            mailbox: self.mailbox.clone(),
            address: self.address.clone(),
            enabled: self.enabled,
            poll_interval: Duration::from_secs(self.poll_interval_seconds),
            limits: IntakeLimits {
                max_messages_per_poll: self.max_messages_per_poll,
                max_message_bytes: self.max_message_bytes,
                max_messages_per_day: self.max_messages_per_day,
            },
        };
        account.validate().map_err(|e| bad(n, e.field, e.rule))?;
        Ok(Ready {
            account,
            target: Target {
                doctype: self.target_doctype.clone(),
                run_as: self.run_as.clone(),
                fields: self.field_map.clone(),
                store_attachments: self.store_attachments,
                max_attachments: self.max_attachments,
                blocked: self
                    .blocked_extensions
                    .iter()
                    .map(|e| e.trim_start_matches('.').to_ascii_lowercase())
                    .collect(),
            },
        })
    }
}

/// Splits the table's accounts into those the host runs and those it reports.
#[must_use]
pub fn resolve_all(settings: &Settings) -> (Vec<Ready>, Vec<Invalid>) {
    let mut ready = Vec::new();
    let mut invalid = Vec::new();
    for entry in &settings.accounts {
        match entry.resolve(&settings.secrets) {
            Ok(account) => ready.push(account),
            Err(why) => invalid.push(why),
        }
    }
    (ready, invalid)
}
