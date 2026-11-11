//! Who is calling, whether they may read or upload to a destination, and the quota hook.
//!
//! Three seams, all object-safe and injected so the host wires them to the real identity and
//! permission engines and tests use small fakes:
//!
//! - [`CallerSource`] turns the request's credential into a [`Caller`] (a user or a guest);
//! - [`AttachAccess`] is the **one** permission question: may this caller read (or write) the
//!   document a file is attached to, and, when the file hangs off a protected field, that
//!   field? The host implements it over the permission engine (ADR-070 section 12); the
//!   [`TableAttachAccess`] fake here loads grants from a TOML table;
//! - [`QuotaCheck`] is the per-site and per-user quota hook; [`UnlimitedQuota`] is the default.

use std::collections::BTreeMap;

use serde::Deserialize;
use spot_core::SiteId;
use spot_storage_contract::RecordId;

use crate::BoxFuture;
use crate::ctx::FileCtx;
use crate::error::FileServiceError;
use crate::record::AttachTarget;

/// The caller of a file operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caller {
    /// A signed-in user.
    User(RecordId),
    /// No signed-in user (an unauthenticated or anonymous request).
    Guest,
}

impl Caller {
    /// The user's id, when signed in.
    pub fn user_id(self) -> Option<RecordId> {
        match self {
            Caller::User(id) => Some(id),
            Caller::Guest => None,
        }
    }

    /// Whether this is a guest.
    pub fn is_guest(self) -> bool {
        matches!(self, Caller::Guest)
    }
}

/// Turns the request's credential into a [`Caller`].
pub trait CallerSource: Send + Sync + 'static {
    /// The caller of `ctx`. An unknown or absent credential is a guest, never an error, so a
    /// request never reveals whether a credential is valid.
    fn caller<'a>(&'a self, ctx: &'a FileCtx) -> BoxFuture<'a, Result<Caller, FileServiceError>>;
}

/// The permission question for an attached file.
pub trait AttachAccess: Send + Sync + 'static {
    /// Whether `caller` may read the document `target` names, and, when `target.field` is
    /// `Some`, that protected field (a protected field is closed by default).
    fn may_read<'a>(
        &'a self,
        caller: &'a Caller,
        target: &'a AttachTarget,
    ) -> BoxFuture<'a, Result<bool, FileServiceError>>;

    /// Whether `caller` may write the document `target` names, and, when `target.field` is
    /// `Some`, that protected field.
    fn may_upload<'a>(
        &'a self,
        caller: &'a Caller,
        target: &'a AttachTarget,
    ) -> BoxFuture<'a, Result<bool, FileServiceError>>;
}

/// The quota hook: whether `add_bytes` more may be stored.
pub trait QuotaCheck: Send + Sync + 'static {
    /// Checks the site's and the user's quota for `add_bytes` more bytes. A refusal is
    /// [`FileServiceError::QuotaExceeded`]; the default [`UnlimitedQuota`] always allows.
    fn check<'a>(
        &'a self,
        site: &'a SiteId,
        user: &'a RecordId,
        add_bytes: u64,
    ) -> BoxFuture<'a, Result<(), FileServiceError>>;
}

/// The default quota: no limit.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnlimitedQuota;

impl QuotaCheck for UnlimitedQuota {
    fn check<'a>(
        &'a self,
        _site: &'a SiteId,
        _user: &'a RecordId,
        _add_bytes: u64,
    ) -> BoxFuture<'a, Result<(), FileServiceError>> {
        Box::pin(async { Ok(()) })
    }
}

/// A simple byte limit per site and per user, for tests and small hosts: `add_bytes` above
/// either limit is refused. It does not track running usage.
#[derive(Clone, Copy, Debug)]
pub struct ByteLimitQuota {
    /// The site's limit in bytes.
    pub site_bytes: u64,
    /// The user's limit in bytes.
    pub user_bytes: u64,
}

impl QuotaCheck for ByteLimitQuota {
    fn check<'a>(
        &'a self,
        _site: &'a SiteId,
        _user: &'a RecordId,
        add_bytes: u64,
    ) -> BoxFuture<'a, Result<(), FileServiceError>> {
        Box::pin(async move {
            if add_bytes > self.user_bytes {
                return Err(FileServiceError::QuotaExceeded {
                    scope: "user",
                    limit: self.user_bytes,
                });
            }
            if add_bytes > self.site_bytes {
                return Err(FileServiceError::QuotaExceeded {
                    scope: "site",
                    limit: self.site_bytes,
                });
            }
            Ok(())
        })
    }
}

/// A [`CallerSource`] over a fixed token table, for tests and single-process hosts: a token in
/// the table is that user; anything else (including no credential) is a guest.
#[derive(Clone, Debug, Default)]
pub struct TokenCallers {
    /// `token -> caller`.
    by_token: BTreeMap<String, Caller>,
}

impl TokenCallers {
    /// An empty table: every request is a guest.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same table with `token` resolving to `caller`.
    pub fn with(mut self, token: &str, caller: Caller) -> Self {
        self.by_token.insert(token.to_owned(), caller);
        self
    }
}

impl CallerSource for TokenCallers {
    fn caller<'a>(&'a self, ctx: &'a FileCtx) -> BoxFuture<'a, Result<Caller, FileServiceError>> {
        Box::pin(async move {
            let caller = ctx
                .credential
                .as_ref()
                .and_then(|token| self.by_token.get(token).copied())
                .unwrap_or(Caller::Guest);
            Ok(caller)
        })
    }
}

/// A [`CallerSource`] that always answers the same caller, for tests.
#[derive(Clone, Copy, Debug)]
pub struct FixedCaller(pub Caller);

impl CallerSource for FixedCaller {
    fn caller<'a>(&'a self, _ctx: &'a FileCtx) -> BoxFuture<'a, Result<Caller, FileServiceError>> {
        Box::pin(async move { Ok(self.0) })
    }
}

/// An [`AttachAccess`] fake driven by a TOML grant table.
///
/// ```toml
/// # Fields that are protected (ADR-070 section 12): closed unless a grant opens them. A field
/// # not listed here follows the document right.
/// protected_fields = ["attachment"]
///
/// [[grant]]
/// user = "*"            # optional: a user id, or "*"/absent for every signed-in user
/// doctype = "sales_invoice"
/// read = true
/// write = true
///
/// [[grant.field]]       # a protected field this grant opens
/// name = "attachment"
/// read = true
/// write = false
/// ```
#[derive(Clone, Debug, Default)]
pub struct TableAttachAccess {
    /// The parsed grants.
    grants: Vec<Grant>,
    /// The protected field names (closed by default).
    protected_fields: std::collections::BTreeSet<String>,
}

/// One grant row.
#[derive(Clone, Debug)]
struct Grant {
    /// `None` means every signed-in user.
    user: Option<String>,
    /// The DocType the grant covers.
    doctype: String,
    /// Whether the document itself is readable.
    read: bool,
    /// Whether the document itself is writable.
    write: bool,
    /// The protected fields the grant opens.
    fields: Vec<FieldGrant>,
}

/// A protected-field grant.
#[derive(Clone, Debug)]
struct FieldGrant {
    /// The field name.
    name: String,
    /// Whether the field is readable.
    read: bool,
    /// Whether the field is writable.
    write: bool,
}

/// The raw TOML shape.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTable {
    #[serde(default)]
    grant: Vec<RawGrant>,
    #[serde(default)]
    protected_fields: Vec<String>,
}

/// One raw grant row.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGrant {
    #[serde(default)]
    user: Option<String>,
    doctype: String,
    #[serde(default)]
    read: bool,
    #[serde(default)]
    write: bool,
    #[serde(default)]
    field: Vec<RawField>,
}

/// One raw protected-field row.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    name: String,
    #[serde(default)]
    read: bool,
    #[serde(default)]
    write: bool,
}

impl TableAttachAccess {
    /// Parses the grant table.
    ///
    /// # Errors
    ///
    /// A TOML syntax error or an unknown key.
    pub fn from_toml(text: &str) -> Result<Self, toml::de::Error> {
        let raw: RawTable = toml::from_str(text)?;
        let grants = raw
            .grant
            .into_iter()
            .map(|g| Grant {
                user: g.user.filter(|u| u != "*"),
                doctype: g.doctype,
                read: g.read,
                write: g.write,
                fields: g
                    .field
                    .into_iter()
                    .map(|f| FieldGrant {
                        name: f.name,
                        read: f.read,
                        write: f.write,
                    })
                    .collect(),
            })
            .collect();
        Ok(TableAttachAccess {
            grants,
            protected_fields: raw.protected_fields.into_iter().collect(),
        })
    }

    /// Whether any grant matching the caller and DocType allows `want`, where `want` is the
    /// document-level right or a protected field's right.
    fn allows(&self, caller: &Caller, target: &AttachTarget, want: Want) -> bool {
        let Some(user) = caller.user_id() else {
            return false; // guests hold no document grants here
        };
        let user = user.to_string();
        let protected = target
            .field
            .as_ref()
            .is_some_and(|field| self.protected_fields.contains(field));
        self.grants
            .iter()
            .filter(|g| g.doctype == target.doctype)
            .filter(|g| g.user.as_deref().is_none_or(|u| u == user))
            .any(|g| {
                let document = match want {
                    Want::Read => g.read,
                    Want::Write => g.write,
                };
                if !protected {
                    return document;
                }
                let field = target.field.as_deref().unwrap_or_default();
                let field_right = g
                    .fields
                    .iter()
                    .find(|f| f.name == field)
                    .map(|f| match want {
                        Want::Read => f.read,
                        Want::Write => f.write,
                    });
                document && field_right.unwrap_or(false)
            })
    }
}

/// Which right a check asks for.
#[derive(Clone, Copy)]
enum Want {
    /// The read right.
    Read,
    /// The write right.
    Write,
}

impl AttachAccess for TableAttachAccess {
    fn may_read<'a>(
        &'a self,
        caller: &'a Caller,
        target: &'a AttachTarget,
    ) -> BoxFuture<'a, Result<bool, FileServiceError>> {
        Box::pin(async move { Ok(self.allows(caller, target, Want::Read)) })
    }

    fn may_upload<'a>(
        &'a self,
        caller: &'a Caller,
        target: &'a AttachTarget,
    ) -> BoxFuture<'a, Result<bool, FileServiceError>> {
        Box::pin(async move { Ok(self.allows(caller, target, Want::Write)) })
    }
}

#[cfg(test)]
mod tests {
    use super::{ByteLimitQuota, Caller, QuotaCheck, TableAttachAccess, Want};
    use crate::record::AttachTarget;
    use spot_core::SiteId;
    use spot_storage_contract::RecordId;

    fn table() -> TableAttachAccess {
        TableAttachAccess::from_toml(
            r#"
            protected_fields = ["attachment"]

            [[grant]]
            user = "*"
            doctype = "note"
            read = true
            write = false

            [[grant]]
            doctype = "secret"
            read = true
            write = true

            [[grant.field]]
            name = "attachment"
            read = true
            write = true
            "#,
        )
        .unwrap_or_else(|e| panic!("table: {e:?}"))
    }

    #[test]
    fn grants_follow_the_document_and_protected_field() {
        let table = table();
        let user = Caller::User(RecordId::new_v7());
        let doc = RecordId::new_v7();

        // A document-level read is open; a protected field is closed unless granted.
        assert!(table.allows(&user, &AttachTarget::document("note", doc), Want::Read));
        assert!(!table.allows(
            &user,
            &AttachTarget::field("note", doc, "attachment"),
            Want::Read
        ));
        // A field that is not protected follows the document right.
        assert!(table.allows(
            &user,
            &AttachTarget::field("note", doc, "plain"),
            Want::Read
        ));
        // `secret` opens its `attachment` field.
        assert!(table.allows(
            &user,
            &AttachTarget::field("secret", doc, "attachment"),
            Want::Read
        ));
        // Guests hold nothing.
        assert!(!table.allows(
            &Caller::Guest,
            &AttachTarget::document("note", doc),
            Want::Read
        ));
    }

    #[tokio::test]
    async fn quota_fake_refuses_over_the_limit() {
        let quota = ByteLimitQuota {
            site_bytes: 100,
            user_bytes: 10,
        };
        let site = SiteId::new("files.test").unwrap_or_else(|e| panic!("site: {e:?}"));
        let user = RecordId::new_v7();
        assert!(quota.check(&site, &user, 5).await.is_ok());
        assert!(quota.check(&site, &user, 50).await.is_err());
    }
}
