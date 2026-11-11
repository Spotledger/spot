//! Parsing the manifest's structured string values into model types.
//!
//! Every `[[...]]` row names what it attaches through a small set of well-known strings,
//! such as hook-point names (ADR-51) and session events. These are the single parsing points
//! for them; the manifest, the installer and its consumers all share them.

use semver::{Version, VersionReq};
use spot_app_api::lifecycle::op_name;
use spot_app_api::prelude::Name;
use spot_app_api::{HookPoint, Operation, OverrideKind, Phase, Target};

use crate::error::{INVALID_EVENT, INVALID_NAME, INVALID_SESSION_EVENT, ManifestError, PHASE_RULE};
use crate::model::{Endpoint, FileAccess, HttpVerb, PatchIntent, Queue, SessionEvent};

/// The ADR-51 operations, in contract order, for their event-name suffixes.
const OPS: [Operation; 7] = [
    Operation::Create,
    Operation::Update,
    Operation::Post,
    Operation::Cancel,
    Operation::Delete,
    Operation::Rename,
    Operation::UpdatePosted,
];

/// Parses an ADR-51 [`HookPoint`] from its name — a `[[events]]` `event` value.
///
/// Names are `{before,validate,after}_{create,update,post,cancel,delete,rename,update_posted}`
/// and `{before,validate,after}_save`, with the `committed` phase suffixed instead of
/// prefixed (`post_committed`), plus `load` and `field_changed:<field>`. Parsing proves the
/// round trip: for a valid `name`, `parse_hook_point(name)` returns a point whose `name()`
/// is `name`.
pub fn parse_hook_point(raw: &str) -> Result<HookPoint, ManifestError> {
    if raw == "load" {
        return Ok(HookPoint::Load);
    }
    if let Some(field) = raw.strip_prefix("field_changed:") {
        return match Name::new(field) {
            Ok(_) => Ok(HookPoint::FieldChanged(field.to_owned())),
            Err(_) => unknown_event(raw),
        };
    }
    for (prefix, phase) in [
        ("before", Phase::Before),
        ("validate", Phase::Validate),
        ("after", Phase::After),
    ] {
        if let Some(rest) = raw.strip_prefix(prefix).and_then(|r| r.strip_prefix('_')) {
            return match target(rest) {
                Some(target) => Ok(HookPoint::of(phase, target)),
                None => unknown_event(raw),
            };
        }
    }
    if let Some(rest) = raw.strip_suffix("_committed") {
        return match target(rest) {
            Some(target) => Ok(HookPoint::of(Phase::Committed, target)),
            None => unknown_event(raw),
        };
    }
    if let Some(rest) = raw.strip_prefix("committed_") {
        return Err(ManifestError::new(
            PHASE_RULE,
            format!(
                "`{raw}` is not an ADR-51 name: the phase comes first except for `committed`, \
             as in `{rest}`_committed"
            ),
        ));
    }
    unknown_event(raw)
}

fn unknown_event(raw: &str) -> Result<HookPoint, ManifestError> {
    Err(ManifestError::new(
        INVALID_EVENT,
        format!(
            "`{raw}` is not an ADR-51 hook-point name (before_create, validate_save, \
             post_committed, load, field_changed:<field>)"
        ),
    ))
}

fn target(name: &str) -> Option<Target> {
    if name == "save" {
        return Some(Target::Save);
    }
    OPS.iter()
        .copied()
        .find(|op| op_name(*op) == name)
        .map(Target::Op)
}

/// Parses an `[[overrides]]` `kind` name (ADR-08).
pub fn parse_override_kind(raw: &str) -> Result<OverrideKind, ManifestError> {
    match raw {
        "before" => Ok(OverrideKind::Before),
        "after" => Ok(OverrideKind::After),
        "wrap" => Ok(OverrideKind::Wrap),
        "replace" => Ok(OverrideKind::Replace),
        "disable" => Ok(OverrideKind::Disable),
        other => Err(ManifestError::new(
            crate::error::INVALID_OVERRIDE,
            format!("`{other}` is not an override kind (before, after, wrap, replace, disable)"),
        )),
    }
}

/// The three session events a manifest may subscribe to.
pub const SESSION_EVENT_NAMES: [&str; 3] = ["login", "logout", "session_created"];

/// Parses a session event name — a `[[session_events]]` `event` value.
pub fn parse_session_event(raw: &str) -> Result<SessionEvent, ManifestError> {
    match raw {
        "login" => Ok(SessionEvent::Login),
        "logout" => Ok(SessionEvent::Logout),
        "session_created" => Ok(SessionEvent::SessionCreated),
        other => Err(ManifestError::new(
            INVALID_SESSION_EVENT,
            format!("`{other}` is not a session event (login, logout, session_created)"),
        )),
    }
}

/// Parses a `[[schedules]]` `queue` name.
pub fn parse_queue(raw: &str) -> Result<Queue, ManifestError> {
    match raw {
        "default" => Ok(Queue::Default),
        "short" => Ok(Queue::Short),
        "long" => Ok(Queue::Long),
        other => Err(ManifestError::new(
            crate::error::INVALID_QUEUE,
            format!("`{other}` is not a queue; queues are default, short and long"),
        )),
    }
}

/// Parses an HTTP verb for `[[functions]]` `http`.
pub fn parse_verb(raw: &str) -> Result<HttpVerb, ManifestError> {
    match raw {
        "GET" => Ok(HttpVerb::Get),
        "POST" => Ok(HttpVerb::Post),
        "PUT" => Ok(HttpVerb::Put),
        "PATCH" => Ok(HttpVerb::Patch),
        "DELETE" => Ok(HttpVerb::Delete),
        "HEAD" => Ok(HttpVerb::Head),
        "OPTIONS" => Ok(HttpVerb::Options),
        other => Err(ManifestError::new(
            crate::error::INVALID_METHOD,
            format!("`{other}` is not an HTTP verb (GET, POST, PUT, PATCH, DELETE, HEAD, OPTIONS)"),
        )),
    }
}

/// Parses a `[[patches]]` `intent` name (ADR-22).
pub fn parse_intent(raw: &str) -> Result<PatchIntent, ManifestError> {
    match raw {
        "schema" => Ok(PatchIntent::Schema),
        "data" => Ok(PatchIntent::Data),
        "metadata" => Ok(PatchIntent::Metadata),
        other => Err(ManifestError::new(
            crate::error::INVALID_INTENT,
            format!("`{other}` is not a patch intent (schema, data, metadata)"),
        )),
    }
}

/// Parses a `[[capabilities]]` `files` value.
pub fn parse_file_access(raw: &str) -> Result<FileAccess, ManifestError> {
    match raw {
        "none" => Ok(FileAccess::None),
        "read" => Ok(FileAccess::Read),
        "read_write" => Ok(FileAccess::ReadWrite),
        other => Err(ManifestError::new(
            crate::error::INVALID_CAPABILITY,
            format!("`{other}` is not a file access level (none, read, read_write)"),
        )),
    }
}

/// Parses a `capabilities.network` entry: a host name or IPv4 address with a port, as
/// `host:port`.
pub fn parse_endpoint(raw: &str) -> Result<Endpoint, ManifestError> {
    let bad = |why: String| {
        Err(ManifestError::new(
            crate::error::INVALID_CAPABILITY,
            format!("network entry {raw:?}: {why} (example: erp.ledger.internal:443)"),
        ))
    };
    let Some((host, port)) = raw.rsplit_once(':') else {
        return bad("a network entry is `host:port`".to_owned());
    };
    if host.is_empty() || host.contains([':', '/', '\\', ' ', '[', ']']) {
        return bad("`host` must be a host name or IPv4 address".to_owned());
    }
    let port = match port.parse::<u16>() {
        Ok(0) | Err(_) => return bad("`port` must be between 1 and 65535".to_owned()),
        Ok(port) => port,
    };
    Ok(Endpoint {
        host: host.to_owned(),
        port,
    })
}

/// Builds a [`Name`] parse failure for a manifest value.
pub(crate) fn invalid_name(what: &str, raw: &str) -> ManifestError {
    ManifestError::new(
        INVALID_NAME,
        format!(
            "`{raw}` is not a valid {what} (lowercase letters, digits and `_`, starting with a letter)"
        ),
    )
}

/// Builds a semver requirement parse failure for a manifest value.
pub(crate) fn invalid_req(what: &str, raw: &str) -> ManifestError {
    ManifestError::new(
        crate::error::INVALID_VERSION,
        format!("`{raw}` is not a valid {what} (a semver requirement, like `0.3` or `>=0.2`)"),
    )
}

/// Builds a semver version parse failure for a manifest value.
pub(crate) fn invalid_version(what: &str, raw: &str) -> ManifestError {
    ManifestError::new(
        crate::error::INVALID_VERSION,
        format!("`{raw}` is not a valid {what} (a semver version, like `0.3.1`)"),
    )
}

/// Parses a semver version requirement.
pub fn parse_version_req(raw: &str, what: &str) -> Result<VersionReq, ManifestError> {
    VersionReq::parse(raw).map_err(|_| invalid_req(what, raw))
}

/// Parses a semver version.
pub fn parse_version(raw: &str, what: &str) -> Result<Version, ManifestError> {
    Version::parse(raw).map_err(|_| invalid_version(what, raw))
}
