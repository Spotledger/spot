//! The build script of the compile test (WP-MET-04.T1): runs the generator under test the
//! way an app would. `spot_typed_views::build_views` loads the fixture definitions, emits
//! their views into `OUT_DIR`, and prints `cargo:rerun-if-changed` for every fixture TOML,
//! so editing a definition rebuilds the test.

use std::path::Path;

fn main() {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/doctypes");
    let out = std::env::var_os("OUT_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| panic!("cargo did not set OUT_DIR"));
    spot_typed_views::build_views(&fixtures, &out.join("generated_views.rs"))
        .unwrap_or_else(|e| panic!("build_views({}): {e}", fixtures.display()));
}