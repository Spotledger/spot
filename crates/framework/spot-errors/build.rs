//! Lists the catalogue files at build time, so adding `catalogue/<prefix>.toml` is the whole
//! change for a new code prefix: no list in `src/lib.rs` to edit, and no shared file for two tasks
//! to fight over.
//!
//! Writes `$OUT_DIR/catalogue_files.rs`: a slice of `(file name, include_str!(path))` in file name
//! order, which `src/lib.rs` includes as `CATALOGUE_FILES`.

use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let directory = manifest.join("catalogue");
    println!("cargo:rerun-if-changed={}", directory.display());
    let mut names = Vec::new();
    for entry in fs::read_dir(&directory)? {
        if let Ok(name) = entry?.file_name().into_string()
            && name.ends_with(".toml")
        {
            names.push(name);
        }
    }
    names.sort();
    let mut out = String::from("&[\n");
    for name in &names {
        let path = directory.join(name);
        out.push_str(&format!(
            "    ({name:?}, include_str!({:?})),\n",
            path.display().to_string()
        ));
    }
    out.push_str("]\n");
    fs::write(
        PathBuf::from(env::var("OUT_DIR")?).join("catalogue_files.rs"),
        out,
    )?;
    Ok(())
}
