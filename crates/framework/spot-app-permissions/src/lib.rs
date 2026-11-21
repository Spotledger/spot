//! The adapter from the permission decision core to the document engine's named-permission
//! seam (WP-PLT-03.T3).
//!
//! A whitelisted method can declare `right = permission("approve_note")`. The document engine
//! refuses such a call unless an [`AppPermissionCheck`] says the principal holds the
//! permission, and ships only a deny-all default. [`SnapshotAppPermissions`] is the real one:
//! it asks `spot-permissions` (`PermissionEngine::app_permission`) and so honours the site's
//! catalogue, the roles' grants, inheritance, duties and expiry that built its snapshot.
//!
//! # Rules
//!
//! - **One per site.** It is built over that site's [`PermissionAuthorizer`]; there is no
//!   process-wide state, so one site's grants can never be read for another.
//! - **Nothing is cached.** Every call reads the engine in force at that moment, so a snapshot
//!   swap (a revoked grant) takes effect on the next request.
//! - **Fail closed.** The seam answers a plain `bool`; every refusal, whatever the reason
//!   (invalid name, undeclared, not granted), is `false`, and the caller-visible refusal is the
//!   engine's own, which does not say why.
//! - **The operator can tell why.** The adapter keeps the last 256 denials with their reason
//!   code ([`SnapshotAppPermissions::denial_log`]). The log holds no role name.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use spot_app_api::BoxFuture;
use spot_document::engine::{AppPermissionCheck, PermissionAuthorizer};
use spot_permissions::Principal;

/// How many denials the adapter keeps.
pub const DENIAL_LOG_CAPACITY: usize = 256;

/// The longest permission name or user name a log entry keeps, in bytes. Longer text is cut
/// at a character boundary: the log is bounded in size as well as in length.
const MAX_FIELD: usize = 128;

/// One denial the adapter decided: what was asked, by whom, and why it was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppDenial {
    /// The permission name asked for, as asked (cut at 128 bytes).
    pub permission: String,
    /// The principal's user name (cut at 128 bytes).
    pub user: String,
    /// The reason code: `perm.app.not_granted`, `perm.app.unknown` or `perm.app.invalid_name`.
    pub reason: &'static str,
}

/// An [`AppPermissionCheck`] that decides from the snapshot of one site's permission engine.
pub struct SnapshotAppPermissions {
    authorizer: Arc<PermissionAuthorizer>,
    denials: Mutex<VecDeque<AppDenial>>,
}

impl SnapshotAppPermissions {
    /// The check for the site whose engine `authorizer` holds. Pass the same authorizer the
    /// document engine uses, so a swap of its permission engine reaches this check too.
    #[must_use]
    pub fn new(authorizer: Arc<PermissionAuthorizer>) -> SnapshotAppPermissions {
        SnapshotAppPermissions {
            authorizer,
            denials: Mutex::new(VecDeque::new()),
        }
    }

    /// The denials decided so far (the last 256), oldest first.
    #[must_use]
    pub fn denial_log(&self) -> Vec<AppDenial> {
        self.denials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    /// Decides one question against the engine in force right now and logs a denial.
    fn decide(&self, principal: &Principal, permission: &str) -> bool {
        let decision = self
            .authorizer
            .engine()
            .app_permission(principal, permission);
        if !decision.allowed {
            self.record(AppDenial {
                permission: cut(permission),
                user: cut(&principal.user),
                reason: decision.reason.code(),
            });
        }
        decision.allowed
    }

    fn record(&self, denial: AppDenial) {
        let mut log = self
            .denials
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if log.len() >= DENIAL_LOG_CAPACITY {
            log.pop_front();
        }
        log.push_back(denial);
    }
}

impl AppPermissionCheck for SnapshotAppPermissions {
    fn holds<'a>(&'a self, principal: &'a Principal, permission: &'a str) -> BoxFuture<'a, bool> {
        // Decided synchronously, with no await: the log's guard is never held across one.
        let allowed = self.decide(principal, permission);
        Box::pin(async move { allowed })
    }
}

fn cut(text: &str) -> String {
    let mut end = text.len().min(MAX_FIELD);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}
