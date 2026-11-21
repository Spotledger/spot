//! [`FunctionKey`]: the `app/module/name` address of a whitelisted function, and
//! [`ModuleKey`]: the `app/module` address of one declared module.

use std::fmt;

/// The longest storage-style name in bytes (the PostgreSQL identifier limit).
pub const NAME_MAX_BYTES: usize = 63;

/// The address of one whitelisted function: `app/module/name` (ADR-16), every part a
/// storage-style name.
///
/// Built from already validated parts ([`FunctionKey::from_parts`], what
/// [`FunctionRegistryBuilder::register`] uses on the manifest's `Name`s) or validated raw
/// ([`FunctionKey::parse`], what a transport uses on URL segments). The `Display` form is
/// `app/module/name`, the path the function URL uses. All three parts are lower-case, so the
/// key's `Ord`/`Hash` are canonical.
///
/// [`FunctionRegistryBuilder::register`]: crate::FunctionRegistryBuilder::register
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FunctionKey {
    /// The app that declares the function.
    pub app: String,
    /// The module the function lives in.
    pub module: String,
    /// The function's name within its module.
    pub name: String,
}

impl FunctionKey {
    /// Builds a key from already validated parts: every one must be a storage-style name, as
    /// the manifest's `Name` guarantees for functions declared there.
    pub fn from_parts(app: &str, module: &str, name: &str) -> FunctionKey {
        debug_assert!(is_storage_name(app) && is_storage_name(module) && is_storage_name(name));
        FunctionKey {
            app: app.to_owned(),
            module: module.to_owned(),
            name: name.to_owned(),
        }
    }

    /// Validates three raw strings and builds a key from them.
    pub fn parse(app: &str, module: &str, name: &str) -> Result<FunctionKey, InvalidFunctionKey> {
        let parts = [("app", app), ("module", module), ("name", name)];
        for (what, part) in parts {
            if !is_storage_name(part) {
                return Err(InvalidFunctionKey {
                    key: format!("{app}/{module}/{name}"),
                    part: what,
                    input: part.to_owned(),
                });
            }
        }
        Ok(FunctionKey::from_parts(app, module, name))
    }

    /// The app part.
    pub fn app(&self) -> &str {
        &self.app
    }

    /// The module part.
    pub fn module(&self) -> &str {
        &self.module
    }

    /// The function part.
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl fmt::Display for FunctionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.app, self.module, self.name)
    }
}

/// The address of one declared module: `app/module` (ADR-16). [`App::add_functions`] and the
/// conflicting-apps question are asked about modules; the registry's module list is a list of
/// these.
///
/// [`App::add_functions`]: crate::FunctionRegistryBuilder::add_functions
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ModuleKey {
    /// The app that declares the module.
    pub app: String,
    /// The module's name.
    pub module: String,
}

impl ModuleKey {
    /// Builds a module key from already validated parts.
    pub fn from_parts(app: &str, module: &str) -> ModuleKey {
        debug_assert!(is_storage_name(app) && is_storage_name(module));
        ModuleKey {
            app: app.to_owned(),
            module: module.to_owned(),
        }
    }

    /// Validates two raw strings and builds a module key from them.
    pub fn parse(app: &str, module: &str) -> Result<ModuleKey, InvalidFunctionKey> {
        let parts = [("app", app), ("module", module)];
        for (what, part) in parts {
            if !is_storage_name(part) {
                return Err(InvalidFunctionKey {
                    key: format!("{app}/{module}"),
                    part: what,
                    input: part.to_owned(),
                });
            }
        }
        Ok(ModuleKey::from_parts(app, module))
    }
}

impl fmt::Display for ModuleKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.app, self.module)
    }
}

impl From<&ModuleKey> for ModuleKey {
    fn from(m: &ModuleKey) -> ModuleKey {
        m.clone()
    }
}

impl From<&FunctionKey> for ModuleKey {
    fn from(k: &FunctionKey) -> ModuleKey {
        ModuleKey::from_parts(&k.app, &k.module)
    }
}

/// Why a string is not a valid [`FunctionKey`] or [`ModuleKey`] part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidFunctionKey {
    /// The attempted key, in its `/`-joined shape.
    pub key: String,
    /// Which part failed: `app`, `module` or `name`.
    pub part: &'static str,
    /// The rejected input.
    pub input: String,
}

impl fmt::Display for InvalidFunctionKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} in key {:?}: {:?} is not a storage-style name (lower-case letters, digits and \
             underscores, starting with a letter)",
            self.part, self.key, self.input
        )
    }
}

impl std::error::Error for InvalidFunctionKey {}

/// Whether `s` is a storage-style name: what the manifest's `Name` and the storage contract
/// accept — lowercase letters, digits and underscores, starting with a letter, at most
/// [`NAME_MAX_BYTES`] bytes (the PostgreSQL identifier limit).
pub fn is_storage_name(s: &str) -> bool {
    let mut chars = s.chars();
    s.len() <= NAME_MAX_BYTES
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}
