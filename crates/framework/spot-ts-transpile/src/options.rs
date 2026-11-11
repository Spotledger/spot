/// Script-tier knobs for [`transpile::transpile`]; everything else about the
/// output is fixed by the engine tier (see the crate README).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Module specifiers an import (or `export ... from`) may name.
    ///
    /// A pattern is a literal module name, or a prefix ending in `*`
    /// (`spot:*` matches any specifier that starts with `spot:`). The engine
    /// has no module loader: anything not on the list is rejected on save
    /// with `ts.import_not_allowed` (the default is only the `spot:*`
    /// virtual modules built into the engine).
    pub allow_imports: Vec<String>,
    /// Maximum source size in bytes (default 1 MiB, the save-time cap).
    pub source_size_cap: usize,
    /// Maximum bracket nesting depth the parser is given (default 128);
    /// deeper input is rejected without attempting to parse it.
    pub nesting_depth_cap: u32,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            allow_imports: vec!["spot:*".to_string()],
            source_size_cap: 1_048_576,
            nesting_depth_cap: 128,
        }
    }
}

impl Options {
    /// Does the allow-list admit this module specifier?
    pub fn allows_import(&self, module: &str) -> bool {
        self.allow_imports
            .iter()
            .any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => module.starts_with(prefix),
                None => module == pattern.as_str(),
            })
    }
}
