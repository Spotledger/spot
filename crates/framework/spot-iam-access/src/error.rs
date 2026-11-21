//! The stable error codes of this crate, as listed in the error catalogue
//! (`crates/framework/spot-errors/catalogue/iamaccess.toml`).

/// `iamaccess.scope_excess`: an API key asks for a right its owner does not hold.
pub const SCOPE_EXCESS: &str = "iamaccess.scope_excess";
/// `iamaccess.scope_empty`: an API key scope allows nothing, or a rule names no action.
pub const SCOPE_EMPTY: &str = "iamaccess.scope_empty";
/// `iamaccess.key_owner_type`: only a service or internal user owns an API key.
pub const KEY_OWNER_TYPE: &str = "iamaccess.key_owner_type";
/// `iamaccess.key_owner_inactive`: a key cannot be created for a disabled or anonymised owner.
pub const KEY_OWNER_INACTIVE: &str = "iamaccess.key_owner_inactive";
/// `iamaccess.key_not_usable`: the key cannot be used; the message says nothing more.
pub const KEY_NOT_USABLE: &str = "iamaccess.key_not_usable";
/// `iamaccess.impersonation_not_allowed`: this principal may not impersonate this user.
pub const IMPERSONATION_NOT_ALLOWED: &str = "iamaccess.impersonation_not_allowed";
/// `iamaccess.impersonation_grant_invalid`: the support grant does not allow impersonation now.
pub const IMPERSONATION_GRANT_INVALID: &str = "iamaccess.impersonation_grant_invalid";
/// `iamaccess.impersonation_expired`: the impersonation session is over.
pub const IMPERSONATION_EXPIRED: &str = "iamaccess.impersonation_expired";
/// `iamaccess.step_up_required`: the operation needs a recent strong authentication.
pub const STEP_UP_REQUIRED: &str = "iamaccess.step_up_required";
/// `iamaccess.method_sso_not_allowed`: an SSO session cannot switch to a user without SSO.
pub const METHOD_SSO_NOT_ALLOWED: &str = "iamaccess.method_sso_not_allowed";
/// `iamaccess.method_weaker`: a session cannot move to a weaker sign-in method.
pub const METHOD_WEAKER: &str = "iamaccess.method_weaker";

/// The codes, grouped for a caller that re-exports them in one place.
pub mod codes {
    pub use super::{
        IMPERSONATION_EXPIRED, IMPERSONATION_GRANT_INVALID, IMPERSONATION_NOT_ALLOWED,
        KEY_NOT_USABLE, KEY_OWNER_INACTIVE, KEY_OWNER_TYPE, METHOD_SSO_NOT_ALLOWED, METHOD_WEAKER,
        SCOPE_EMPTY, SCOPE_EXCESS, STEP_UP_REQUIRED,
    };
}
