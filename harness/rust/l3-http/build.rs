//! Registers every `src/cases/<name>.rs` that declares `ORDER` and `cases()` (see
//! `src/cases/mod.rs`).

#[path = "build/scan.rs"]
mod scan;

use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src/cases");
    println!("cargo:rerun-if-changed=build/scan.rs");
    if let Err(message) = run() {
        eprintln!("error: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?;
    let out = std::env::var("OUT_DIR").map_err(|e| e.to_string())?;
    let dir = PathBuf::from(&manifest).join("src").join("cases");
    let mut files = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let path = entry.path();
        if !entry.file_type().map_err(|e| e.to_string())?.is_file()
            || path.extension().and_then(|e| e.to_str()) != Some("rs")
        {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| format!("{}: a file name that is not UTF-8", dir.display()))?;
        let text = fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        files.push((name, text));
    }
    let modules = scan::registered(&files)?;
    let dir_text = dir.to_string_lossy().replace('\\', "/");
    let text = scan::registry_source(&dir_text, &modules);
    fs::write(PathBuf::from(out).join("cases_registry.rs"), text).map_err(|e| e.to_string())
}
