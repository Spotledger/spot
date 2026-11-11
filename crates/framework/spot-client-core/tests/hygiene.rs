//! The crate stays sans-IO: no forbidden dependency, no wall clock, a token that cannot be serialised.
#![allow(missing_docs)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde::Serialize;
use spot_client_core::SessionToken;

// If `T: Serialize` held for `SessionToken`, both blanket impls would apply to it and the
// call below would not compile (ambiguity). So this file compiling proves it is not `Serialize`.
trait AmbiguousIfSerialize<A> {
    fn check() {}
}
impl<T: ?Sized> AmbiguousIfSerialize<()> for T {}
impl<T: ?Sized + Serialize> AmbiguousIfSerialize<u8> for T {}

#[test]
fn session_token_is_not_serialize() {
    <SessionToken as AmbiguousIfSerialize<_>>::check();
}

#[test]
fn no_forbidden_dependency() -> Result<(), String> {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let output = Command::new(cargo)
        .args([
            "tree",
            "-p",
            "spot-client-core",
            "--offline",
            "--prefix",
            "none",
            "-e",
            "normal,build",
        ])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .map_err(|e| e.to_string())?;
    assert!(
        output.status.success(),
        "cargo tree failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    for line in tree.lines() {
        let name = line.split_whitespace().next().unwrap_or("");
        for forbidden in [
            "tokio",
            "reqwest",
            "web-sys",
            "js-sys",
            "wasm-bindgen",
            "hyper",
            "getrandom",
            "rand",
        ] {
            assert_ne!(
                name, forbidden,
                "spot-client-core must not depend on {forbidden}"
            );
        }
    }
    Ok(())
}

#[test]
fn source_reads_no_clock_or_random_source() -> Result<(), String> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in fs::read_dir(src).map_err(|e| e.to_string())? {
        let path = entry.map_err(|e| e.to_string())?.path();
        let text = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        for forbidden in [
            "SystemTime",
            "Instant::now",
            "std::time",
            "rand::",
            "getrandom",
            "unsafe ",
            "web_sys",
        ] {
            assert!(
                !text.contains(forbidden),
                "{} mentions {forbidden}",
                path.display()
            );
        }
    }
    Ok(())
}
