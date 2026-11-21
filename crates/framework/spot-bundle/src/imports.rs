//! The import table: which host import needs which capability, mirroring `spot-wasm`'s
//! `IMPORTS` (which this crate cannot `use`: it is `pub(crate)` there and `spot-wasm` sits
//! above the framework layer's dependency direction here).
//!
//! When `spot-wasm` grows a new import (WP-EXT-05.T2), the same row lands here — the
//! manifest-aware privilege check and the runtime's linker must never disagree.

/// One host import and the capability it needs.
///
/// The order mirrors `spot-wasm`'s `IMPORTS` table.
pub(crate) const IMPORTS: &[(&str, Need)] = &[
    // The world's `level` type: a type import carries no authority.
    ("level", Need::Nothing),
    // Logging goes to the host's sink.
    ("log", Need::Nothing),
    // The time of the invocation, not the machine's clock.
    ("now", Need::Nothing),
    // Reading a document needs read access to at least one DocType.
    ("get-doc", Need::ReadDocTypes),
];

/// What an import needs from the manifest's capabilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need {
    /// Nothing: every manifest may import it.
    Nothing,
    /// Read access to at least one DocType.
    ReadDocTypes,
}
