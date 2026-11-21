//! Sealed and encrypted protected fields on a node (WP-SEC-10.T6, ADR-070 rule 12.13). Guide:
//! `docs/running-spot/sealed_fields.md`.
//!
//! `spot-sealed-fields` has the behaviour (seal on write, never read back, reveal through the
//! hub); this module hosts it. It turns the sync-class table of the `protected_keys` module and the
//! protection facts of the served metadata into **one rule per protected field**, and offers:
//!
//! - [`SealedFields`]: the field transform. [`SealedFields::seal_document`] replaces the value of
//!   every field in a `sealed` group with a [`SEALED_PREFIX`] envelope (a box to the hub's public
//!   key, bound to tenant, DocType, field and record) and every field in an `encrypted` group
//!   with an [`ENCRYPTED_PREFIX`] envelope (the group data key the node holds). Child-table columns
//!   are bound to the parent record **and the row**, so a ciphertext cannot move between rows.
//! - [`PlaintextGuard`]: a `TxParticipant`. A write that still carries a plaintext value in a
//!   protected field is refused (`sealed_fields_host.plaintext_refused`) before anything is stored,
//!   so a site that hosts this module can never store one. This is the fail-closed half that does
//!   not need the engine to change.
//! - [`Revealer`]: reveal over the sync transport. Offline it answers `sealed_fields.reveal_offline`
//!   and never serves a cached plaintext; each call counts against a per-user rate **before** the
//!   hub is asked.
//!
//! ```toml
//! [services.sealed_fields]
//! enabled = true
//! reveal_per_minute = 30
//! ```
//!
//! # The write transform (WP-SEC-10.T7)
//!
//! The participant seam lends the write read-only, so a participant can refuse a write but not
//! replace a value. The engine's [`WriteTransform`] seam runs after authorisation and validation and
//! before the participants and the storage write: [`SealTransform`] calls
//! [`SealedFields::seal_document`] there, so the stored value is the envelope and the
//! [`PlaintextGuard`] (which runs next) only ever sees protected forms. The module provides the
//! transform as the `dyn WriteTransform` port; the host installs it with `Engine::set_write_transform`.
//! Without the transform installed a protected field still cannot be written at all (the guard
//! refuses): the safe failure.
//!
//! # Fail closed
//!
//! A tagged group with no class is `sealed_fields.class_unset`; an unknown DocType, a non-text value
//! in a protected field, an `encrypted` group with no node keys: refused. No value, key or sealed
//! blob appears in an error, log line or record: names, counts and codes only. The service is
//! **required** (key custody).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use spot_app_api::prelude::{Name, Value};
use spot_app_api::{AppError, AppResult, BoxFuture, Document, ErrorClass};
use spot_core::Clock;
use spot_document::engine::{TxParticipant, WriteIntent, WriteTransform, WriteView};
use spot_field_guard::{DocProtection, ProtectionSource};
use spot_permissions::Principal;
use spot_postgres::PgTransaction;
use spot_protected_keys::{GroupName, MonoTime, TenantId};
use spot_sealed_fields::{
    EncryptedField, HubPort, HubPublicKey, RemoteReveal, RevealError, Revealed, SealedAd,
    SealedFieldsError, SealedWriter, SyncClass, SyncClassTable,
};
use spot_storage_contract::Transaction;

use crate::services::{Idle, Level, SiteCtx};
use crate::{Error, Result};

/// Raised in this module's messages as `sealed_fields_host.settings_invalid: ...`.
#[allow(dead_code)]
const CODE_SEALED_FIELDS_HOST_SETTINGS_INVALID: &str = "sealed_fields_host.settings_invalid";

/// The module's name.
pub const NAME: &str = "sealed_fields";
/// After `protected_keys` (1060), whose class table and key custody it reads.
pub const ORDER: u32 = 1070;
/// The stored form of a sealed value: this prefix, then the box in lower-case hex.
pub const SEALED_PREFIX: &str = "$sealed:v1:";
/// The stored form of an encrypted value: this prefix, then the JSON of an `EncryptedField` in
/// lower-case hex.
pub const ENCRYPTED_PREFIX: &str = "$enc:v1:";
/// The most reveals one user may make in a minute, whatever the setting says.
pub const MAX_REVEAL_PER_MINUTE: u32 = 10_000;
/// The most users the rate window tracks at once. A full window refuses new users (fail closed).
pub const MAX_TRACKED_USERS: usize = 10_000;

/// `[services.sealed_fields]`.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Off unless `true`.
    pub enabled: bool,
    /// Reveals one user may make in one minute (counted before the hub is asked).
    pub reveal_per_minute: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            reveal_per_minute: 30,
        }
    }
}

impl Settings {
    /// # Errors
    ///
    /// [`Error`] for a rate of zero or above [`MAX_REVEAL_PER_MINUTE`].
    pub fn validate(&self) -> Result<()> {
        if self.reveal_per_minute == 0 || self.reveal_per_minute > MAX_REVEAL_PER_MINUTE {
            return Err(Error(format!(
                "sealed_fields_host.settings_invalid: reveal_per_minute is 1 to {MAX_REVEAL_PER_MINUTE}"
            )));
        }
        Ok(())
    }
}

// ------------------------------------------------------------------------------- errors

/// Why the host refused a field operation. Never carries a value.
#[derive(Debug)]
pub enum HostError {
    /// The protection source does not know the DocType: nothing can be judged.
    UnknownDocType,
    /// A protected field holds a value that is not the protected form.
    PlaintextRefused {
        /// The field path (`field` or `table.column`).
        field: String,
    },
    /// A protected field holds something other than text.
    NotText {
        /// The field path.
        field: String,
    },
    /// A group needs the node's group keys and the host has none.
    NoEncryptor {
        /// The field path.
        field: String,
    },
    /// The sealing layer refused (an unset class, a stale key, a failed cipher).
    Sealed(SealedFieldsError),
}

impl HostError {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownDocType => "sealed_fields_host.unknown_doctype",
            Self::PlaintextRefused { .. } => "sealed_fields_host.plaintext_refused",
            Self::NotText { .. } => "sealed_fields_host.field_not_text",
            Self::NoEncryptor { .. } => "sealed_fields_host.no_encryptor",
            Self::Sealed(error) => error.code(),
        }
    }

    /// The error a write answers with: the code and the field name, never a value.
    #[must_use]
    pub fn into_app_error(self) -> AppError {
        let class = match &self {
            Self::UnknownDocType | Self::NoEncryptor { .. } => ErrorClass::Unavailable,
            Self::Sealed(SealedFieldsError::LeaseExpired { .. }) => ErrorClass::Unavailable,
            _ => ErrorClass::PermissionDenied,
        };
        let message = match &self {
            Self::PlaintextRefused { field }
            | Self::NotText { field }
            | Self::NoEncryptor { field } => {
                format!("The protected field `{field}` cannot be stored in this form.")
            }
            _ => "A protected field cannot be written on this node.".to_owned(),
        };
        AppError::new(class, self.code(), message)
    }
}

impl std::fmt::Display for HostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for HostError {}

// ------------------------------------------------------------------------------- hex

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 15), 16).unwrap_or('0'));
    }
    out
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digits: Vec<u32> = text
        .chars()
        .map(|c| c.to_digit(16))
        .collect::<Option<Vec<_>>>()?;
    digits
        .chunks(2)
        .map(|pair| match pair {
            [hi, lo] => u8::try_from(hi * 16 + lo).ok(),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------------------------- rules

/// How strict a class is when a field sits in several groups (a derived field): sealed beats
/// home-only beats encrypted beats everywhere.
fn rank(class: SyncClass) -> u8 {
    match class {
        SyncClass::Sealed => 3,
        SyncClass::HomeOnly => 2,
        SyncClass::Encrypted => 1,
        SyncClass::Everywhere => 0,
    }
}

/// One protected field: its path, the group whose class governs it and that class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldRule {
    /// `field`, or `table.column` for a child-table column.
    pub path: String,
    /// The group whose class applies (the strictest of the field's groups).
    pub group: GroupName,
    /// The class.
    pub class: SyncClass,
}

impl FieldRule {
    /// Whether the host changes the stored value of this field (`sealed` and `encrypted`).
    #[must_use]
    pub fn acts(&self) -> bool {
        matches!(self.class, SyncClass::Sealed | SyncClass::Encrypted)
    }
}

fn rules_of(
    prefix: &str,
    prot: &DocProtection,
    table: &SyncClassTable,
) -> std::result::Result<Vec<FieldRule>, HostError> {
    let mut out = Vec::new();
    for facts in &prot.fields {
        let mut best: Option<(GroupName, SyncClass)> = None;
        for group in &facts.groups {
            let name = GroupName::new(&group.0)
                .map_err(|_| HostError::Sealed(SealedFieldsError::Malformed("a group name")))?;
            let class = table
                .class_for(&name, &facts.tags)
                .map_err(HostError::Sealed)?;
            if best
                .as_ref()
                .is_none_or(|(_, held)| rank(class) > rank(*held))
            {
                best = Some((name, class));
            }
        }
        if let Some((group, class)) = best {
            out.push(FieldRule {
                path: format!("{prefix}{}", facts.path),
                group,
                class,
            });
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------------------- encryption seam

/// Encrypts a value under the group data key the node holds. The node's key custody (release
/// under lease, revocation) lives elsewhere; this is the one call the transform needs.
pub trait GroupEncryptor: Send + Sync {
    /// Encrypts `plaintext` for one field of one record under the newest held epoch of `group`.
    ///
    /// # Errors
    ///
    /// `unknown_group` when no key is held, `lease_expired` once the lease passed.
    fn encrypt(
        &self,
        group: &GroupName,
        place: &Place<'_>,
        plaintext: &[u8],
        at: MonoTime,
    ) -> std::result::Result<EncryptedField, SealedFieldsError>;
}

/// Where a protected value lives.
#[derive(Clone, Copy, Debug)]
pub struct Place<'a> {
    /// The DocType.
    pub doctype: &'a str,
    /// The field path.
    pub field: &'a str,
    /// The record (a child row is `<parent>/<row>`).
    pub document: &'a str,
}

// ------------------------------------------------------------------------------- the transform

/// The field transform and the plaintext check of one site.
pub struct SealedFields {
    tenant: TenantId,
    source: Arc<dyn ProtectionSource>,
    table: Arc<SyncClassTable>,
    writer: SealedWriter,
    encryptor: Option<Arc<dyn GroupEncryptor>>,
}

impl SealedFields {
    /// A transform for `tenant` over the served protection facts and the class table, sealing to
    /// `hub`.
    #[must_use]
    pub fn new(
        tenant: TenantId,
        source: Arc<dyn ProtectionSource>,
        table: Arc<SyncClassTable>,
        hub: HubPublicKey,
        encryptor: Option<Arc<dyn GroupEncryptor>>,
    ) -> Self {
        Self {
            tenant,
            source,
            table,
            writer: SealedWriter::from_public_key(hub),
            encryptor,
        }
    }

    /// The rules of `doctype`: its own protected fields and those of its child tables.
    ///
    /// # Errors
    ///
    /// `unknown_doctype` for a DocType (or child) the source does not know; `class_unset` for a
    /// tagged group with no class.
    pub fn rules(&self, doctype: &str) -> std::result::Result<Vec<FieldRule>, HostError> {
        let prot = self
            .source
            .protection(doctype)
            .ok_or(HostError::UnknownDocType)?;
        let mut out = rules_of("", &prot, &self.table)?;
        for (table, child) in &prot.tables {
            let child_prot = self
                .source
                .protection(child)
                .ok_or(HostError::UnknownDocType)?;
            out.extend(rules_of(&format!("{table}."), &child_prot, &self.table)?);
        }
        Ok(out)
    }

    /// Whether `text` is the protected form of `class`.
    fn is_protected_form(class: SyncClass, text: &str) -> bool {
        match class {
            SyncClass::Sealed => text
                .strip_prefix(SEALED_PREFIX)
                .and_then(unhex)
                .is_some_and(|bytes| spot_sealed_fields::Sealed::from_bytes(&bytes).is_ok()),
            SyncClass::Encrypted => text
                .strip_prefix(ENCRYPTED_PREFIX)
                .and_then(unhex)
                .is_some_and(|bytes| serde_json::from_slice::<EncryptedField>(&bytes).is_ok()),
            _ => true,
        }
    }

    /// Refuses a document that still carries a plaintext value in a protected field.
    ///
    /// # Errors
    ///
    /// `plaintext_refused` (the first offending field), `field_not_text`, and the rule errors.
    pub fn check_document(&self, doc: &Document) -> std::result::Result<(), HostError> {
        let doctype = doc.doctype().as_str();
        for rule in self.rules(doctype)?.iter().filter(|rule| rule.acts()) {
            match rule.path.split_once('.') {
                None => self.check_value(rule, doc.get(&rule.path))?,
                Some((table, column)) => {
                    for row in doc.rows(table) {
                        let value = row
                            .fields
                            .iter()
                            .find(|(name, _)| name.as_str() == column)
                            .map_or(&Value::Null, |(_, value)| value);
                        self.check_value(rule, value)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn check_value(&self, rule: &FieldRule, value: &Value) -> std::result::Result<(), HostError> {
        match value {
            Value::Null => Ok(()),
            Value::Text(text) if Self::is_protected_form(rule.class, text) => Ok(()),
            Value::Text(_) => Err(HostError::PlaintextRefused {
                field: rule.path.clone(),
            }),
            _ => Err(HostError::NotText {
                field: rule.path.clone(),
            }),
        }
    }

    /// Replaces every plaintext value of a protected field with its protected form and returns
    /// how many were replaced. A value already in its protected form, and a null, stay.
    ///
    /// # Errors
    ///
    /// The rule errors, `field_not_text`, `no_encryptor` and the sealing layer's. On an error the
    /// document may hold some replaced values: the caller discards it.
    pub fn seal_document(
        &self,
        doc: &mut Document,
        at: MonoTime,
    ) -> std::result::Result<usize, HostError> {
        let doctype = doc.doctype().as_str().to_owned();
        let parent = doc.id().to_string();
        let mut changed = 0;
        for rule in self.rules(&doctype)?.iter().filter(|rule| rule.acts()) {
            match rule.path.split_once('.') {
                None => {
                    let place = Place {
                        doctype: &doctype,
                        field: &rule.path,
                        document: &parent,
                    };
                    if let Some(form) = self.protect(rule, &place, doc.get(&rule.path), at)? {
                        let name = Name::new(&rule.path).map_err(|_| {
                            HostError::Sealed(SealedFieldsError::Malformed("a field name"))
                        })?;
                        doc.set(name, Value::Text(form));
                        changed += 1;
                    }
                }
                Some((table, column)) => {
                    let table_name = Name::new(table).map_err(|_| {
                        HostError::Sealed(SealedFieldsError::Malformed("a table name"))
                    })?;
                    let column_name = Name::new(column).map_err(|_| {
                        HostError::Sealed(SealedFieldsError::Malformed("a column name"))
                    })?;
                    for row in doc.rows_mut(table_name).iter_mut() {
                        let place_document = format!("{parent}/{}", row.id);
                        let place = Place {
                            doctype: &doctype,
                            field: &rule.path,
                            document: &place_document,
                        };
                        let current = row.fields.get(&column_name).unwrap_or(&Value::Null);
                        if let Some(form) = self.protect(rule, &place, current, at)? {
                            row.fields.insert(column_name.clone(), Value::Text(form));
                            changed += 1;
                        }
                    }
                }
            }
        }
        Ok(changed)
    }

    fn protect(
        &self,
        rule: &FieldRule,
        place: &Place<'_>,
        value: &Value,
        at: MonoTime,
    ) -> std::result::Result<Option<String>, HostError> {
        let text = match value {
            Value::Null => return Ok(None),
            Value::Text(text) if Self::is_protected_form(rule.class, text) => return Ok(None),
            Value::Text(text) => text,
            _ => {
                return Err(HostError::NotText {
                    field: rule.path.clone(),
                });
            }
        };
        match rule.class {
            SyncClass::Sealed => {
                let ad = SealedAd::of_field(
                    self.tenant.clone(),
                    place.doctype,
                    place.field,
                    place.document,
                );
                let sealed = self
                    .writer
                    .seal(text.as_bytes(), &ad)
                    .map_err(HostError::Sealed)?;
                Ok(Some(format!("{SEALED_PREFIX}{}", hex(&sealed.to_bytes()))))
            }
            SyncClass::Encrypted => {
                let encryptor = self
                    .encryptor
                    .as_ref()
                    .ok_or_else(|| HostError::NoEncryptor {
                        field: rule.path.clone(),
                    })?;
                let field = encryptor
                    .encrypt(&rule.group, place, text.as_bytes(), at)
                    .map_err(HostError::Sealed)?;
                let json = serde_json::to_vec(&field).map_err(|_| {
                    HostError::Sealed(SealedFieldsError::Malformed("an encrypted field"))
                })?;
                Ok(Some(format!("{ENCRYPTED_PREFIX}{}", hex(&json))))
            }
            _ => Ok(None),
        }
    }
}

// ------------------------------------------------------------------------------- the participant

/// The participant that refuses a write holding a plaintext in a protected field.
pub struct PlaintextGuard {
    fields: Arc<SealedFields>,
}

impl PlaintextGuard {
    /// A guard over `fields`.
    #[must_use]
    pub fn new(fields: Arc<SealedFields>) -> Self {
        Self { fields }
    }
}

impl<Tx: Transaction> TxParticipant<Tx> for PlaintextGuard {
    fn before_write<'a>(
        &'a self,
        _tx: &'a mut Tx,
        write: &'a WriteIntent<'a>,
    ) -> BoxFuture<'a, AppResult<()>> {
        Box::pin(async move {
            match write.new {
                Some(doc) => self
                    .fields
                    .check_document(doc)
                    .map_err(HostError::into_app_error),
                None => Ok(()),
            }
        })
    }
}

// ------------------------------------------------------------------------------- the transform seam

/// The engine's write transform: seals the protected fields of the document being written.
///
/// Idempotent (a value already in its protected form stays, so an update that carries an unchanged
/// field stores it unchanged), and its errors name codes and field names only. The monotonic time
/// an encrypted group's key lookup needs comes from the injected clock, never the wall clock.
pub struct SealTransform {
    fields: Arc<SealedFields>,
    clock: Arc<dyn Clock>,
}

impl SealTransform {
    /// A transform over `fields`, reading monotonic time from `clock`.
    #[must_use]
    pub fn new(fields: Arc<SealedFields>, clock: Arc<dyn Clock>) -> Self {
        Self { fields, clock }
    }

    fn now(&self) -> MonoTime {
        MonoTime::from_millis(u64::try_from(self.clock.monotonic().as_millis()).unwrap_or(u64::MAX))
    }
}

impl WriteTransform for SealTransform {
    fn transform(&self, _write: &WriteView<'_>, doc: &mut Document) -> AppResult<()> {
        self.fields
            .seal_document(doc, self.now())
            .map(|_| ())
            .map_err(HostError::into_app_error)
    }
}

// ------------------------------------------------------------------------------- reveal

/// Why a reveal did not return a value.
#[derive(Debug)]
pub enum RevealRefusal {
    /// The user made too many reveals this minute; nothing was asked of the hub.
    RateLimited {
        /// Whole seconds until the next minute starts.
        retry_after_secs: u64,
    },
    /// The reveal layer's own answer (offline, denied, revoked node, audit failure).
    Reveal(RevealError),
}

impl RevealRefusal {
    /// The stable code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "sealed_fields_host.rate_limited",
            Self::Reveal(error) => error.code(),
        }
    }
}

/// Where a refused-by-rate reveal is recorded (the host adapts this to the audit chain).
pub trait RateAudit: Send + Sync {
    /// One reveal was refused by the rate limit. Never carries a value.
    fn rate_limited(&self, user: &str, doctype: &str, id: &str, field: &str, at_micros: i64);
}

/// Reveal over the sync transport with a per-user rate.
pub struct Revealer {
    remote: RemoteReveal,
    per_minute: u32,
    audit: Option<Arc<dyn RateAudit>>,
    windows: Mutex<BTreeMap<String, (i64, u32)>>,
}

impl Revealer {
    /// A revealer calling `port` (`None`: offline, every call answers `reveal_offline`).
    #[must_use]
    pub fn new(
        port: Option<Arc<dyn HubPort>>,
        per_minute: u32,
        audit: Option<Arc<dyn RateAudit>>,
    ) -> Self {
        Self {
            remote: match port {
                Some(port) => RemoteReveal::online(port),
                None => RemoteReveal::offline(),
            },
            per_minute,
            audit,
            windows: Mutex::new(BTreeMap::new()),
        }
    }

    /// Whether the hub can be reached.
    #[must_use]
    pub fn is_online(&self) -> bool {
        self.remote.is_online()
    }

    /// Counts one reveal against the user's minute; `Err` when the minute is used up.
    fn admit(&self, user: &str, at_micros: i64) -> std::result::Result<(), u64> {
        let minute = at_micros.div_euclid(60_000_000);
        let into = at_micros.rem_euclid(60_000_000);
        let retry = u64::try_from((60_000_000 - into) / 1_000_000).unwrap_or(60);
        let mut windows = self.windows.lock().unwrap_or_else(|p| p.into_inner());
        if windows.len() >= MAX_TRACKED_USERS && !windows.contains_key(user) {
            windows.retain(|_, (held, _)| *held == minute);
            if windows.len() >= MAX_TRACKED_USERS {
                return Err(retry);
            }
        }
        let slot = windows.entry(user.to_owned()).or_insert((minute, 0));
        if slot.0 != minute {
            *slot = (minute, 0);
        }
        if slot.1 >= self.per_minute {
            return Err(retry);
        }
        slot.1 += 1;
        Ok(())
    }

    /// Reveals one field of one document through the hub, at the caller's UTC instant in
    /// microseconds.
    ///
    /// # Errors
    ///
    /// [`RevealRefusal::RateLimited`] before the hub is asked, otherwise the reveal layer's own
    /// answer; offline it is `sealed_fields.reveal_offline`.
    pub fn reveal(
        &self,
        principal: &Principal,
        doctype: &str,
        id: &str,
        field: &str,
        at_micros: i64,
    ) -> std::result::Result<Revealed, RevealRefusal> {
        if let Err(retry_after_secs) = self.admit(&principal.user, at_micros) {
            if let Some(audit) = &self.audit {
                audit.rate_limited(&principal.user, doctype, id, field, at_micros);
            }
            return Err(RevealRefusal::RateLimited { retry_after_secs });
        }
        self.remote
            .reveal(principal, doctype, id, field, at_micros)
            .map_err(RevealRefusal::Reveal)
    }
}

// ------------------------------------------------------------------------------- registration

/// Registers the module when `[services.sealed_fields]` has `enabled = true`.
///
/// Needs the ports `SyncClassTable` (from `protected_keys`), `dyn ProtectionSource` (the served
/// metadata's facts) and `HubPublicKey` (the key the hub's sealed-field box opens with). Optional:
/// `dyn GroupEncryptor` (without it an `encrypted` group is refused at start) and `dyn HubPort`
/// (without it reveal is offline). With the module on and a required port missing, the start is
/// refused. Provides `SealedFields`, `Revealer` and the participant
/// (`dyn TxParticipant<PgTransaction>`) for the participants module to install.
///
/// # Errors
///
/// [`Error`] for invalid settings, a missing port, or a rule the table cannot satisfy.
pub fn register(ctx: &mut SiteCtx<'_>) -> Result<()> {
    let settings = ctx.settings_or_default::<Settings>(NAME)?;
    if !settings.enabled {
        return Ok(());
    }
    settings.validate()?;
    if !ctx.assembling() {
        return Ok(());
    }
    let table = ctx.require_port::<SyncClassTable>()?;
    let source = ctx.require_port::<dyn ProtectionSource>()?;
    let hub = ctx.require_port::<HubPublicKey>()?;
    let tenant = TenantId::new(ctx.label()).map_err(|_| {
        Error(format!(
            "sealed_fields_host.settings_invalid: `{}` is not a usable tenant id",
            ctx.label()
        ))
    })?;
    let encryptor = ctx.port::<dyn GroupEncryptor>();
    let encrypted_groups = table
        .rows()
        .iter()
        .any(|(_, class)| *class == SyncClass::Encrypted);
    if encrypted_groups && encryptor.is_none() {
        return Err(Error(
            "sealed_fields_host.no_encryptor: a group is `encrypted` and no module provides the node's group keys"
                .to_owned(),
        ));
    }
    let fields = Arc::new(SealedFields::new(tenant, source, table, *hub, encryptor));
    let guard: Arc<dyn TxParticipant<PgTransaction>> =
        Arc::new(PlaintextGuard::new(fields.clone()));
    let revealer = Revealer::new(ctx.port::<dyn HubPort>(), settings.reveal_per_minute, None);
    ctx.provide::<SealedFields>(fields.clone())?;
    ctx.provide::<Revealer>(Arc::new(revealer))?;
    ctx.provide::<dyn TxParticipant<PgTransaction>>(guard)?;
    ctx.provide::<dyn WriteTransform>(Arc::new(SealTransform::new(
        fields.clone(),
        Arc::clone(ctx.clock()),
    )))?;
    let canary = fields;
    ctx.add(Idle::new(NAME, Level::Required, move || {
        let canary = canary.clone();
        Box::pin(async move {
            // A box to the hub's key must work before the site serves.
            canary
                .writer
                .seal(b"canary", &SealedAd::unbound())
                .map(|_| ())
                .map_err(|e| e.code().to_owned())
        })
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use spot_app_api::document::ChildRow;
    use spot_app_api::prelude::RecordId;
    use spot_field_guard::StaticProtection;
    use spot_permissions::{FieldFacts, GroupName as PermGroup, Tag};
    use spot_protected_keys::KeyEpoch;
    use spot_sealed_fields::{LocalHub, Sealed};

    const SECRET: &str = "SALARY-4815162342";
    const IBAN: &str = "PK36SCBL0000001123456702";
    const CARD: &str = "4111111111111111";

    fn group(name: &str) -> GroupName {
        GroupName::new(name).unwrap()
    }

    fn tenant() -> TenantId {
        TenantId::new("acme").unwrap()
    }

    fn name(text: &str) -> Name {
        Name::new(text).unwrap()
    }

    fn facts(path: &str, groups: &[&str], tags: &[&str]) -> FieldFacts {
        let mut f = FieldFacts::plain(path);
        f.groups = groups.iter().map(|g| PermGroup((*g).to_owned())).collect();
        f.tags = tags.iter().map(|t| Tag((*t).to_owned())).collect();
        f
    }

    fn prot(doctype: &str, fields: Vec<FieldFacts>, tables: &[(&str, &str)]) -> DocProtection {
        DocProtection {
            name: doctype.to_owned(),
            fields,
            tables: tables
                .iter()
                .map(|(a, b)| ((*a).into(), (*b).into()))
                .collect(),
            trace_groups: Vec::new(),
        }
    }

    fn source() -> Arc<dyn ProtectionSource> {
        let employee = vec![
            facts("salary", &["hr"], &[]),
            facts("card", &["pci"], &[]),
            facts("blend", &["pci", "hr"], &[]),
            facts("note", &["open"], &[]),
        ];
        Arc::new(StaticProtection::new(vec![
            prot("employee", employee, &[("lines", "employee_line")]),
            prot("employee_line", vec![facts("account", &["bank"], &[])], &[]),
        ]))
    }

    fn table() -> Arc<SyncClassTable> {
        let mut t = SyncClassTable::new();
        for (g, class) in [
            ("hr", SyncClass::Sealed),
            ("bank", SyncClass::Sealed),
            ("pci", SyncClass::Encrypted),
            ("open", SyncClass::Everywhere),
        ] {
            t.set(group(g), class);
        }
        Arc::new(t)
    }

    /// Not a cipher: reverses the bytes so the stored form is not the plaintext.
    struct FakeEncryptor;

    impl GroupEncryptor for FakeEncryptor {
        fn encrypt(
            &self,
            group: &GroupName,
            _place: &Place<'_>,
            plaintext: &[u8],
            _at: MonoTime,
        ) -> std::result::Result<EncryptedField, SealedFieldsError> {
            let bytes = plaintext.iter().rev().copied().collect();
            Ok(EncryptedField::new(group.clone(), KeyEpoch::FIRST, bytes))
        }
    }

    fn fields(hub: &LocalHub, encryptor: bool) -> SealedFields {
        let enc: Option<Arc<dyn GroupEncryptor>> = encryptor.then(|| Arc::new(FakeEncryptor) as _);
        SealedFields::new(tenant(), source(), table(), hub.public_key(), enc)
    }

    fn employee() -> Document {
        let mut doc = Document::new(name("employee"), RecordId::new_v7());
        for (field, value) in [("salary", SECRET), ("card", CARD), ("note", "open note")] {
            doc.set(name(field), Value::Text(value.to_owned()));
        }
        for _ in 0..2 {
            let row = ChildRow {
                id: RecordId::new_v7(),
                fields: [(name("account"), Value::Text(IBAN.to_owned()))].into(),
            };
            doc.rows_mut(name("lines")).push(row);
        }
        doc
    }

    fn open_sealed(text: &str) -> Sealed {
        Sealed::from_bytes(&unhex(text.strip_prefix(SEALED_PREFIX).unwrap()).unwrap()).unwrap()
    }

    fn code<T>(r: std::result::Result<T, HostError>) -> &'static str {
        r.err().map_or("ok", |e| e.code())
    }

    #[test]
    fn the_strictest_class_wins_and_an_unset_tagged_group_fails_closed() {
        let hub = LocalHub::generate(tenant()).unwrap();
        let rules = fields(&hub, true).rules("employee").unwrap();
        let class_of = |path: &str| rules.iter().find(|r| r.path == path).map(|r| r.class);
        assert_eq!(class_of("salary"), Some(SyncClass::Sealed));
        assert_eq!(class_of("card"), Some(SyncClass::Encrypted));
        assert_eq!(class_of("blend"), Some(SyncClass::Sealed));
        assert_eq!(class_of("lines.account"), Some(SyncClass::Sealed));
        assert_eq!(class_of("note"), Some(SyncClass::Everywhere));
        let tagged = vec![facts("salary", &["payroll"], &["secret"])];
        let unset = Arc::new(StaticProtection::new(vec![prot("employee", tagged, &[])]));
        let empty = Arc::new(SyncClassTable::new());
        let strict = SealedFields::new(tenant(), unset, empty, hub.public_key(), None);
        assert_eq!(code(strict.rules("employee")), "sealed_fields.class_unset");
        assert_eq!(
            code(fields(&hub, false).rules("ghost")),
            "sealed_fields_host.unknown_doctype"
        );
    }

    #[test]
    fn values_are_boxes_only_the_hub_opens_and_each_is_bound_to_its_record_and_row() {
        let hub = LocalHub::generate(tenant()).unwrap();
        let sealer = fields(&hub, true);
        let mut doc = employee();
        // salary, card, two account cells.
        assert_eq!(sealer.seal_document(&mut doc, MonoTime::ZERO).unwrap(), 4);
        let stored = format!("{doc:?}");
        assert!(
            [SECRET, IBAN, CARD].iter().all(|s| !stored.contains(s)),
            "{stored}"
        );
        assert_eq!(doc.get("note"), &Value::Text("open note".to_owned()));
        let Value::Text(salary) = doc.get("salary").clone() else {
            panic!("text")
        };
        let sealed = open_sealed(&salary);
        let id = doc.id().to_string();
        let ad = |doc_id: &str, t: TenantId| SealedAd::of_field(t, "employee", "salary", doc_id);
        let opened = hub.unseal(&sealed, &ad(&id, tenant())).unwrap();
        assert_eq!(opened.expose(), SECRET.as_bytes());
        let moved = ad(&RecordId::new_v7().to_string(), tenant());
        assert_eq!(
            hub.unseal(&sealed, &moved).unwrap_err().code(),
            "sealed_fields.tampered"
        );
        assert!(
            hub.unseal(&sealed, &ad(&id, TenantId::new("other").unwrap()))
                .is_err()
        );

        // A child cell opens for its own row only.
        let rows = doc.rows("lines").to_vec();
        let Some(Value::Text(cell)) = rows[0].fields.get(&name("account")) else {
            panic!("text")
        };
        let cell = open_sealed(cell);
        let row_ad = |r: &ChildRow| {
            SealedAd::of_field(
                tenant(),
                "employee",
                "lines.account",
                format!("{id}/{}", r.id),
            )
        };
        assert_eq!(
            hub.unseal(&cell, &row_ad(&rows[0])).unwrap().expose(),
            IBAN.as_bytes()
        );
        assert!(hub.unseal(&cell, &row_ad(&rows[1])).is_err());

        // A second pass changes nothing and the protected document passes the check.
        assert_eq!(sealer.seal_document(&mut doc, MonoTime::ZERO).unwrap(), 0);
        sealer.check_document(&doc).unwrap();
        let Value::Text(card) = doc.get("card").clone() else {
            panic!("text")
        };
        assert!(card.starts_with(ENCRYPTED_PREFIX) && !card.contains(CARD));
    }

    #[test]
    fn a_plaintext_or_a_typed_value_is_refused_without_quoting_it_and_keys_are_needed() {
        let hub = LocalHub::generate(tenant()).unwrap();
        let sealer = fields(&hub, true);
        let error = sealer.check_document(&employee()).unwrap_err();
        assert_eq!(error.code(), "sealed_fields_host.plaintext_refused");
        let texts = [
            error.to_string(),
            format!("{error:?}"),
            format!("{:?}", error.into_app_error()),
        ];
        assert!(
            texts
                .iter()
                .all(|t| [SECRET, IBAN, CARD].iter().all(|s| !t.contains(s)))
        );
        let mut doc = Document::new(name("employee"), RecordId::new_v7());
        sealer.check_document(&doc).unwrap();
        // Looks like the protected form but is not, and a number cannot be stored.
        doc.set(name("salary"), Value::Text(format!("{SEALED_PREFIX}zz")));
        assert_eq!(
            code(sealer.check_document(&doc)),
            "sealed_fields_host.plaintext_refused"
        );
        doc.set(name("salary"), Value::Int(5000));
        let at = MonoTime::ZERO;
        assert_eq!(
            code(sealer.seal_document(&mut doc, at)),
            "sealed_fields_host.field_not_text"
        );
        // An encrypted group with no node keys is refused.
        let no_keys = fields(&hub, false);
        assert_eq!(
            code(no_keys.seal_document(&mut employee(), at)),
            "sealed_fields_host.no_encryptor"
        );
    }

    #[test]
    fn reveal_is_online_only_and_rate_limited_per_user_and_settings_are_strict() {
        let revealer = Revealer::new(None, 2, None);
        assert!(!revealer.is_online());
        let (alice, bob) = (
            Principal::with_roles("alice", &[]),
            Principal::with_roles("bob", &[]),
        );
        let minute = 60_000_000_i64;
        let ask = |who: &Principal, at: i64| {
            revealer
                .reveal(who, "employee", "id-1", "salary", at)
                .unwrap_err()
        };
        let offline = "sealed_fields.reveal_offline";
        assert_eq!(ask(&alice, 5 * minute).code(), offline);
        assert_eq!(ask(&alice, 5 * minute).code(), offline);
        // The third call in the same minute is refused before the hub is asked.
        let refusal = ask(&alice, 5 * minute + 1);
        assert_eq!(refusal.code(), "sealed_fields_host.rate_limited");
        assert!(
            matches!(refusal, RevealRefusal::RateLimited { retry_after_secs } if retry_after_secs <= 60)
        );
        // Another user and the next minute have their own count.
        assert_eq!(ask(&bob, 5 * minute).code(), offline);
        assert_eq!(ask(&alice, 6 * minute).code(), offline);
        let bad = |rate| {
            Settings {
                enabled: true,
                reveal_per_minute: rate,
            }
            .validate()
            .is_err()
        };
        assert!(toml::from_str::<Settings>("enabled = true\nreveal = 1").is_err());
        assert!(bad(0) && bad(MAX_REVEAL_PER_MINUTE + 1) && !Settings::default().enabled);
        assert_eq!(unhex(&hex(&[0, 1, 255])), Some(vec![0, 1, 255]));
    }

    fn invocation() -> spot_app_api::context::Invocation {
        spot_app_api::context::Invocation {
            site: spot_app_api::prelude::SiteId::new("acme").unwrap(),
            actor: spot_app_api::context::Actor::System {
                component: name("sealed_fields_tests"),
            },
            command_id: None,
            language: None,
            now: time::OffsetDateTime::UNIX_EPOCH,
        }
    }

    fn transform(hub: &LocalHub, encryptor: bool) -> SealTransform {
        SealTransform::new(
            Arc::new(fields(hub, encryptor)),
            Arc::new(spot_core::ManualClock::new(
                time::OffsetDateTime::UNIX_EPOCH,
            )),
        )
    }

    #[test]
    fn the_engine_transform_seals_the_document_and_a_second_write_changes_nothing() {
        let hub = LocalHub::generate(tenant()).unwrap();
        let seal = transform(&hub, true);
        let inv = invocation();
        let view = WriteView {
            doctype: "employee",
            operation: spot_app_api::Operation::Create,
            invocation: &inv,
        };
        let mut doc = employee();
        seal.transform(&view, &mut doc).unwrap();
        let stored = format!("{doc:?}");
        assert!([SECRET, IBAN, CARD].iter().all(|s| !stored.contains(s)));
        let Value::Text(salary) = doc.get("salary").clone() else {
            panic!("text")
        };
        assert!(salary.starts_with(SEALED_PREFIX));
        // A value read back and saved again is already in its stored form.
        let before = format!("{doc:?}");
        seal.transform(&view, &mut doc).unwrap();
        assert_eq!(format!("{doc:?}"), before);
    }

    #[test]
    fn a_transform_that_cannot_seal_fails_the_write_without_quoting_a_value() {
        let hub = LocalHub::generate(tenant()).unwrap();
        // An encrypted group with no node keys, and a typed value in a protected field.
        let no_keys = transform(&hub, false);
        let inv = invocation();
        let view = WriteView {
            doctype: "employee",
            operation: spot_app_api::Operation::Update,
            invocation: &inv,
        };
        let mut doc = employee();
        let err = no_keys.transform(&view, &mut doc).unwrap_err();
        assert!(!format!("{err:?}").contains(CARD) && !format!("{err:?}").contains(SECRET));
        let mut typed = employee();
        typed.set(name("salary"), Value::Bool(true));
        let err = transform(&hub, true)
            .transform(&view, &mut typed)
            .unwrap_err();
        assert!(!format!("{err:?}").contains(SECRET));
    }
}
