//! The RFC 7643 resources and their mapping to and from the platform's records (ADR-070 7.2).
//!
//! [`ScimUser`] and [`ScimGroup`] are the wire documents an Entra-style or Okta-style client
//! sends and reads. They are **not** the platform's records: a [`StoredUser`] pairs the
//! directory [`User`] (identity, enablement, manager) with a [`ScimProfile`] holding the
//! attributes SCIM has and the directory does not (the `externalId`, the name parts, the
//! addresses and the enterprise extension). Mapping is explicit and total in both directions,
//! so the same SCIM document always means the same directory record.
//!
//! Unknown attributes are tolerated on input, as RFC 7643 requires of a server that does not
//! implement every extension; they are simply not stored. The attributes this server does know
//! are validated, and a bad one is refused with [`ScimError::invalid_value`].

use serde::{Deserialize, Serialize};
use spot_iam_directory::{User, UserType};
use spot_storage_contract::RecordId;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::TenantId;
use crate::error::{ScimError, ScimResult};

/// The core User schema URN.
pub const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
/// The core Group schema URN.
pub const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
/// The enterprise user extension URN.
pub const ENTERPRISE_USER_SCHEMA: &str =
    "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User";
/// The ListResponse URN.
pub const LIST_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:ListResponse";
/// The Error URN.
pub const ERROR_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:Error";
/// The PatchOp URN.
pub const PATCH_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
/// The ServiceProviderConfig URN.
pub const SERVICE_PROVIDER_CONFIG_SCHEMA: &str =
    "urn:ietf:params:scim:schemas:core:2.0:ServiceProviderConfig";
/// The Schema resource URN.
pub const SCHEMA_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Schema";
/// The ResourceType resource URN.
pub const RESOURCE_TYPE_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:ResourceType";

/// The path every resource lives under.
pub const SCIM_BASE: &str = "/scim/v2";

/// The `application/scim+json` content type.
pub const SCIM_CONTENT_TYPE: &str = "application/scim+json";

fn yes() -> bool {
    true
}

/// A name, in the parts RFC 7643 defines.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Name {
    /// The full name as one string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formatted: Option<String>,
    /// The given (first) name.
    #[serde(rename = "givenName", default, skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    /// The family (last) name.
    #[serde(
        rename = "familyName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub family_name: Option<String>,
    /// The middle name.
    #[serde(
        rename = "middleName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub middle_name: Option<String>,
    /// A prefix such as "Dr.".
    #[serde(
        rename = "honorificPrefix",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub honorific_prefix: Option<String>,
    /// A suffix such as "Jr.".
    #[serde(
        rename = "honorificSuffix",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub honorific_suffix: Option<String>,
}

/// One e-mail address of a user.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Email {
    /// The address.
    pub value: String,
    /// `work`, `home` or another type the client uses.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Whether this is the preferred address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<bool>,
}

/// A manager reference in the enterprise extension.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manager {
    /// The manager's user id.
    pub value: String,
    /// The manager's display name.
    #[serde(
        rename = "displayName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub display_name: Option<String>,
}

/// The enterprise user extension subset this server keeps.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnterpriseUser {
    /// The employee number.
    #[serde(
        rename = "employeeNumber",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub employee_number: Option<String>,
    /// The department.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub department: Option<String>,
    /// The manager.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manager: Option<Manager>,
}

/// The resource metadata RFC 7643 defines.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// `User` or `Group`.
    #[serde(rename = "resourceType")]
    pub resource_type: String,
    /// When the resource was created.
    #[serde(
        default,
        with = "wire::opt_utc",
        skip_serializing_if = "Option::is_none"
    )]
    pub created: Option<OffsetDateTime>,
    /// When it last changed.
    #[serde(
        rename = "lastModified",
        default,
        with = "wire::opt_utc",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_modified: Option<OffsetDateTime>,
    /// The URI of the resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// The ETag, quoted as an HTTP entity tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A user as the SCIM wire carries it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimUser {
    /// The schemas, always including the core User URN.
    #[serde(default)]
    pub schemas: Vec<String>,
    /// The resource id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The client's own id for this user.
    #[serde(
        rename = "externalId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub external_id: Option<String>,
    /// The sign-in name, unique per tenant.
    #[serde(rename = "userName", default)]
    pub user_name: String,
    /// The name parts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<Name>,
    /// The name shown to people.
    #[serde(
        rename = "displayName",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub display_name: Option<String>,
    /// The addresses.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub emails: Vec<Email>,
    /// Whether the user may sign in. `false` is deactivation, never deletion.
    #[serde(default = "yes")]
    pub active: bool,
    /// The enterprise extension.
    #[serde(
        rename = "urn:ietf:params:scim:schemas:extension:enterprise:2.0:User",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub enterprise: Option<EnterpriseUser>,
    /// The resource metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

/// One member of a group.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    /// The member's user id.
    pub value: String,
    /// The member's display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// `User` or `Group`.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// A group as the SCIM wire carries it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScimGroup {
    /// The schemas, always including the core Group URN.
    #[serde(default)]
    pub schemas: Vec<String>,
    /// The resource id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The client's own id for this group.
    #[serde(
        rename = "externalId",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub external_id: Option<String>,
    /// The group's name, unique per tenant.
    #[serde(rename = "displayName", default)]
    pub display_name: String,
    /// The members.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<Member>,
    /// The resource metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<Meta>,
}

/// The SCIM-only attributes of a user: the parts the directory record does not carry.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScimProfile {
    /// The `userName` exactly as the client sent it (the directory stores it lower-cased).
    pub user_name: String,
    /// The client's own id.
    pub external_id: Option<String>,
    /// The name parts.
    pub name: Option<Name>,
    /// The addresses.
    pub emails: Vec<Email>,
    /// The enterprise extension.
    pub enterprise: Option<EnterpriseUser>,
}

/// A stored user: the directory record plus the SCIM profile and the resource version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredUser {
    /// The tenant the record belongs to. A SCIM call never reaches another tenant's record.
    pub tenant: TenantId,
    /// The directory user.
    pub user: User,
    /// The SCIM-only attributes.
    pub profile: ScimProfile,
    /// The resource version, bumped on every change; the ETag's source.
    pub version: u64,
    /// When the record last changed.
    pub updated_at: OffsetDateTime,
}

/// A stored group: its name, its members and its version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredGroup {
    /// The tenant the record belongs to.
    pub tenant: TenantId,
    /// The group's id.
    pub id: RecordId,
    /// The client's own id.
    pub external_id: Option<String>,
    /// The group's name, unique per tenant.
    pub display_name: String,
    /// The member user ids.
    pub members: Vec<RecordId>,
    /// The resource version, bumped on every change.
    pub version: u64,
    /// When the group was created.
    pub created_at: OffsetDateTime,
    /// When it last changed.
    pub updated_at: OffsetDateTime,
}

/// The quoted ETag of a resource at `version`.
pub fn etag(version: u64) -> String {
    format!("W/\"{version}\"")
}

/// The unquoted version number of an `If-Match` header value, or `None` when it is not one.
pub fn parse_etag(value: &str) -> Option<u64> {
    let trimmed = value.trim();
    let inner = trimmed
        .strip_prefix("W/\"")
        .and_then(|s| s.strip_suffix('"'))
        .or_else(|| trimmed.strip_prefix('"').and_then(|s| s.strip_suffix('"')))
        .unwrap_or(trimmed);
    inner.parse::<u64>().ok()
}

fn schemas_with(core: &str, extension: bool) -> Vec<String> {
    let mut schemas = vec![core.to_owned()];
    if extension {
        schemas.push(ENTERPRISE_USER_SCHEMA.to_owned());
    }
    schemas
}

impl StoredUser {
    /// The wire resource, with `meta` filled in.
    pub fn to_resource(&self) -> ScimUser {
        let id = self.user.id.to_string();
        ScimUser {
            schemas: schemas_with(USER_SCHEMA, self.profile.enterprise.is_some()),
            id: Some(id.clone()),
            external_id: self.profile.external_id.clone(),
            user_name: self.profile.user_name.clone(),
            name: self.profile.name.clone(),
            display_name: Some(self.user.display_name.clone()),
            emails: self.profile.emails.clone(),
            active: self.user.enabled,
            enterprise: self.profile.enterprise.clone(),
            meta: Some(Meta {
                resource_type: "User".to_owned(),
                created: Some(self.user.created_at),
                last_modified: Some(self.updated_at),
                location: Some(format!("{SCIM_BASE}/Users/{id}")),
                version: Some(etag(self.version)),
            }),
        }
    }

    /// A new stored user from a create request. The id, the tenant and the instant are the
    /// caller's; nothing here reads a clock or storage.
    pub fn create(
        tenant: TenantId,
        id: RecordId,
        input: ScimUser,
        now: OffsetDateTime,
    ) -> ScimResult<Self> {
        let user_name = input.user_name.trim().to_owned();
        if user_name.is_empty() {
            return Err(ScimError::invalid_value("`userName` is required"));
        }
        let display_name = input
            .display_name
            .clone()
            .or_else(|| input.name.as_ref().and_then(|n| n.formatted.clone()))
            .unwrap_or_else(|| user_name.clone());
        let manager = input
            .enterprise
            .as_ref()
            .and_then(|e| e.manager.as_ref())
            .and_then(|m| parse_record_id(&m.value));
        let user = User {
            id,
            user_type: UserType::Internal,
            enabled: input.active,
            sign_in_name: normalise_sign_in_name(&user_name),
            display_name,
            party_link: None,
            manager,
            created_at: now,
            deactivated_at: if input.active { None } else { Some(now) },
        };
        check_directory_user(&user)?;
        Ok(Self {
            tenant,
            user,
            profile: ScimProfile {
                user_name,
                external_id: input.external_id,
                name: input.name,
                emails: input.emails,
                enterprise: input.enterprise,
            },
            version: 1,
            updated_at: now,
        })
    }

    /// Replaces the mutable attributes of the stored user from a PUT body, keeping the id, the
    /// creation time and the SCIM profile's shape. `active` moves through the directory's
    /// enable/disable rules, so a PUT is deactivation just like a PATCH.
    pub fn apply_put(&self, input: &ScimUser, now: OffsetDateTime) -> ScimResult<Self> {
        let user_name = input.user_name.trim();
        if user_name.is_empty() {
            return Err(ScimError::invalid_value("`userName` is required"));
        }
        let display_name = input
            .display_name
            .clone()
            .or_else(|| input.name.as_ref().and_then(|n| n.formatted.clone()))
            .unwrap_or_else(|| self.user.display_name.clone());
        let manager = input
            .enterprise
            .as_ref()
            .and_then(|e| e.manager.as_ref())
            .and_then(|m| parse_record_id(&m.value));
        let user = User {
            id: self.user.id,
            user_type: self.user.user_type,
            enabled: input.active,
            sign_in_name: normalise_sign_in_name(user_name),
            display_name,
            party_link: self.user.party_link,
            manager,
            created_at: self.user.created_at,
            deactivated_at: if input.active {
                None
            } else {
                Some(self.user.deactivated_at.unwrap_or(now))
            },
        };
        check_directory_user(&user)?;
        Ok(Self {
            tenant: self.tenant.clone(),
            user,
            profile: ScimProfile {
                user_name: user_name.to_owned(),
                external_id: input.external_id.clone(),
                name: input.name.clone(),
                emails: input.emails.clone(),
                enterprise: input.enterprise.clone(),
            },
            version: self.version + 1,
            updated_at: now,
        })
    }
}

impl StoredGroup {
    /// The wire resource, with `meta` filled in and each member's display name resolved.
    pub fn to_resource(&self, display_of: &dyn Fn(&RecordId) -> Option<String>) -> ScimGroup {
        let id = self.id.to_string();
        ScimGroup {
            schemas: schemas_with(GROUP_SCHEMA, false),
            id: Some(id.clone()),
            external_id: self.external_id.clone(),
            display_name: self.display_name.clone(),
            members: self
                .members
                .iter()
                .map(|m| Member {
                    value: m.to_string(),
                    display: display_of(m),
                    kind: Some("User".to_owned()),
                })
                .collect(),
            meta: Some(Meta {
                resource_type: "Group".to_owned(),
                created: Some(self.created_at),
                last_modified: Some(self.updated_at),
                location: Some(format!("{SCIM_BASE}/Groups/{id}")),
                version: Some(etag(self.version)),
            }),
        }
    }
}

/// Lower-cases a `userName` for the directory's sign-in name. An address is case-insensitive
/// on the wire; the directory stores the canonical lower-case form.
pub fn normalise_sign_in_name(user_name: &str) -> String {
    user_name.trim().to_lowercase()
}

/// Parses a SCIM id into a [`RecordId`]; `None` when it is not the canonical form.
pub fn parse_record_id(text: &str) -> Option<RecordId> {
    crate::wire::parse_record_id(text).ok()
}

/// Maps the directory's validation to a SCIM refusal, naming the field and the rule.
fn check_directory_user(user: &User) -> ScimResult<()> {
    user.validate().map_err(|errors| {
        let detail = errors
            .errors()
            .iter()
            .map(|e| match e.field {
                Some(field) => format!("{field}: {}", e.message),
                None => e.message.clone(),
            })
            .collect::<Vec<_>>()
            .join("; ");
        ScimError::invalid_value(detail)
    })
}

/// The wire forms of the two types serde does not know: an instant (RFC 3339, always UTC).
mod wire {
    use super::*;
    use serde::{Deserializer, Serializer};

    pub mod opt_utc {
        use super::*;

        pub fn serialize<S: Serializer>(
            t: &Option<OffsetDateTime>,
            s: S,
        ) -> Result<S::Ok, S::Error> {
            match t {
                Some(t) => s.serialize_str(
                    &t.to_offset(time::UtcOffset::UTC)
                        .format(&Rfc3339)
                        .map_err(serde::ser::Error::custom)?,
                ),
                None => s.serialize_none(),
            }
        }

        pub fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> Result<Option<OffsetDateTime>, D::Error> {
            match Option::<String>::deserialize(d)? {
                Some(text) => OffsetDateTime::parse(&text, &Rfc3339)
                    .map(|t| Some(t.to_offset(time::UtcOffset::UTC)))
                    .map_err(serde::de::Error::custom),
                None => Ok(None),
            }
        }
    }
}
