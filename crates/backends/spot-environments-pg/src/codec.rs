//! The stored document: a set to JSON and back. Decoding rebuilds the set through
//! [`EnvironmentSet::new`], so a document edited by hand into an invalid set is refused.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use spot_environments::{AppRef, Environment, EnvironmentSet, Policy, Role};
use spot_tenancy::{Region, TenantId};

use spot_environments::EnvironmentError;

use crate::error::row_invalid;

#[derive(Serialize, Deserialize)]
struct AppDoc {
    name: String,
    version: String,
}

#[derive(Serialize, Deserialize)]
struct MemberDoc {
    tenant: TenantId,
    role: String,
    order: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetDoc {
    name: String,
    region: Region,
    apps: Vec<AppDoc>,
    members: Vec<MemberDoc>,
    allow_skip: bool,
}

fn role_of(name: &str) -> Option<Role> {
    Role::ALL.into_iter().find(|role| role.name() == name)
}

/// The document of a (validated) set.
pub(crate) fn encode(set: &EnvironmentSet) -> Result<Value, EnvironmentError> {
    let doc = SetDoc {
        name: set.name().to_owned(),
        region: set.region().clone(),
        apps: set
            .apps()
            .iter()
            .map(|a| AppDoc {
                name: a.name.clone(),
                version: a.version.clone(),
            })
            .collect(),
        members: set
            .members()
            .iter()
            .map(|m| MemberDoc {
                tenant: m.tenant,
                role: m.role.name().to_owned(),
                order: m.order,
            })
            .collect(),
        allow_skip: set.policy().allow_skip,
    };
    serde_json::to_value(doc).map_err(|_| row_invalid())
}

/// The set a stored document describes, which must be valid and carry the row's `name`.
pub(crate) fn decode(name: &str, value: Value) -> Result<EnvironmentSet, EnvironmentError> {
    let doc: SetDoc = serde_json::from_value(value).map_err(|_| row_invalid())?;
    if doc.name != name {
        return Err(row_invalid());
    }
    let apps = doc
        .apps
        .into_iter()
        .map(|a| AppRef::new(a.name, a.version))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| row_invalid())?;
    let members = doc
        .members
        .into_iter()
        .map(|m| {
            Some(Environment {
                tenant: m.tenant,
                role: role_of(&m.role)?,
                order: m.order,
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(row_invalid)?;
    EnvironmentSet::new(
        doc.name,
        doc.region,
        apps,
        members,
        Policy {
            allow_skip: doc.allow_skip,
        },
    )
    .map_err(|_| row_invalid())
}
