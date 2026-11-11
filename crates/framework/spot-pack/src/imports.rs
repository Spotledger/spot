//! The import table: which host import needs which capability.
//!
//! `spot-wasm`'s table is `pub(crate)` there and `spot-wasm` sits above this crate's
//! dependency direction, so the rows are mirrored here exactly as `spot-bundle`'s
//! privilege check spells them. When `spot-wasm` grows an import, the row lands in all
//! three places; the packer refusing an app whose component imports something the host
//! will not provide is the point of this check.

/// What one import needs from the manifest's capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    /// Nothing: every manifest may import it.
    Nothing,
    /// Read access to at least one DocType.
    ReadDocTypes,
    /// The host does not provide this import at all.
    Unknown,
}

/// The table, in `spot-wasm`'s / `spot-bundle`'s order.
const IMPORTS: &[(&str, Need)] = &[
    ("level", Need::Nothing),
    ("log", Need::Nothing),
    ("now", Need::Nothing),
    ("get-doc", Need::ReadDocTypes),
];

/// What `import` needs.
#[must_use]
pub fn need_of(import: &str) -> Need {
    IMPORTS
        .iter()
        .find(|(name, _)| *name == import)
        .map_or(Need::Unknown, |(_, need)| *need)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_mirrors_spot_bundle() {
        assert_eq!(need_of("log"), Need::Nothing);
        assert_eq!(need_of("get-doc"), Need::ReadDocTypes);
        assert_eq!(need_of("send-mail"), Need::Unknown);
    }
}
