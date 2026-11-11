//! Helpers shared by the module unit tests: a tiny valid component and bundles built from
//! hand-written manifests. Available only in test builds.
#![allow(dead_code, missing_docs, clippy::unwrap_used, clippy::expect_used)]

use spot_bundle::{Bundle, BundleBuilder, HostInfo, Limits};

/// The host the unit tests open bundles for: the workspace's platform version.
pub(crate) fn host() -> HostInfo {
    spot_pack::PackOptions::default().host
}

/// A minimal, valid WebAssembly component with no imports and no callable exports. Enough for
/// `Bundle::open`, the static checks and the security scan, which never call into it.
pub(crate) fn plain_component() -> Vec<u8> {
    wat::parse_str("(component)").expect("the empty component assembles")
}

/// A component that imports one function name, for the privilege checks.
pub(crate) fn component_importing(import: &str) -> Vec<u8> {
    let text = format!(
        r#"(component
  (import "{import}" (func $f (param "message" string)))
  (core module $libc
    (memory (export "memory") 1)
    (global $heap (mut i32) (i32.const 1024))
    (func (export "realloc") (param $old i32) (param $old_size i32) (param $align i32) (param $new_size i32) (result i32)
      (local $ptr i32)
      (local.set $ptr (global.get $heap))
      (global.set $heap (i32.add (local.get $ptr) (local.get $new_size)))
      (local.get $ptr)))
  (core instance $c (instantiate $libc))
  (core func $f_l (canon lower (func $f)
    (memory (core memory $c "memory"))
    (realloc (core func $c "realloc"))
    string-encoding=utf8)))"#
    );
    wat::parse_str(&text).expect("the component assembles")
}

/// Builds a bundle from a manifest and a component and opens it.
pub(crate) fn bundle(manifest: &str, wasm: &[u8]) -> Bundle {
    bundle_with_assets(manifest, wasm, &[])
}

/// Builds a bundle with assets and opens it.
pub(crate) fn bundle_with_assets(manifest: &str, wasm: &[u8], assets: &[(&str, &[u8])]) -> Bundle {
    let bytes = bundle_bytes(manifest, wasm, assets);
    Bundle::open(&bytes, &Limits::new(), &host()).expect("the bundle opens")
}

/// Builds a bundle's bytes without opening it.
pub(crate) fn bundle_bytes(manifest: &str, wasm: &[u8], assets: &[(&str, &[u8])]) -> Vec<u8> {
    let mut builder = BundleBuilder::new(manifest.to_owned(), wasm.to_vec());
    for (name, bytes) in assets {
        builder = builder.asset((*name).to_owned(), bytes.to_vec());
    }
    builder.build().expect("the bundle builds")
}
