//! Room identity: a validated structured name, scoped to a site by the bus.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use thiserror::Error;

/// Hard limit on each validated part of a room name, in bytes.
const MAX_PART_LEN: usize = 140;

/// The name of the site-wide room.
const SITE_ROOM: &str = "site";

/// Why a string is not a valid [`Room`] name.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("invalid room name {name:?}: {reason}")]
pub struct InvalidRoom {
    /// The rejected input.
    pub name: String,
    /// Plain-language reason.
    pub reason: &'static str,
}

/// A validated room name: `doc:<doctype>:<id>`, `doctype:<doctype>`, `user:<user>` or `site`.
///
/// Rooms are built through the constructors ([`Room::document`], [`Room::doctype`],
/// [`Room::user`], [`Room::site`]), never as free strings; [`Room::parse`] exists so a name
/// arriving over the wire (cross-node fan-out) is re-validated before use. Parts may not
/// contain `:` or control characters, may not be empty, and are at most 140 bytes.
///
/// A room name alone is not a room: the bus scopes every room to a
/// [`SiteId`](spot_core::SiteId), so the same name on two sites is two unrelated rooms.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Room(Arc<str>);

impl Room {
    /// The per-document room: `doc:<doctype>:<id>`.
    pub fn document(doctype: &str, id: &str) -> Result<Self, InvalidRoom> {
        build(&["doc", doctype, id])
    }

    /// The doctype feed room: `doctype:<doctype>`.
    pub fn doctype(doctype: &str) -> Result<Self, InvalidRoom> {
        build(&["doctype", doctype])
    }

    /// The per-user room: `user:<user>`.
    pub fn user(user: &str) -> Result<Self, InvalidRoom> {
        build(&["user", user])
    }

    /// The site-wide room: `site`. Always valid; receives site-level notifications.
    pub fn site() -> Self {
        // The fixed name passes every check below by construction.
        Self(Arc::from(SITE_ROOM))
    }

    /// Re-validates a room name that arrived as a free string, for example from the wire or
    /// from another node's invalidation bus. Accepts exactly the four constructor forms.
    pub fn parse(name: &str) -> Result<Self, InvalidRoom> {
        if name == SITE_ROOM {
            return Ok(Self::site());
        }
        let parts: Vec<&str> = name.split(':').collect();
        let shape_ok = match parts.first() {
            Some(p) if *p == "doc" => parts.len() == 3,
            Some(p) if *p == "doctype" || *p == "user" => parts.len() == 2,
            _ => false,
        };
        if shape_ok {
            build(&parts)
        } else {
            Err(InvalidRoom {
                name: name.to_string(),
                reason: "a room is `site`, `doc:<doctype>:<id>`, `doctype:<doctype>` or `user:<user>`",
            })
        }
    }

    /// The room name, exactly as the bus keys it.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn from_shared(name: String) -> Self {
        Self(Arc::from(name.as_str()))
    }
}

/// Validates every part of a room and assembles the canonical name.
fn build(parts: &[&str]) -> Result<Room, InvalidRoom> {
    for part in parts {
        if let Err(reason) = check_part(part) {
            return Err(InvalidRoom {
                name: parts.join(":"),
                reason,
            });
        }
    }
    Ok(Room::from_shared(parts.join(":")))
}

fn check_part(part: &str) -> Result<(), &'static str> {
    if part.is_empty() {
        return Err("a part is empty");
    }
    if part.len() > MAX_PART_LEN {
        return Err("a part is longer than 140 bytes");
    }
    if part.contains(':') {
        return Err("a part contains ':'");
    }
    if part.chars().any(char::is_control) {
        return Err("a part contains a control character");
    }
    Ok(())
}

impl fmt::Display for Room {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for Room {
    type Err = InvalidRoom;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[cfg(test)]
mod tests {
    use super::Room;

    #[test]
    fn constructors_build_canonical_names() {
        assert_eq!(
            Room::document("DocType One", "abc-123")
                .map(|r| r.as_str().to_string())
                .as_deref(),
            Ok("doc:DocType One:abc-123")
        );
        assert_eq!(
            Room::doctype("Item")
                .map(|r| r.as_str().to_string())
                .as_deref(),
            Ok("doctype:Item")
        );
        assert_eq!(
            Room::user("admin@example.com")
                .map(|r| r.as_str().to_string())
                .as_deref(),
            Ok("user:admin@example.com")
        );
        assert_eq!(Room::site().as_str(), "site");
    }

    #[test]
    fn parts_reject_colons_control_chars_emptiness_and_length() {
        assert!(Room::document("a:b", "c").is_err());
        assert!(Room::document("a", "c:d").is_err());
        assert!(Room::doctype("").is_err());
        assert!(Room::document("", "id").is_err());
        assert!(Room::user("bad\nname").is_err());
        let long = "x".repeat(141);
        assert!(Room::doctype(&long).is_err());
        let ok = "x".repeat(140);
        assert!(Room::doctype(&ok).is_ok());
    }

    #[test]
    fn parse_accepts_only_the_four_forms() {
        assert_eq!(Room::parse("site"), Ok(Room::site()));
        assert!(Room::parse("doc:a:1").is_ok());
        assert!(Room::parse("doctype:A").is_ok());
        assert!(Room::parse("user:bob").is_ok());
        for bad in [
            "",
            "other:x",
            "doc:a",
            "doc:a:b:c",
            "user:",
            "Site",
            "site:extra",
        ] {
            assert!(Room::parse(bad).is_err(), "{bad:?} should be rejected");
        }
        assert!(Room::parse("doc:a:\u{1}").is_err());
    }
}
