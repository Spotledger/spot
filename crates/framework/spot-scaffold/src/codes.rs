//! The `scaffold.*` code vocabulary. Every code is catalogued in
//! `crates/framework/spot-errors/catalogue/scaffold.toml` (the error-code scan checks both
//! directions: a raised code with no entry, and an entry nothing raises, both fail the gate).

/// `scaffold.io`: a file, directory or stream could not be read or written.
pub const IO: &str = "scaffold.io";
/// `scaffold.path_escape`: a path is absolute, holds `..`, or leaves the output root.
pub const PATH_ESCAPE: &str = "scaffold.path_escape";
/// `scaffold.symlink`: a symlink sits on the path a scaffold would write through.
pub const SYMLINK: &str = "scaffold.symlink";
/// `scaffold.exists`: a file would be overwritten without `--force`.
pub const EXISTS: &str = "scaffold.exists";
/// `scaffold.invalid_name`: a name is not a valid storage name.
pub const INVALID_NAME: &str = "scaffold.invalid_name";
/// `scaffold.reserved_name`: a name is a reserved word or platform prefix.
pub const RESERVED_NAME: &str = "scaffold.reserved_name";
/// `scaffold.duplicate`: the name already exists in the app.
pub const DUPLICATE: &str = "scaffold.duplicate";
/// `scaffold.template`: a template could not be rendered.
pub const TEMPLATE: &str = "scaffold.template";
/// `scaffold.manifest`: an existing manifest could not be parsed or edited.
pub const MANIFEST: &str = "scaffold.manifest";
/// `scaffold.not_an_app`: the output directory is not a scaffolded app.
pub const NOT_AN_APP: &str = "scaffold.not_an_app";
/// `scaffold.unknown_doctype`: the app has no DocType by that name.
pub const UNKNOWN_DOCTYPE: &str = "scaffold.unknown_doctype";
/// `scaffold.usage`: the command line was used incorrectly.
pub const USAGE: &str = "scaffold.usage";

/// Every code this crate raises, in catalogue order.
pub const ALL: [&str; 12] = [
    IO,
    PATH_ESCAPE,
    SYMLINK,
    EXISTS,
    INVALID_NAME,
    RESERVED_NAME,
    DUPLICATE,
    TEMPLATE,
    MANIFEST,
    NOT_AN_APP,
    UNKNOWN_DOCTYPE,
    USAGE,
];
