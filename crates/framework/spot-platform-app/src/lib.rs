//! The platform's own app (WP-IAM-02.T2, ADR-070): the records the platform keeps about who can
//! sign in, what they may do, and which keys and sessions exist.
//!
//! They are ordinary DocTypes, so permissions, audit, sync and the generic UI work on them. This
//! crate holds **data and its loaders, no behaviour**: the TOML under `apps/platform/` is
//! embedded here, [`definitions`] parses it, [`permissions`] loads the shipped grants and
//! [`bootstrap::seed`] writes the records every site starts with. Signing in, hashing and HTTP
//! live in `spot-auth`, the sign-in service and `spot-http`; the sign-in service reads these
//! records as the system principal, directly through `DocumentStore`.
//!
//! # Credentials
//!
//! Every credential or digest field is a `secret` (no column; the value is held by the secrets
//! service) in the protection group `credential`, which is closed by default: no shipped grant
//! opens it, so only the system principal reads it, and every read is traced.
//!
//! # Names
//!
//! [`PLATFORM_APP`] is the app's name, [`doctype`] has the DocType names and [`role`] the system
//! role names, so no other crate repeats the strings.

pub mod bootstrap;

use spot_meta::{Definition, MetaError};
use spot_permissions::{Model, ModelTexts, PermErrors, load};

/// The app's name, as `[app] name` in its manifest and `app` on every DocType.
pub const PLATFORM_APP: &str = "platform";

/// The manifest, `apps/platform/app.toml`.
pub const MANIFEST: &str = include_str!("../../../../apps/platform/app.toml");

/// The DocType names.
pub mod doctype {
    /// A person, service or agent that can sign in.
    pub const USER: &str = "user";
    /// Child of `user`: one-time recovery digests.
    pub const USER_RECOVERY_DIGEST: &str = "user_recovery_digest";
    /// A named set of duties.
    pub const ROLE: &str = "role";
    /// Child of `role`: one duty it includes.
    pub const ROLE_DUTY: &str = "role_duty";
    /// A user's role, with its own scope and time.
    pub const ROLE_ASSIGNMENT: &str = "role_assignment";
    /// Child of `role_assignment`: one value restriction.
    pub const ROLE_ASSIGNMENT_SCOPE: &str = "role_assignment_scope";
    /// A user's identity at an external provider.
    pub const LINKED_IDENTITY: &str = "linked_identity";
    /// A service credential.
    pub const API_KEY: &str = "api_key";
    /// Child of `api_key`: one scope.
    pub const API_KEY_SCOPE: &str = "api_key_scope";
    /// A signed-in session.
    pub const SESSION: &str = "session";
    /// One sign-in attempt; append-only.
    pub const LOGIN_ATTEMPT: &str = "login_attempt";
    /// A DocType a site added (WP-PLT-16.T1).
    pub const CUSTOM_DOCTYPE: &str = "custom_doctype";
    /// A field a site added to a DocType (WP-PLT-16.T1).
    pub const CUSTOM_FIELD: &str = "custom_field";
    /// A site's override of a property of a DocType or field (WP-PLT-16.T1).
    pub const PROPERTY_SETTER: &str = "property_setter";
    /// An outbound webhook subscription (WP-INT-01.T1).
    pub const WEBHOOK: &str = "webhook";
    /// One delivery attempt of a webhook; written only by the delivery service.
    pub const WEBHOOK_DELIVERY: &str = "webhook_delivery";

    /// Every DocType of the app, parents before their children, sorted otherwise.
    pub const ALL: [&str; 16] = [
        USER,
        USER_RECOVERY_DIGEST,
        ROLE,
        ROLE_DUTY,
        ROLE_ASSIGNMENT,
        ROLE_ASSIGNMENT_SCOPE,
        LINKED_IDENTITY,
        API_KEY,
        API_KEY_SCOPE,
        SESSION,
        LOGIN_ATTEMPT,
        CUSTOM_DOCTYPE,
        CUSTOM_FIELD,
        PROPERTY_SETTER,
        WEBHOOK,
        WEBHOOK_DELIVERY,
    ];
}

/// The protection group names.
pub mod group {
    /// Stored credentials and their digests: closed to every role.
    pub const CREDENTIAL: &str = "credential";
    /// The digest of a sign-in attempt's source address.
    pub const LOGIN_SOURCE: &str = "login_source";
    /// The sealed signing secret of a webhook: no role grant opens it; only the system
    /// principal (the delivery service) and the Tenant Administrator bypass (ADR-070 s12 rule 4,
    /// an engine-wide exception pending the architecture owner's decision) open it.
    pub const WEBHOOK_SECRET: &str = "secret";
    /// A webhook's configuration group.
    pub const WEBHOOK_CONFIG: &str = "config";
}

/// The system role names.
pub mod role {
    /// Holds every right (ADR-070 section 12, rule 4). The Administrator is assigned it.
    pub const TENANT_ADMINISTRATOR: &str = "Tenant Administrator";
    /// Manages roles, assignments and linked identities.
    pub const SYSTEM_MANAGER: &str = "System Manager";
    /// Creates and edits users and their assignments.
    pub const USER_MANAGER: &str = "User Manager";
    /// Reads webhooks and their delivery log (never the signing secret).
    pub const WEBHOOK_MANAGER: &str = "Webhook Manager";
}

/// The embedded definitions, in the order of [`doctype::ALL`].
const DOCTYPE_SOURCES: [(&str, &str); 16] = [
    (
        "user.toml",
        include_str!("../../../../apps/platform/doctypes/user.toml"),
    ),
    (
        "user_recovery_digest.toml",
        include_str!("../../../../apps/platform/doctypes/user_recovery_digest.toml"),
    ),
    (
        "role.toml",
        include_str!("../../../../apps/platform/doctypes/role.toml"),
    ),
    (
        "role_duty.toml",
        include_str!("../../../../apps/platform/doctypes/role_duty.toml"),
    ),
    (
        "role_assignment.toml",
        include_str!("../../../../apps/platform/doctypes/role_assignment.toml"),
    ),
    (
        "role_assignment_scope.toml",
        include_str!("../../../../apps/platform/doctypes/role_assignment_scope.toml"),
    ),
    (
        "linked_identity.toml",
        include_str!("../../../../apps/platform/doctypes/linked_identity.toml"),
    ),
    (
        "api_key.toml",
        include_str!("../../../../apps/platform/doctypes/api_key.toml"),
    ),
    (
        "api_key_scope.toml",
        include_str!("../../../../apps/platform/doctypes/api_key_scope.toml"),
    ),
    (
        "session.toml",
        include_str!("../../../../apps/platform/doctypes/session.toml"),
    ),
    (
        "login_attempt.toml",
        include_str!("../../../../apps/platform/doctypes/login_attempt.toml"),
    ),
    (
        "custom_doctype.toml",
        include_str!("../../../../apps/platform/doctypes/custom_doctype.toml"),
    ),
    (
        "custom_field.toml",
        include_str!("../../../../apps/platform/doctypes/custom_field.toml"),
    ),
    (
        "property_setter.toml",
        include_str!("../../../../apps/platform/doctypes/property_setter.toml"),
    ),
    (
        "webhook.toml",
        include_str!("../../../../apps/platform/doctypes/webhook.toml"),
    ),
    (
        "webhook_delivery.toml",
        include_str!("../../../../apps/platform/doctypes/webhook_delivery.toml"),
    ),
];

/// The platform DocTypes, parsed from the embedded TOML, in the order of [`doctype::ALL`].
///
/// # Errors
///
/// A [`MetaError`] naming the file, if an embedded file does not parse (a test keeps that from
/// shipping).
pub fn definitions() -> Result<Vec<Definition>, MetaError> {
    DOCTYPE_SOURCES
        .iter()
        .map(|(file, text)| Definition::parse(file, text))
        .collect()
}

/// The texts of the shipped permission model: DocType grants, field grants and protection
/// groups. There are no shares.
#[must_use]
pub fn permission_texts() -> ModelTexts {
    ModelTexts {
        grants: [
            include_str!("../../../../apps/platform/permissions/grants.toml"),
            include_str!("../../../../apps/platform/permissions/webhook_grants.toml"),
        ]
        .join("\n"),
        field_grants: include_str!("../../../../apps/platform/permissions/field_grants.toml")
            .to_owned(),
        groups: include_str!("../../../../apps/platform/permissions/groups.toml").to_owned(),
        shares: String::new(),
    }
}

/// The shipped grants, loaded and checked by the permission loader.
///
/// # Errors
///
/// Every problem the loader finds, at once.
pub fn permissions() -> Result<Model, PermErrors> {
    load(&permission_texts())
}
