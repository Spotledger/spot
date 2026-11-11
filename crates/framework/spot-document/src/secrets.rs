//! The capability and the vault behind a `secret` field (WP-SEC-03.T3; ADR-068 section 2,
//! ADR-070 section 12).
//!
//! A credential is the one value that must cross from a request into storage without ever
//! being a part of a document. The engine therefore holds it in a **separate path**:
//!
//! - a [`SecretAccess`] is the only key to that path. It cannot be built outside this crate
//!   except by [`Engine::secret_access`](crate::engine::Engine::secret_access), it is not
//!   `Clone`, `Default` or serde, and no hook, controller, script or API handler is ever handed
//!   one;
//! - a [`SecretVault`] seals, opens, re-wraps and blind-indexes values. [`SecretBox`] is the
//!   production vault;
//! - the value written is bound to its **site, DocType, record and field** (the associated data
//!   of the seal), and the blind index of a lookup field covers the **site** too (see
//!   [`index_input`]), so a copied blob does not open in another place and an index computed in
//!   one tenant never matches in another, even when two tenants share a key-encryption key.
//!
//! # What a document shows
//!
//! Nothing but whether the secret is set. Events, audit entries, error messages, `Debug` output
//! and the secret audit log of the engine name a secret by its field and show `<set>` or
//! `<unset>`; they never hold the text, its sealed bytes or its blind index.
//!
//! # A caller with no capability cannot compile
//!
//! ```compile_fail
//! // `SecretAccess` has private fields: it cannot be built by hand.
//! use spot_document::secrets::{SecretAccess, SecretCaller};
//! let _ = SecretAccess { caller: SecretCaller::LoginService };
//! ```
//!
//! ```compile_fail
//! // It is not `Clone`: a capability cannot be copied and handed on.
//! fn needs_clone<T: Clone>() {}
//! needs_clone::<spot_document::secrets::SecretAccess>();
//! ```
//!
//! ```compile_fail
//! // It is not `Default`: there is no empty capability.
//! fn needs_default<T: Default>() {}
//! needs_default::<spot_document::secrets::SecretAccess>();
//! ```
//!
//! ```compile_fail
//! // It is not serde: it never travels.
//! fn to_json(access: &spot_document::secrets::SecretAccess) -> String {
//!     serde_json::to_string(access).unwrap_or_default()
//! }
//! ```

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use spot_app_api::context::Invocation;
use spot_app_api::prelude::{OffsetDateTime, RecordId};
use spot_secrets::{
    BlindIndex, KeyProvider, KeyRef, ProviderFuture, Sealed, SecretBox, SecretBytes, SecretError,
    SecretString, WrappedKey,
};
use zeroize::Zeroizing;

/// The error codes of the secret path (see `catalogue/secretfield.toml`).
pub mod codes {
    /// The engine has no [`SecretVault`](super::SecretVault).
    pub const NOT_CONFIGURED: &str = "secretfield.not_configured";
    /// The DocType has no `secret` field of that name.
    pub const UNKNOWN_FIELD: &str = "secretfield.unknown_field";
    /// A lookup was asked of a secret field that does not declare `lookup = true`.
    pub const NOT_LOOKUP: &str = "secretfield.not_lookup";
    /// Secrets are held by documents, not by the rows of a child table.
    pub const UNSUPPORTED: &str = "secretfield.unsupported_doctype";
    /// An empty value is never a credential.
    pub const EMPTY_VALUE: &str = "secretfield.empty_value";
    /// Sealing the value failed.
    pub const SEAL_FAILED: &str = "secretfield.seal_failed";
    /// Opening the stored value failed (tampered, wrong place, key retired).
    pub const OPEN_FAILED: &str = "secretfield.open_failed";
    /// Re-wrapping the stored value failed.
    pub const REWRAP_FAILED: &str = "secretfield.rewrap_failed";
    /// Deriving the blind index failed.
    pub const INDEX_FAILED: &str = "secretfield.index_failed";
    /// Another record already holds this value in a unique lookup field.
    pub const DUPLICATE: &str = "secretfield.duplicate";
    /// More than one record matched a lookup in a field that is not unique.
    pub const AMBIGUOUS: &str = "secretfield.ambiguous";
    /// The field's definition cannot be read as a secret definition.
    pub const INVALID_DEFINITION: &str = "secretfield.invalid_definition";
    /// A rewrap batch size is out of range.
    pub const INVALID_BATCH: &str = "secretfield.invalid_batch";
    /// The stored secret columns are inconsistent with each other.
    pub const INCONSISTENT: &str = "secretfield.inconsistent";
    /// The audit row of a secret operation could not be written, so the operation did not
    /// happen (or, for a refused one, its refusal could not be recorded).
    pub const AUDIT_FAILED: &str = "secretfield.audit_failed";
}

/// The boxed `Send` future of a [`SecretVault`] call, so the trait stays object-safe.
pub type VaultFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SecretError>> + Send + 'a>>;

/// What the engine needs from envelope encryption: seal, open, re-wrap and blind-index, on
/// byte forms that go straight into the `<field>__sealed` and `<field>__bidx` columns.
///
/// [`SecretBox`] implements it for any [`KeyProvider`]; a test double may implement it to
/// inject failures.
pub trait SecretVault: Send + Sync + 'static {
    /// Seals `plain` bound to `aad`; returns the stored form ([`Sealed::to_bytes`]).
    fn seal<'a>(&'a self, plain: &'a SecretString, aad: &'a [u8]) -> VaultFuture<'a, Vec<u8>>;

    /// Opens a stored form; fails with `secrets.tampered` unless `aad` is the one it was sealed
    /// with.
    fn open<'a>(&'a self, sealed: &'a [u8], aad: &'a [u8]) -> VaultFuture<'a, SecretString>;

    /// Re-wraps the data key of a stored form under the current key, without changing the
    /// ciphertext.
    fn rewrap<'a>(&'a self, sealed: &'a [u8], aad: &'a [u8]) -> VaultFuture<'a, Vec<u8>>;

    /// Whether a stored form is under a key version other than the current one.
    ///
    /// # Errors
    ///
    /// `secrets.malformed` for bytes that are not a sealed value.
    fn needs_rewrap(&self, sealed: &[u8]) -> Result<bool, SecretError>;

    /// The 33-byte blind index of `input` in `scope` (`"<doctype>.<field>"`) under the current
    /// key version.
    fn blind_index<'a>(&'a self, scope: &'a str, input: &'a [u8]) -> VaultFuture<'a, Vec<u8>>;

    /// Whether a stored blind index was derived under a key version other than the current one.
    ///
    /// # Errors
    ///
    /// `secrets.malformed` or `secrets.unknown_key_version` for bytes that are not an index.
    fn needs_reindex(&self, index: &[u8]) -> Result<bool, SecretError>;

    /// The key version new values are sealed and indexed under, or 0 when the vault does not
    /// say. The engine uses it to tell whether a rewrap sweep finished under the current key.
    fn current_key_version(&self) -> u32 {
        0
    }

    /// The blind index of `input` in `scope` under the key version **before** the current one,
    /// or `None` when there is none (the current version is the first, or the previous key is
    /// retired). It is for the lookup window of a rotation only: while a rewrap is still moving
    /// the stored indexes onto the current key, a lookup also asks for the previous one.
    ///
    /// A vault that does not support it answers `None`, which leaves lookups on the current
    /// version alone.
    fn blind_index_previous<'a>(
        &'a self,
        scope: &'a str,
        input: &'a [u8],
    ) -> VaultFuture<'a, Option<Vec<u8>>> {
        let _ = (scope, input);
        Box::pin(async { Ok(None) })
    }
}

/// A key provider pinned to one version: the same keys, but `current()` answers `key`. It lets
/// [`SecretBox`] derive an index under the previous version without any change to its crate.
struct AtVersion<'a, P: KeyProvider> {
    inner: &'a P,
    key: KeyRef,
}

impl<P: KeyProvider> KeyProvider for AtVersion<'_, P> {
    fn current(&self) -> KeyRef {
        self.key.clone()
    }

    fn wrap<'a>(&'a self, data_key: &'a [u8], aad: &'a [u8]) -> ProviderFuture<'a, WrappedKey> {
        self.inner.wrap(data_key, aad)
    }

    fn unwrap<'a>(
        &'a self,
        key: &'a KeyRef,
        wrapped: &'a WrappedKey,
        aad: &'a [u8],
    ) -> ProviderFuture<'a, SecretBytes> {
        self.inner.unwrap(key, wrapped, aad)
    }

    fn index_key<'a>(&'a self, key: &'a KeyRef) -> ProviderFuture<'a, SecretBytes> {
        self.inner.index_key(key)
    }
}

impl<P: KeyProvider + 'static> SecretVault for SecretBox<P> {
    fn seal<'a>(&'a self, plain: &'a SecretString, aad: &'a [u8]) -> VaultFuture<'a, Vec<u8>> {
        Box::pin(async move { Ok(SecretBox::seal(self, plain, aad).await?.to_bytes()) })
    }

    fn open<'a>(&'a self, sealed: &'a [u8], aad: &'a [u8]) -> VaultFuture<'a, SecretString> {
        Box::pin(async move {
            let sealed = Sealed::from_bytes(sealed)?;
            SecretBox::open(self, &sealed, aad).await
        })
    }

    fn rewrap<'a>(&'a self, sealed: &'a [u8], aad: &'a [u8]) -> VaultFuture<'a, Vec<u8>> {
        Box::pin(async move {
            let sealed = Sealed::from_bytes(sealed)?;
            Ok(SecretBox::rewrap(self, &sealed, aad).await?.to_bytes())
        })
    }

    fn needs_rewrap(&self, sealed: &[u8]) -> Result<bool, SecretError> {
        Ok(SecretBox::needs_rewrap(self, &Sealed::from_bytes(sealed)?))
    }

    fn blind_index<'a>(&'a self, scope: &'a str, input: &'a [u8]) -> VaultFuture<'a, Vec<u8>> {
        Box::pin(async move {
            Ok(SecretBox::blind_index(self, scope, input)
                .await?
                .to_bytes()
                .to_vec())
        })
    }

    fn needs_reindex(&self, index: &[u8]) -> Result<bool, SecretError> {
        Ok(SecretBox::needs_reindex(
            self,
            &BlindIndex::from_bytes(index)?,
        ))
    }

    fn current_key_version(&self) -> u32 {
        self.provider().current().version
    }

    fn blind_index_previous<'a>(
        &'a self,
        scope: &'a str,
        input: &'a [u8],
    ) -> VaultFuture<'a, Option<Vec<u8>>> {
        Box::pin(async move {
            let current = self.provider().current();
            let Some(version) = current.version.checked_sub(1).filter(|v| *v >= 1) else {
                return Ok(None);
            };
            let pinned = SecretBox::new(AtVersion {
                inner: self.provider(),
                key: KeyRef {
                    id: current.id,
                    version,
                },
            });
            match pinned.blind_index(scope, input).await {
                Ok(index) => Ok(Some(index.to_bytes().to_vec())),
                // The previous key is gone (retired): no stored index can use it.
                Err(SecretError::UnknownKeyVersion { .. } | SecretError::IndexKey(_)) => Ok(None),
                Err(e) => Err(e),
            }
        })
    }
}

/// Who asked for a [`SecretAccess`]. It is a label for the audit log, not a permission: holding
/// the capability is the permission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretCaller {
    /// The sign-in service: passwords, one-time secrets, session and API key digests.
    LoginService,
    /// The bootstrap that creates a site's first records.
    Seed,
    /// A test. Only compiled with the `testing` feature.
    #[cfg(feature = "testing")]
    Test,
}

impl SecretCaller {
    /// The stable name used in the audit log.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LoginService => "login_service",
            Self::Seed => "seed",
            #[cfg(feature = "testing")]
            Self::Test => "test",
        }
    }

    /// The caller a stable name stands for; `None` for a name this build does not know.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "login_service" => Some(Self::LoginService),
            "seed" => Some(Self::Seed),
            #[cfg(feature = "testing")]
            "test" => Some(Self::Test),
            _ => None,
        }
    }
}

/// The capability to read, write and look up secret values.
///
/// Built only by [`Engine::secret_access`](crate::engine::Engine::secret_access). It binds the
/// [`Invocation`] it was made for, so the event of a write carries the real actor and time, and
/// the site of that invocation is part of every value's associated data.
///
/// It is not `Clone`, not `Default` and not serde (see the module's `compile_fail` examples),
/// and the engine never passes one to a hook, controller, script or API handler.
pub struct SecretAccess {
    caller: SecretCaller,
    inv: Invocation,
}

impl SecretAccess {
    pub(crate) fn new(caller: SecretCaller, inv: &Invocation) -> Self {
        Self {
            caller,
            inv: inv.clone(),
        }
    }

    /// Who asked for it.
    pub fn caller(&self) -> SecretCaller {
        self.caller
    }

    pub(crate) fn invocation(&self) -> &Invocation {
        &self.inv
    }
}

impl fmt::Debug for SecretAccess {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SecretAccess")
            .field("caller", &self.caller)
            .finish_non_exhaustive()
    }
}

/// What a rewrap call does: at most `size` records, those after `after`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RewrapBatch {
    /// The most records to examine, from 1 to [`RewrapBatch::MAX_SIZE`].
    pub size: u32,
    /// Resume after this record (the previous report's [`RewrapReport::next`]); `None` starts
    /// from the first record.
    pub after: Option<RecordId>,
}

impl RewrapBatch {
    /// The largest batch: one transaction holds a row lock for each record it examines.
    pub const MAX_SIZE: u32 = 500;

    /// A batch of `size` records from the start.
    pub fn first(size: u32) -> Self {
        Self { size, after: None }
    }
}

/// What one rewrap batch did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RewrapReport {
    /// Records examined.
    pub examined: u32,
    /// Sealed values whose key was replaced by the current one.
    pub rewrapped: u32,
    /// Blind indexes recomputed under the current key.
    pub reindexed: u32,
    /// Values that could not be rewrapped (tampered, key retired); they are left untouched and
    /// each has an entry in the secret audit log.
    pub failed: u32,
    /// Where the next batch starts, or `None` when the DocType has no more records.
    pub next: Option<RecordId>,
}

/// What a secret operation was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretOp {
    /// `set_secret` or the secrets of `create_with_secrets`.
    Set,
    /// `open_secret`.
    Open,
    /// `find_by_secret`.
    Find,
    /// One `rewrap_secrets` batch (or one value of it that failed).
    Rewrap,
    /// `secret_is_set`.
    IsSet,
}

impl SecretOp {
    /// The stable name used in the journal.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Set => "set",
            Self::Open => "open",
            Self::Find => "find",
            Self::Rewrap => "rewrap",
            Self::IsSet => "is_set",
        }
    }

    /// The operation a stable name stands for.
    pub fn from_name(name: &str) -> Option<Self> {
        [Self::Set, Self::Open, Self::Find, Self::Rewrap, Self::IsSet]
            .into_iter()
            .find(|op| op.as_str() == name)
    }
}

/// One entry of the engine's secret audit log: who did what to which secret, and how it ended.
/// It names the field and shows `<set>` / `<unset>` state, never a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretAuditRecord {
    /// The operation.
    pub op: SecretOp,
    /// Who held the capability.
    pub caller: SecretCaller,
    /// The actor of the invocation, as `user:<id>` (with ` via <id>` when impersonated) or
    /// `system:<component>`.
    pub actor: String,
    /// The site of the invocation.
    pub site: String,
    /// The DocType.
    pub doctype: String,
    /// The field, or `*` for a whole batch.
    pub field: String,
    /// The record, when the operation named one.
    pub id: Option<RecordId>,
    /// The error code it ended with, or `None` when it succeeded.
    pub error: Option<String>,
    /// The state of the secret afterwards, for operations that change it: `<set>` or `<unset>`.
    pub state: &'static str,
    /// When (the invocation's time).
    pub at: OffsetDateTime,
}

/// The text shown in place of a set secret.
pub const SET: &str = "<set>";
/// The text shown in place of an unset secret.
pub const UNSET: &str = "<unset>";
/// The state of a call that failed: nothing is known to have changed, so none is recorded.
pub const NOT_RECORDED: &str = "<none>";

/// The associated data a value is sealed with: site, DocType, record and field, so a blob copied
/// to another place (or another tenant) fails to open.
pub(crate) fn aad(site: &str, doctype: &str, id: RecordId, field: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"spot-document/secret/v1");
    for part in [site, doctype, &id.to_string(), field] {
        // Names, site ids and record ids hold no NUL, so this framing is unambiguous.
        out.push(0);
        out.extend_from_slice(part.as_bytes());
    }
    out
}

/// The bytes the blind index is computed over: the **site** (length-prefixed) and then the
/// value.
///
/// The blind-index scope of the secrets crate is `"<doctype>.<field>"` and has no site, so two
/// tenants that share a key-encryption key would otherwise get the same index for the same
/// value. Putting the site into the MAC input makes an index tenant-specific whatever
/// the key arrangement.
pub(crate) fn index_input(site: &str, value: &[u8]) -> Zeroizing<Vec<u8>> {
    let mut out = Zeroizing::new(Vec::with_capacity(4 + site.len() + value.len()));
    let len = u32::try_from(site.len()).unwrap_or(u32::MAX);
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(site.as_bytes());
    out.extend_from_slice(value);
    out
}

/// Whether `query` names a secret's engine-managed column (`<field>__sealed` or
/// `<field>__bidx`) anywhere: selected, filtered, grouped, joined, ordered or aggregated.
///
/// A repository query (a step's, a hook's) never reaches those columns: the sealed value and
/// its blind index are for the secret path alone. The check is by name, because the permission
/// snapshot knows no field of that name and would treat it as an ordinary readable column.
pub(crate) fn query_names_secret_column(query: &spot_storage_contract::Query) -> bool {
    fn reserved(field: &str) -> bool {
        field.ends_with(spot_meta::SEALED_SUFFIX) || field.ends_with(spot_meta::BLIND_INDEX_SUFFIX)
    }
    fn expr(e: &spot_storage_contract::Expr) -> bool {
        use spot_storage_contract::Expr;
        match e {
            Expr::Field(f) => reserved(f.field.as_str()),
            Expr::Value(_) => false,
            Expr::Cmp(a, _, b) | Expr::Arith(a, _, b) => expr(a) || expr(b),
            Expr::And(v) | Expr::Or(v) | Expr::Coalesce(v) => v.iter().any(expr),
            Expr::Not(e)
            | Expr::IsNull(e)
            | Expr::In(e, _)
            | Expr::Text(e, _)
            | Expr::Geo(e, _)
            | Expr::Range(e, _) => expr(e),
            Expr::Search(t) | Expr::SearchRank(t) => t.fields.iter().any(|f| reserved(f.as_str())),
            Expr::Agg { arg, .. } => arg.as_deref().is_some_and(expr),
        }
    }
    query.select.iter().any(|s| expr(&s.expr))
        || query.joins.iter().any(|j| expr(&j.on))
        || query.filter.as_ref().is_some_and(expr)
        || query.group_by.iter().any(expr)
        || query.having.as_ref().is_some_and(expr)
        || query.order_by.iter().any(|o| expr(&o.expr))
}
