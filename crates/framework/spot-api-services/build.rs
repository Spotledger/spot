//! Declares every `src/resources/<name>.rs` as a module (see `src/resources/mod.rs`).

#[path = "build/scan.rs"]
mod scan;

use std::fs;
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=src/resources");
    println!("cargo:rerun-if-changed=build/scan.rs");
    if let Err(message) = run() {
        println!("cargo:warning={message}");
        eprintln!("error: {message}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").map_err(|e| e.to_string())?;
    let out = std::env::var("OUT_DIR").map_err(|e| e.to_string())?;
    let dir = PathBuf::from(&manifest).join("src").join("resources");
    let mut names = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        if entry.file_type().map_err(|e| e.to_string())?.is_file() {
            let name = entry.file_name();
            let name = name
                .to_str()
                .ok_or_else(|| format!("{}: a file name that is not UTF-8", dir.display()))?;
            names.push(name.to_owned());
        }
    }
    let modules = scan::module_names(&names)?;
    let dir_text = dir.to_string_lossy().replace('\\', "/");
    let text = scan::registry_source(&dir_text, &modules);
    fs::write(PathBuf::from(out).join("resources_registry.rs"), text).map_err(|e| e.to_string())
}
