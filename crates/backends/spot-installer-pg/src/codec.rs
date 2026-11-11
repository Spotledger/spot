//! Turning the registry and journal model into the text and `jsonb` the tables store, and back.
//!
//! The model types carry no serde derives, so each shape is written out here with explicit,
//! tagged keys. Decoding is strict: an unknown tag, a missing key or a value of the wrong type is
//! a [`CORRUPT_ROW`](crate::error::CORRUPT_ROW) error, never a default, because a registry that
//! guesses at a half-readable row could hide an installed app or an approved capability.

use semver::{Version, VersionReq};
use serde_json::{Map, Value, json};
use spot_bundle::Digest;
use spot_installer::{
    DocTypeAccess, Endpoint, ExecError, FileAccess, GrantedCapabilities, InstalledApp,
    LoadedComponent, SiteLifecycle, SnapshotId, StepKind, StepState, Undo,
};
use spot_manifest::AppDependency;
use spot_storage_contract::Name;

use crate::error::corrupt;

type Decoded<T> = Result<T, ExecError>;

/// The text a lifecycle flag is stored as.
pub(crate) fn lifecycle_text(state: SiteLifecycle) -> &'static str {
    state.as_str()
}

/// Reads a lifecycle flag back.
pub(crate) fn lifecycle_from(text: &str) -> Decoded<SiteLifecycle> {
    match text {
        "idle" => Ok(SiteLifecycle::Idle),
        "installing" => Ok(SiteLifecycle::Installing),
        _ => Err(corrupt("a stored lifecycle flag is not known")),
    }
}

/// Reads a step kind back.
pub(crate) fn kind_from(text: &str) -> Decoded<StepKind> {
    const ALL: [StepKind; 8] = [
        StepKind::Snapshot,
        StepKind::Patch,
        StepKind::Fixture,
        StepKind::RemoveFixture,
        StepKind::Commit,
        StepKind::Load,
        StepKind::Unload,
        StepKind::Smoke,
    ];
    ALL.into_iter()
        .find(|kind| kind.as_str() == text)
        .ok_or_else(|| corrupt("a stored step kind is not known"))
}

/// Reads a step state back.
pub(crate) fn state_from(text: &str) -> Decoded<StepState> {
    match text {
        "started" => Ok(StepState::Started),
        "done" => Ok(StepState::Done),
        "undone" => Ok(StepState::Undone),
        _ => Err(corrupt("a stored step state is not known")),
    }
}

/// A `u32` as the `integer` column holds it.
pub(crate) fn to_i32(value: u32, what: &str) -> Decoded<i32> {
    i32::try_from(value).map_err(|_| corrupt(&format!("{what} is out of range")))
}

/// An `integer` column as a `u32`.
pub(crate) fn from_i32(value: i32, what: &str) -> Decoded<u32> {
    u32::try_from(value).map_err(|_| corrupt(&format!("a stored {what} is negative")))
}

/// Parses a stored digest.
pub(crate) fn digest_from(text: &str) -> Decoded<Digest> {
    text.parse::<Digest>()
        .map_err(|_| corrupt("a stored digest is not valid"))
}

/// Parses a stored version.
pub(crate) fn version_from(text: &str) -> Decoded<Version> {
    Version::parse(text).map_err(|_| corrupt("a stored version is not valid"))
}

fn object(value: &Value) -> Decoded<&Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| corrupt("a stored value is not an object"))
}

fn field<'a>(map: &'a Map<String, Value>, key: &str) -> Decoded<&'a Value> {
    map.get(key)
        .ok_or_else(|| corrupt(&format!("a stored value lacks `{key}`")))
}

fn text<'a>(map: &'a Map<String, Value>, key: &str) -> Decoded<&'a str> {
    field(map, key)?
        .as_str()
        .ok_or_else(|| corrupt(&format!("a stored `{key}` is not text")))
}

fn flag(map: &Map<String, Value>, key: &str) -> Decoded<bool> {
    field(map, key)?
        .as_bool()
        .ok_or_else(|| corrupt(&format!("a stored `{key}` is not a boolean")))
}

fn texts(value: &Value, what: &str) -> Decoded<Vec<String>> {
    value
        .as_array()
        .ok_or_else(|| corrupt(&format!("a stored {what} is not a list")))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| corrupt(&format!("a stored {what} entry is not text")))
        })
        .collect()
}

fn files_text(access: FileAccess) -> &'static str {
    match access {
        FileAccess::None => "none",
        FileAccess::Read => "read",
        FileAccess::ReadWrite => "read_write",
    }
}

fn files_from(text: &str) -> Decoded<FileAccess> {
    match text {
        "none" => Ok(FileAccess::None),
        "read" => Ok(FileAccess::Read),
        "read_write" => Ok(FileAccess::ReadWrite),
        _ => Err(corrupt("a stored file access is not known")),
    }
}

/// Approved capabilities as `jsonb`.
pub(crate) fn granted_to_json(granted: &GrantedCapabilities) -> Value {
    json!({
        "network": granted
            .network
            .iter()
            .map(|e| json!({"host": e.host, "port": e.port}))
            .collect::<Vec<_>>(),
        "email": granted.email,
        "files": files_text(granted.files),
        "doctypes": {"read": granted.doctypes.read, "write": granted.doctypes.write},
    })
}

/// Approved capabilities back from `jsonb`.
pub(crate) fn granted_from_json(value: &Value) -> Decoded<GrantedCapabilities> {
    let map = object(value)?;
    let network = field(map, "network")?
        .as_array()
        .ok_or_else(|| corrupt("stored network capabilities are not a list"))?
        .iter()
        .map(|item| {
            let entry = object(item)?;
            let port = field(entry, "port")?
                .as_u64()
                .and_then(|p| u16::try_from(p).ok())
                .ok_or_else(|| corrupt("a stored port is out of range"))?;
            Ok(Endpoint {
                host: text(entry, "host")?.to_owned(),
                port,
            })
        })
        .collect::<Decoded<Vec<_>>>()?;
    let doctypes = object(field(map, "doctypes")?)?;
    Ok(GrantedCapabilities {
        network,
        email: flag(map, "email")?,
        files: files_from(text(map, "files")?)?,
        doctypes: DocTypeAccess {
            read: texts(field(doctypes, "read")?, "readable DocType list")?,
            write: texts(field(doctypes, "write")?, "writable DocType list")?,
        },
    })
}

/// Patch ids as `jsonb`.
pub(crate) fn patches_to_json(patches: &[String]) -> Value {
    json!(patches)
}

/// Patch ids back from `jsonb`.
pub(crate) fn patches_from_json(value: &Value) -> Decoded<Vec<String>> {
    texts(value, "patch list")
}

/// Dependencies as `jsonb`.
pub(crate) fn depends_to_json(depends: &[AppDependency]) -> Value {
    Value::Array(
        depends
            .iter()
            .map(|d| json!({"app": d.app.as_str(), "version": d.version.to_string()}))
            .collect(),
    )
}

/// Dependencies back from `jsonb`.
pub(crate) fn depends_from_json(value: &Value) -> Decoded<Vec<AppDependency>> {
    value
        .as_array()
        .ok_or_else(|| corrupt("stored dependencies are not a list"))?
        .iter()
        .map(|item| {
            let entry = object(item)?;
            Ok(AppDependency {
                app: Name::new(text(entry, "app")?)
                    .map_err(|_| corrupt("a stored dependency name is not valid"))?,
                version: VersionReq::parse(text(entry, "version")?)
                    .map_err(|_| corrupt("a stored dependency range is not valid"))?,
            })
        })
        .collect()
}

fn component_to_json(component: &LoadedComponent) -> Value {
    json!({"version": component.version.to_string(), "digest": component.digest.to_hex()})
}

fn component_from_json(value: &Value) -> Decoded<LoadedComponent> {
    let map = object(value)?;
    Ok(LoadedComponent::new(
        version_from(text(map, "version")?)?,
        digest_from(text(map, "digest")?)?,
    ))
}

fn optional_component(value: &Value) -> Decoded<Option<LoadedComponent>> {
    if value.is_null() {
        Ok(None)
    } else {
        component_from_json(value).map(Some)
    }
}

fn optional_component_json(component: Option<&LoadedComponent>) -> Value {
    component.map_or(Value::Null, component_to_json)
}

/// A whole installed app (the `before` of a registry undo) as `jsonb`.
fn app_to_json(app: &InstalledApp) -> Value {
    json!({
        "name": app.name,
        "version": app.version.to_string(),
        "digest": app.digest.to_hex(),
        "approved": granted_to_json(&app.approved),
        "applied_patches": patches_to_json(&app.applied_patches),
        "order": app.order,
        "depends": depends_to_json(&app.depends),
    })
}

fn app_from_json(value: &Value) -> Decoded<InstalledApp> {
    let map = object(value)?;
    let order = field(map, "order")?
        .as_u64()
        .and_then(|o| u32::try_from(o).ok())
        .ok_or_else(|| corrupt("a stored order is out of range"))?;
    Ok(InstalledApp {
        name: text(map, "name")?.to_owned(),
        version: version_from(text(map, "version")?)?,
        digest: digest_from(text(map, "digest")?)?,
        approved: granted_from_json(field(map, "approved")?)?,
        applied_patches: patches_from_json(field(map, "applied_patches")?)?,
        order,
        depends: depends_from_json(field(map, "depends")?)?,
    })
}

/// A journal row's undo data as `jsonb`.
pub(crate) fn undo_to_json(undo: &Undo) -> Value {
    match undo {
        Undo::Snapshot(id) => json!({"tag": "snapshot", "id": id.as_str()}),
        Undo::Patch {
            app,
            patch_id,
            reversible,
        } => json!({"tag": "patch", "app": app, "patch_id": patch_id, "reversible": reversible}),
        Undo::FixtureLoaded { app, fixture } => {
            json!({"tag": "fixture_loaded", "app": app, "fixture": fixture})
        }
        Undo::FixtureRemoved { app, fixture } => {
            json!({"tag": "fixture_removed", "app": app, "fixture": fixture})
        }
        Undo::Registry {
            app,
            before,
            after,
            doctypes,
        } => json!({
            "tag": "registry",
            "app": app,
            "before": before.as_deref().map_or(Value::Null, app_to_json),
            "after": optional_component_json(after.as_ref()),
            "doctypes": doctypes,
        }),
        Undo::Component { app, before } => json!({
            "tag": "component",
            "app": app,
            "before": optional_component_json(before.as_ref()),
        }),
    }
}

/// A journal row's undo data back from `jsonb`.
pub(crate) fn undo_from_json(value: &Value) -> Decoded<Undo> {
    let map = object(value)?;
    let app = || text(map, "app").map(str::to_owned);
    match text(map, "tag")? {
        "snapshot" => Ok(Undo::Snapshot(SnapshotId::new(text(map, "id")?))),
        "patch" => Ok(Undo::Patch {
            app: app()?,
            patch_id: text(map, "patch_id")?.to_owned(),
            reversible: flag(map, "reversible")?,
        }),
        "fixture_loaded" => Ok(Undo::FixtureLoaded {
            app: app()?,
            fixture: text(map, "fixture")?.to_owned(),
        }),
        "fixture_removed" => Ok(Undo::FixtureRemoved {
            app: app()?,
            fixture: text(map, "fixture")?.to_owned(),
        }),
        "registry" => {
            let before = field(map, "before")?;
            Ok(Undo::Registry {
                app: app()?,
                before: if before.is_null() {
                    None
                } else {
                    Some(Box::new(app_from_json(before)?))
                },
                after: optional_component(field(map, "after")?)?,
                doctypes: texts(field(map, "doctypes")?, "DocType list")?,
            })
        }
        "component" => Ok(Undo::Component {
            app: app()?,
            before: optional_component(field(map, "before")?)?,
        }),
        _ => Err(corrupt("a stored undo tag is not known")),
    }
}

/// Reads one `spot_installer_app` row's columns back into the model.
pub(crate) struct AppColumns<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub digest: &'a str,
    pub approved: &'a Value,
    pub applied_patches: &'a Value,
    pub order: i32,
    pub depends: &'a Value,
}

impl AppColumns<'_> {
    /// The installed app the columns hold.
    pub(crate) fn decode(&self) -> Decoded<InstalledApp> {
        Ok(InstalledApp {
            name: self.name.to_owned(),
            version: version_from(self.version)?,
            digest: digest_from(self.digest)?,
            approved: granted_from_json(self.approved)?,
            applied_patches: patches_from_json(self.applied_patches)?,
            order: from_i32(self.order, "install order")?,
            depends: depends_from_json(self.depends)?,
        })
    }
}
