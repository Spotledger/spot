//! Scans `src/handlers/*.rs` and generates the handler registry (the pattern of
//! WP-PRD-01.T26): every file declares `pub const NAMESPACE: &str` and
//! `pub fn register(reg: &mut HandlerRegistry, ctx: &HandlerCtx)`; a later handler set is a
//! new file, nothing else changes.

use std::fmt::Write as _;
use std::path::Path;

fn namespace_of(source: &str) -> Option<String> {
    let line = source
        .lines()
        .find(|l| l.trim_start().starts_with("pub const NAMESPACE: &str"))?;
    let start = line.find('"')? + 1;
    let end = line[start..].find('"')? + start;
    Some(line[start..end].to_owned())
}

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
    let out_dir = std::env::var("OUT_DIR").unwrap_or_default();
    let dir = Path::new(&manifest).join("src").join("handlers");
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut entries: Vec<(String, String, String)> = Vec::new();
    let mut catalogues: Vec<String> = Vec::new();
    if let Ok(read) = std::fs::read_dir(&dir) {
        for entry in read.flatten() {
            let path = entry.path();
            let (Some(stem), Some("rs")) = (
                path.file_stem().and_then(|s| s.to_str()),
                path.extension().and_then(|s| s.to_str()),
            ) else {
                continue;
            };
            if stem == "mod" {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap_or_default();
            println!("cargo:rerun-if-changed={}", path.display());
            let Some(namespace) = namespace_of(&source) else {
                println!("cargo:warning={stem}.rs declares no NAMESPACE and is skipped");
                continue;
            };
            if source.contains("pub fn register_catalogue") {
                catalogues.push(stem.to_owned());
            }
            let abs = path.display().to_string().replace('\\', "/");
            entries.push((stem.to_owned(), namespace, abs));
        }
    }
    entries.sort();
    let mut code = String::new();
    for (stem, _, abs) in &entries {
        let _ = writeln!(code, "#[path = {abs:?}]\npub mod {stem};");
    }
    let _ = writeln!(
        code,
        "/// The namespaces the handler files declare.\npub const NAMESPACES: &[&str] = &["
    );
    for (_, ns, _) in &entries {
        let _ = writeln!(code, "    {ns:?},");
    }
    let _ = writeln!(code, "];");
    let _ = writeln!(
        code,
        "/// Registers every handler set whose namespace `enabled` accepts.\npub fn register_all(reg: &mut HandlerRegistry, ctx: &HandlerCtx, enabled: &dyn Fn(&str) -> bool) {{"
    );
    for (stem, _, _) in &entries {
        let _ = writeln!(
            code,
            "    if enabled({stem}::NAMESPACE) {{\n        {stem}::register(reg, ctx);\n    }}"
        );
    }
    let _ = writeln!(code, "    let _ = (reg, ctx, enabled);\n}}");
    catalogues.sort();
    let _ = writeln!(
        code,
        "/// Adds the operation catalogue of every handler set that declares one.\npub fn register_catalogues(builder: &mut spot_mgmt_ops::CatalogueBuilder) {{"
    );
    for stem in &catalogues {
        let _ = writeln!(code, "    {stem}::register_catalogue(builder);");
    }
    let _ = writeln!(code, "    let _ = builder;\n}}");
    let _ = std::fs::write(Path::new(&out_dir).join("handlers_gen.rs"), code);
}
