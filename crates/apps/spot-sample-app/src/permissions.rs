//! The app's permission model as embedded TOML text (WP-DX-05.T1).
//!
//! The same files as `apps/sample/permissions/`. [`PERMISSION_TEXTS`] is the
//! `ModelTexts` shape `spot-permissions`' loader (`spot_permissions::load`)
//! parses: `grants` the DocType rights, `field_grants` the field and
//! protection-group rights, `groups` the declared protection groups, `shares`
//! the per-document rights (the sample ships none).

/// The permission model texts of the app, as embedded TOML.
pub struct PermissionTexts {
    /// The DocType grants: role, doctype, rights, `if_owner`.
    pub grants: &'static str,
    /// The field and protection-group grants.
    pub field_grants: &'static str,
    /// The declared protection groups.
    pub groups: &'static str,
    /// The per-document shares; empty for the sample.
    pub shares: &'static str,
}

/// The app's permission model (the `Project Manager` / `Team Member` / `Viewer`
/// roles, the owner rules and the `confidential` group).
pub const PERMISSION_TEXTS: PermissionTexts = PermissionTexts {
    grants: include_str!("../../../../apps/sample/permissions/grants.toml"),
    field_grants: include_str!("../../../../apps/sample/permissions/field_grants.toml"),
    groups: include_str!("../../../../apps/sample/permissions/groups.toml"),
    shares: "",
};

/// The three roles the app ships, in grant-table order.
pub const ROLES: [&str; 3] = ["Project Manager", "Team Member", "Viewer"];

impl PermissionTexts {
    /// The four texts as a tuple, in the `ModelTexts` field order — the shape the
    /// engine's loader constructs from.
    #[must_use]
    pub const fn as_tuple(&self) -> (&'static str, &'static str, &'static str, &'static str) {
        (self.grants, self.field_grants, self.groups, self.shares)
    }
}
