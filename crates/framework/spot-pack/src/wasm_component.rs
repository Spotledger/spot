//! Parsing a WebAssembly component binary: imports, exports, and whether it parses at all.
//!
//! `spot-bundle` holds its walk `pub(crate)`, so this crate repeats the small one over
//! `wasmparser` (the same version and features the bundle uses): the top-level import and
//! export names of the outer component, which is all the packer needs.

/// One parsed component: its top-level import names and export names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// The import names, in declaration order.
    pub imports: Vec<String>,
    /// The export names, in declaration order.
    pub exports: Vec<String>,
}

/// Parses `bytes` as a WebAssembly **component**.
///
/// The walk is depth-aware: a component's sections sit next to the nested modules and
/// components it defines (`(core module ...)`, `(component ...)`), and the flat `parse_all`
/// stream yields the nested regions' payloads between theirs — each nested region opens with
/// a `ModuleSection`/`ComponentSection` and closes with its own `End`. Only the payloads at
/// nesting depth 0 are the component's own imports and exports.
///
/// # Errors
/// A message naming why the bytes are not a component this host accepts, in the same cases
/// `spot-bundle` refuses: wrong header, or a body `wasmparser` rejects.
pub fn parse(bytes: &[u8]) -> Result<Component, String> {
    const HEADER: [u8; 8] = [0x00, 0x61, 0x73, 0x6D, 0x0D, 0x00, 0x01, 0x00];
    if bytes.len() < HEADER.len() || bytes[..8] != HEADER {
        return Err(
            "app.wasm is not a WebAssembly component (the binary must start with the \
             component header 00 61 73 6d 0d 00 01 00)"
                .to_owned(),
        );
    }
    let mut imports = Vec::new();
    let mut exports = Vec::new();
    let mut depth = 0_usize;
    for payload in wasmparser::Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|err| err.to_string())?;
        match payload {
            // A nested module or component: its section payloads follow, down to its End.
            wasmparser::Payload::ModuleSection { .. }
            | wasmparser::Payload::ComponentSection { .. } => {
                depth += 1;
            }
            wasmparser::Payload::End(_) => {
                if depth == 0 {
                    break; // the component's own End: everything after is not its body
                }
                depth -= 1;
            }
            wasmparser::Payload::ComponentImportSection(reader) if depth == 0 => {
                for import in reader {
                    let import = import.map_err(|err| err.to_string())?;
                    imports.push(import.name.name.to_owned());
                }
            }
            wasmparser::Payload::ComponentExportSection(reader) if depth == 0 => {
                for export in reader {
                    let export = export.map_err(|err| err.to_string())?;
                    exports.push(export.name.name.to_owned());
                }
            }
            _ => {}
        }
    }
    Ok(Component { imports, exports })
}

/// Validates `bytes` fully, as `Bundle::open` will before accepting a bundle.
///
/// # Errors
/// The validator's message when the binary does not validate.
pub fn validate(bytes: &[u8]) -> Result<(), String> {
    let mut validator =
        wasmparser::Validator::new_with_features(wasmparser::WasmFeatures::default());
    validator
        .validate_all(bytes)
        .map(|_| ())
        .map_err(|err| err.to_string())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_core_module_is_not_a_component() {
        let module = wat_module();
        assert!(parse(&module).is_err());
    }

    fn wat_module() -> Vec<u8> {
        // A minimal core module, hand-built: magic, version 1.
        vec![0x00, 0x61, 0x73, 0x6D, 0x01, 0x00, 0x00, 0x00]
    }

    /// A component whose nested core module closes before the component's own export
    /// section: the flat stream yields the nested module's payloads and its `End` between
    /// the component's sections, and the walk must not stop at the nested `End`. The bytes
    /// were assembled once from this text (a `wat` dependency is not available here):
    ///
    /// ```text
    /// (component
    ///   (import "log" (func $f (param "message" string)))
    ///   (core module $libc (memory (export "memory") 1))
    ///   (core module $shim
    ///     (func (export "realloc") (param i32 i32 i32 i32) (result i32) (i32.const 0)))
    ///   (core instance $c (instantiate $libc))
    ///   (core instance $s (instantiate $shim))
    ///   (core func $f_l (canon lower (func $f)
    ///     (memory (core memory $c "memory"))
    ///     (realloc (core func $s "realloc"))
    ///     string-encoding=utf8))
    ///   (export "on-save" (core module $libc))
    /// )
    /// ```
    #[test]
    fn an_export_after_a_nested_module_is_seen() {
        #[rustfmt::skip]
        let bytes: Vec<u8> = vec![
            0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00, 0x07, 0x0e, 0x01, 0x40,
            0x01, 0x07, 0x6d, 0x65, 0x73, 0x73, 0x61, 0x67, 0x65, 0x73, 0x01, 0x00,
            0x0a, 0x08, 0x01, 0x00, 0x03, 0x6c, 0x6f, 0x67, 0x01, 0x00, 0x01, 0x27,
            0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x05, 0x03, 0x01, 0x00,
            0x01, 0x07, 0x0a, 0x01, 0x06, 0x6d, 0x65, 0x6d, 0x6f, 0x72, 0x79, 0x02,
            0x00, 0x00, 0x0c, 0x04, 0x6e, 0x61, 0x6d, 0x65, 0x00, 0x05, 0x04, 0x6c,
            0x69, 0x62, 0x63, 0x01, 0x3a, 0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00,
            0x00, 0x01, 0x09, 0x01, 0x60, 0x04, 0x7f, 0x7f, 0x7f, 0x7f, 0x01, 0x7f,
            0x03, 0x02, 0x01, 0x00, 0x07, 0x0b, 0x01, 0x07, 0x72, 0x65, 0x61, 0x6c,
            0x6c, 0x6f, 0x63, 0x00, 0x00, 0x0a, 0x06, 0x01, 0x04, 0x00, 0x41, 0x00,
            0x0b, 0x00, 0x0c, 0x04, 0x6e, 0x61, 0x6d, 0x65, 0x00, 0x05, 0x04, 0x73,
            0x68, 0x69, 0x6d, 0x02, 0x07, 0x02, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00,
            0x06, 0x18, 0x02, 0x00, 0x02, 0x01, 0x00, 0x06, 0x6d, 0x65, 0x6d, 0x6f,
            0x72, 0x79, 0x00, 0x00, 0x01, 0x01, 0x07, 0x72, 0x65, 0x61, 0x6c, 0x6c,
            0x6f, 0x63, 0x08, 0x0a, 0x01, 0x01, 0x00, 0x00, 0x03, 0x03, 0x00, 0x04,
            0x00, 0x00, 0x0b, 0x0e, 0x01, 0x00, 0x07, 0x6f, 0x6e, 0x2d, 0x73, 0x61,
            0x76, 0x65, 0x00, 0x11, 0x00, 0x00, 0x00, 0x3c, 0x0e, 0x63, 0x6f, 0x6d,
            0x70, 0x6f, 0x6e, 0x65, 0x6e, 0x74, 0x2d, 0x6e, 0x61, 0x6d, 0x65, 0x01,
            0x08, 0x00, 0x00, 0x01, 0x01, 0x03, 0x66, 0x5f, 0x6c, 0x01, 0x0f, 0x00,
            0x11, 0x02, 0x00, 0x04, 0x6c, 0x69, 0x62, 0x63, 0x01, 0x04, 0x73, 0x68,
            0x69, 0x6d, 0x01, 0x09, 0x00, 0x12, 0x02, 0x00, 0x01, 0x63, 0x01, 0x01,
            0x73, 0x01, 0x05, 0x01, 0x01, 0x00, 0x01, 0x66,
        ];
        let component = parse(&bytes).expect("a valid component parses");
        assert_eq!(component.imports, vec!["log".to_owned()]);
        assert_eq!(component.exports, vec!["on-save".to_owned()]);
    }
}
