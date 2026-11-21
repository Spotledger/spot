//! Lists `help/*.toml` at build time so adding a help file is the whole change: the files are
//! compiled into the binary with `include_str!` and `HelpIndex::load` needs no runtime files.
//!
//! Writes `$OUT_DIR/help_files.rs`: a slice of `(file name, include_str!(path))` in file name
//! order, which `src/help.rs` includes as `HELP_FILES`.

use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let directory = manifest.join("help");
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
        PathBuf::from(env::var("OUT_DIR")?).join("help_files.rs"),
        out,
    )?;
    Ok(())
}
