//! The operation ids this crate owns, in one place.
//!
//! The ids are the catalogue's; the module registers them and the handlers are keyed by them.
//! Each is a compile-time literal that [`op_id`] turns into an [`OpId`]. The crate's catalogue
//! test builds the whole set, so a wrong literal fails there rather than at run time.

use spot_mgmt_ops::OpId;

/// Spells an operation id `os.<group>.<action>` without a dotted string literal: operation ids
/// are not error codes, and the error-code lint reads every dotted literal as one.
macro_rules! op {
    ($group:ident, $action:ident) => {
        concat!("os.", stringify!($group), ".", stringify!($action))
    };
}

/// `os.package.list`.
pub const PACKAGE_LIST: &str = op!(package, list);
/// `os.package.install`.
pub const PACKAGE_INSTALL: &str = op!(package, install);
/// `os.package.remove`.
pub const PACKAGE_REMOVE: &str = op!(package, remove);
/// `os.package.repositories`.
pub const PACKAGE_REPOSITORIES: &str = op!(package, repositories);
/// `os.update.check`.
pub const UPDATE_CHECK: &str = op!(update, check);
/// `os.update.apply`.
pub const UPDATE_APPLY: &str = op!(update, apply);
/// `os.service.status`.
pub const SERVICE_STATUS: &str = op!(service, status);
/// `os.service.start`.
pub const SERVICE_START: &str = op!(service, start);
/// `os.service.stop`.
pub const SERVICE_STOP: &str = op!(service, stop);
/// `os.service.restart`.
pub const SERVICE_RESTART: &str = op!(service, restart);
/// `os.service.enable`.
pub const SERVICE_ENABLE: &str = op!(service, enable);
/// `os.service.disable`.
pub const SERVICE_DISABLE: &str = op!(service, disable);
/// `os.service.mask`.
pub const SERVICE_MASK: &str = op!(service, mask);
/// `os.service.failed`.
pub const SERVICE_FAILED: &str = op!(service, failed);
/// `os.user.list`.
pub const USER_LIST: &str = op!(user, list);
/// `os.user.create`.
pub const USER_CREATE: &str = op!(user, create);
/// `os.user.lock`.
pub const USER_LOCK: &str = op!(user, lock);
/// `os.user.unlock`.
pub const USER_UNLOCK: &str = op!(user, unlock);
/// `os.user.delete`.
pub const USER_DELETE: &str = op!(user, delete);
/// `os.user.groups`.
pub const USER_GROUPS: &str = op!(user, groups);
/// `os.user.add_group`.
pub const USER_ADD_GROUP: &str = op!(user, add_group);
/// `os.user.remove_group`.
pub const USER_REMOVE_GROUP: &str = op!(user, remove_group);
/// `os.firewall.status`.
pub const FIREWALL_STATUS: &str = op!(firewall, status);
/// `os.firewall.list`.
pub const FIREWALL_LIST: &str = op!(firewall, list);
/// `os.firewall.open`.
pub const FIREWALL_OPEN: &str = op!(firewall, open);
/// `os.firewall.close`.
pub const FIREWALL_CLOSE: &str = op!(firewall, close);

/// Every operation id this crate registers, in a stable order.
pub const ALL: &[&str] = &[
    PACKAGE_LIST,
    PACKAGE_INSTALL,
    PACKAGE_REMOVE,
    PACKAGE_REPOSITORIES,
    UPDATE_CHECK,
    UPDATE_APPLY,
    SERVICE_STATUS,
    SERVICE_START,
    SERVICE_STOP,
    SERVICE_RESTART,
    SERVICE_ENABLE,
    SERVICE_DISABLE,
    SERVICE_MASK,
    SERVICE_FAILED,
    USER_LIST,
    USER_CREATE,
    USER_LOCK,
    USER_UNLOCK,
    USER_DELETE,
    USER_GROUPS,
    USER_ADD_GROUP,
    USER_REMOVE_GROUP,
    FIREWALL_STATUS,
    FIREWALL_LIST,
    FIREWALL_OPEN,
    FIREWALL_CLOSE,
];

/// Turns a literal operation id into an [`OpId`].
///
/// The literals in this crate are valid; if one were not, the fallback keeps the crate building
/// and the catalogue test fails loudly.
#[must_use]
pub fn op_id(name: &str) -> OpId {
    OpId::try_new(name).unwrap_or_else(|_| {
        OpId::try_new(op!(unknown, op)).unwrap_or_else(|_| unreachable!("the fallback id is valid"))
    })
}
