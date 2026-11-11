//! Walking a validated component's import section with `wasmparser`.
//!
//! [`crate::Bundle::check_privileges`] needs the import names; [`crate::bundle_check`]'s
//! component validation needs the file to parse. This module does both walks over the
//! component's top-level sections.

use wasmparser::{Parser, Payload};

use crate::error::ComponentFault;

/// Every import name the component declares, in declaration order, at the top level.
///
/// A component may nest other components (they arrive as nested `Parser` scopes, which the
/// flat `for payload in Parser::new(...)` loop already skips, since only payloads of the
/// top scope have no `Section` indirection here — the outer loop sees the nested
/// `ComponentSection`'s reader only if we descend, which we do not). Imports of nested
/// components are not the host's business: the outer component's imports are.
pub(crate) fn component_import_names(wasm: &[u8]) -> Result<Vec<String>, ComponentFault> {
    let mut imports: Vec<String> = Vec::new();
    for payload in Parser::new(0).parse_all(wasm) {
        match payload.map_err(|err| ComponentFault::Invalid(err.to_string()))? {
            Payload::ComponentImportSection(reader) => {
                for import in reader {
                    let import = import.map_err(|err| ComponentFault::Invalid(err.to_string()))?;
                    imports.push(import.name.name.to_owned());
                }
            }
            // The import table is complete; stop at the first section that cannot add
            // imports, to keep the walk cheap.
            Payload::ComponentExportSection(_)
            | Payload::ComponentStartSection { .. }
            | Payload::End(_) => break,
            _ => {}
        }
    }
    Ok(imports)
}
