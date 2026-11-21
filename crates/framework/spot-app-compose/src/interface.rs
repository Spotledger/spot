//! The composition model: exported and imported interfaces, the capability a provider grants to
//! importers, and the per-app input `resolve` reads.
//!
//! The manifest declares hooks, seams and capabilities today, but it has no field for an app's
//! **exported** or **imported** interfaces yet, so this crate models them as plain data (the
//! manifest extension is proposed under the task's open questions). An `AppInterfaces` value is
//! everything `resolve` needs about one app: its identity, what it exports, what it imports and
//! the capabilities it declares.

use std::cmp::Ordering;

use semver::{Version, VersionReq};
use spot_manifest::{Capabilities, DocTypeAccess, Endpoint, FileAccess};

/// One capability an app holds, obtains through a provider, or grants to importers.
///
/// This is the normalised form of a manifest [`Capabilities`] set: one value per endpoint,
/// DocType and flag, so two sets can be compared and subtracted. The ordering is fixed (e-mail,
/// files, network, read, write) so reports are deterministic.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Capability {
    /// The app may send e-mail.
    Email,
    /// The app has file-store access.
    Files(FileAccess),
    /// The app may call `host:port`.
    Network {
        /// The host name or IPv4 address.
        host: String,
        /// The port.
        port: u16,
    },
    /// The app may read documents of the named DocType.
    ReadDocType(String),
    /// The app may change documents of the named DocType.
    WriteDocType(String),
}

impl Capability {
    /// Flattens a manifest capability set into individual capabilities.
    #[must_use]
    pub fn from_capabilities(caps: &Capabilities) -> Vec<Capability> {
        let mut out = Vec::new();
        if caps.email {
            out.push(Capability::Email);
        }
        if caps.files != FileAccess::None {
            out.push(Capability::Files(caps.files));
        }
        for Endpoint { host, port } in &caps.network {
            out.push(Capability::Network {
                host: host.clone(),
                port: *port,
            });
        }
        for d in &caps.doctypes.read {
            out.push(Capability::ReadDocType(d.clone()));
        }
        for d in &caps.doctypes.write {
            out.push(Capability::WriteDocType(d.clone()));
        }
        out
    }

    /// A plain description, for the message of a refusal.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Capability::Email => "e-mail".to_owned(),
            Capability::Files(access) => format!("file access {}", file_name(*access)),
            Capability::Network { host, port } => format!("network {host}:{port}"),
            Capability::ReadDocType(d) => format!("read {d}"),
            Capability::WriteDocType(d) => format!("write {d}"),
        }
    }

    /// The fixed sort key: kind rank, then a string, then a port.
    fn sort_key(&self) -> (u8, String, u16) {
        match self {
            Capability::Email => (0, String::new(), 0),
            Capability::Files(access) => (1, file_name(*access).to_owned(), 0),
            Capability::Network { host, port } => (2, host.clone(), *port),
            Capability::ReadDocType(d) => (3, d.clone(), 0),
            Capability::WriteDocType(d) => (4, d.clone(), 0),
        }
    }
}

impl PartialOrd for Capability {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Capability {
    fn cmp(&self, other: &Self) -> Ordering {
        self.sort_key().cmp(&other.sort_key())
    }
}

/// The stable name of a file access, for messages and the sort key.
fn file_name(access: FileAccess) -> &'static str {
    match access {
        FileAccess::None => "none",
        FileAccess::Read => "read",
        FileAccess::ReadWrite => "read_write",
    }
}

/// A type an interface function takes or returns. No float: money and quantities are decimals,
/// never binary floats (ADR-12).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TypeRef {
    /// A boolean.
    Bool,
    /// A signed 64-bit integer.
    S64,
    /// An unsigned 64-bit integer.
    U64,
    /// A UTF-8 string.
    String,
    /// A byte string.
    Bytes,
    /// A list of another type.
    List(Box<TypeRef>),
    /// An optional value.
    Option(Box<TypeRef>),
    /// A named record.
    Record(String),
}

/// One parameter of an interface function.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParamSig {
    /// The parameter name, as the interface declares it.
    pub name: String,
    /// Its type.
    pub ty: TypeRef,
}

impl ParamSig {
    /// A parameter from its name and type.
    #[must_use]
    pub fn new(name: &str, ty: TypeRef) -> Self {
        Self {
            name: name.to_owned(),
            ty,
        }
    }
}

/// One function an exported interface offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionSig {
    /// The function name.
    pub name: String,
    /// Its parameters, in order.
    pub params: Vec<ParamSig>,
    /// Its result type, when it returns one.
    pub result: Option<TypeRef>,
}

impl FunctionSig {
    /// A function from its name.
    #[must_use]
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            params: Vec::new(),
            result: None,
        }
    }

    /// Adds a parameter.
    #[must_use]
    pub fn param(mut self, name: &str, ty: TypeRef) -> Self {
        self.params.push(ParamSig::new(name, ty));
        self
    }

    /// Sets the result type.
    #[must_use]
    pub fn result(mut self, ty: TypeRef) -> Self {
        self.result = Some(ty);
        self
    }
}

/// An interface one app exports: `{ app, interface, version, functions }`, plus the capabilities
/// it grants to importers.
///
/// `grants` is the modelled extension of the brief's four fields: a provider states what an
/// importer may obtain through this interface, so an extension cannot escalate beyond it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExportedInterface {
    /// The exporting app.
    pub app: String,
    /// The interface name, such as `pricing.adjust`.
    pub interface: String,
    /// Its semver version.
    pub version: Version,
    /// The functions it offers.
    pub functions: Vec<FunctionSig>,
    /// What it grants to an importer of this interface.
    pub grants: Vec<Capability>,
}

impl ExportedInterface {
    /// An export of `interface` at `version`, from `app`, granting nothing.
    #[must_use]
    pub fn new(app: &str, interface: &str, version: Version) -> Self {
        Self {
            app: app.to_owned(),
            interface: interface.to_owned(),
            version,
            functions: Vec::new(),
            grants: Vec::new(),
        }
    }

    /// Adds a function.
    #[must_use]
    pub fn function(mut self, function: FunctionSig) -> Self {
        self.functions.push(function);
        self
    }

    /// Grants one more capability to importers.
    #[must_use]
    pub fn grant(mut self, capability: Capability) -> Self {
        self.grants.push(capability);
        self
    }
}

/// An interface one app imports: `{ app, interface, version_range, optional }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedInterface {
    /// The importing app.
    pub app: String,
    /// The interface name it needs.
    pub interface: String,
    /// The versions of the interface it accepts.
    pub version_range: VersionReq,
    /// Whether the app still works when no provider is installed.
    pub optional: bool,
}

impl ImportedInterface {
    /// A required import of `interface` matching `version_range`.
    #[must_use]
    pub fn new(app: &str, interface: &str, version_range: VersionReq) -> Self {
        Self {
            app: app.to_owned(),
            interface: interface.to_owned(),
            version_range,
            optional: false,
        }
    }

    /// Marks the import optional: a missing provider is not a problem.
    #[must_use]
    pub fn optional(mut self) -> Self {
        self.optional = true;
        self
    }
}

/// Everything `resolve` reads about one app: its identity, exports, imports and declared
/// capabilities.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppInterfaces {
    /// The app's storage name.
    pub app: String,
    /// Its version.
    pub version: Version,
    /// The interfaces it exports.
    pub exports: Vec<ExportedInterface>,
    /// The interfaces it imports.
    pub imports: Vec<ImportedInterface>,
    /// The capabilities its manifest declares.
    pub declared: Capabilities,
}

impl AppInterfaces {
    /// An app with no exports, imports or declared capabilities.
    #[must_use]
    pub fn new(app: &str, version: Version) -> Self {
        Self {
            app: app.to_owned(),
            version,
            exports: Vec::new(),
            imports: Vec::new(),
            declared: Capabilities::default(),
        }
    }

    /// Adds an export.
    #[must_use]
    pub fn export(mut self, export: ExportedInterface) -> Self {
        self.exports.push(export);
        self
    }

    /// Adds an import.
    #[must_use]
    pub fn import(mut self, import: ImportedInterface) -> Self {
        self.imports.push(import);
        self
    }

    /// Sets the declared capabilities.
    #[must_use]
    pub fn declared(mut self, caps: Capabilities) -> Self {
        self.declared = caps;
        self
    }

    /// The DocType read/write access as a manifest value, for a caller building capabilities.
    #[must_use]
    pub fn doctype_access(read: &[&str], write: &[&str]) -> DocTypeAccess {
        DocTypeAccess {
            read: read.iter().map(|s| (*s).to_owned()).collect(),
            write: write.iter().map(|s| (*s).to_owned()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_flatten_and_sort_stably() {
        let caps = Capabilities {
            email: true,
            files: FileAccess::Read,
            network: vec![Endpoint {
                host: "api.example".to_owned(),
                port: 443,
            }],
            doctypes: DocTypeAccess {
                read: vec!["Customer".to_owned()],
                write: vec!["Sales Invoice".to_owned()],
            },
        };
        let mut flat = Capability::from_capabilities(&caps);
        flat.sort();
        let described: Vec<String> = flat.iter().map(Capability::describe).collect();
        assert_eq!(
            described,
            vec![
                "e-mail".to_owned(),
                "file access read".to_owned(),
                "network api.example:443".to_owned(),
                "read Customer".to_owned(),
                "write Sales Invoice".to_owned(),
            ]
        );
        assert_eq!(flat, Capability::from_capabilities(&caps));
    }
}
