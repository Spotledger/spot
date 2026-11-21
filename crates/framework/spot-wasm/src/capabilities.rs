//! What an app has been granted. A plain struct, so this crate needs no manifest dependency: the
//! manifest-aware layer converts an app's declared capabilities into this one.

/// A host and port an app may call over HTTP.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Endpoint {
    /// The host name or IPv4 address.
    pub host: String,
    /// The port.
    pub port: u16,
}

/// The file-store access an app was granted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum FileAccess {
    /// None (the default).
    #[default]
    None,
    /// Read only.
    Read,
    /// Read and write.
    ReadWrite,
}

/// The DocTypes an app may read and change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocTypeAccess {
    /// DocTypes the app may read.
    pub read: Vec<String>,
    /// DocTypes the app may change.
    pub write: Vec<String>,
}

/// What an app may touch. The default is nothing: every field is the least access.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrantedCapabilities {
    /// Hosts and ports the app may call.
    pub network: Vec<Endpoint>,
    /// Whether the app may send e-mail.
    pub email: bool,
    /// File-store access.
    pub files: FileAccess,
    /// DocType access.
    pub doctypes: DocTypeAccess,
}

/// The imports of the test world and what each needs. The product world's imports are added here
/// when it exists (WP-EXT-05.T2).
///
/// - `log` and `now` carry no authority: logging goes to the host's sink, and `now` is the time of
///   the invocation, not the machine's clock.
/// - `get-doc` needs read access to at least one DocType; each call is then checked against the
///   DocTypes granted.
pub(crate) const IMPORTS: &[(&str, Need)] = &[
    // The world's `level` type: a type import carries no authority.
    ("level", Need::Nothing),
    ("log", Need::Nothing),
    ("now", Need::Nothing),
    ("get-doc", Need::ReadDocTypes),
];

/// What an import needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Need {
    /// Nothing: every app may import it.
    Nothing,
    /// Read access to at least one DocType.
    ReadDocTypes,
}

impl GrantedCapabilities {
    /// Why `import` may not be linked for this app, or `None` if it may. An import the host does
    /// not provide at all (a WASI interface, an unknown function) is denied too: there is no
    /// ambient authority.
    pub(crate) fn denial(&self, import: &str) -> Option<String> {
        match IMPORTS.iter().find(|(name, _)| *name == import) {
            None => Some(format!(
                "the app imports `{import}`, which the host does not provide"
            )),
            Some((_, Need::Nothing)) => None,
            Some((_, Need::ReadDocTypes)) if !self.doctypes.read.is_empty() => None,
            Some((_, Need::ReadDocTypes)) => Some(format!(
                "the app imports `{import}` but was granted no DocType read access"
            )),
        }
    }

    /// Whether the app may read documents of `doctype`.
    pub fn may_read(&self, doctype: &str) -> bool {
        self.doctypes.read.iter().any(|d| d == doctype)
    }
}
