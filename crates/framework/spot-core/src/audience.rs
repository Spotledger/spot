//! Audiences (ADR-083 section 2.3): which kind of caller a route, a session or a listener is
//! for.
//!
//! Three audiences exist: [`Audience::Staff`] (internal, service and agent users),
//! [`Audience::Partner`] (portal users) and [`Audience::Consumer`] (a separate principal that
//! the staff login path never issues). There is no default audience and no "any" value: a
//! caller that needs one spells it out, and text that is not exactly one of the three names is
//! refused.

use std::fmt;
use std::str::FromStr;

/// The audience of a route, a session or a listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Audience {
    /// Internal, service and agent users.
    Staff,
    /// Portal users.
    Partner,
    /// The consumer principal (a later task issues it; no staff login can).
    Consumer,
}

/// The text is not the name of an audience.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidAudience;

impl fmt::Display for InvalidAudience {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The rejected text is not echoed: it may be arbitrarily long.
        f.write_str("not an audience: expected `staff`, `partner` or `consumer`")
    }
}

impl std::error::Error for InvalidAudience {}

impl Audience {
    /// Every audience, in a fixed order.
    pub const ALL: [Audience; 3] = [Audience::Staff, Audience::Partner, Audience::Consumer];

    /// The stable lower-case name.
    pub const fn as_str(self) -> &'static str {
        match self {
            Audience::Staff => "staff",
            Audience::Partner => "partner",
            Audience::Consumer => "consumer",
        }
    }

    /// The audience named exactly `text` (case sensitive, no trimming); `None` for anything
    /// else.
    pub fn parse(text: &str) -> Option<Audience> {
        match text {
            "staff" => Some(Audience::Staff),
            "partner" => Some(Audience::Partner),
            "consumer" => Some(Audience::Consumer),
            _ => None,
        }
    }
}

impl FromStr for Audience {
    type Err = InvalidAudience;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Audience::parse(text).ok_or(InvalidAudience)
    }
}

impl fmt::Display for Audience {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for audience in Audience::ALL {
            assert_eq!(Audience::parse(audience.as_str()), Some(audience));
            assert_eq!(audience.as_str().parse::<Audience>(), Ok(audience));
            assert_eq!(audience.to_string(), audience.as_str());
        }
    }

    #[test]
    fn near_misses_are_refused() {
        for text in ["Staff", " staff", "staff ", "", "admin", "STAFF", "staff\n"] {
            assert_eq!(Audience::parse(text), None, "{text:?}");
        }
    }
}
