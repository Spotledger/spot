//! Generates the Rust client at build time from the converted contract.
//!
//! `tools/contract-clients/check.sh` runs `tools/contract-clients/convert.py
//! to-30` to translate the OpenAPI 3.1 contract into an OpenAPI 3.0 JSON
//! document at `spec/openapi-3.0.json` inside this crate's *copy* (the crate
//! is copied into a temporary directory by check.sh; nothing here is
//! committed). `progenitor` generates the client text into `OUT_DIR`, where
//! `src/lib.rs` includes it. The generated code is never committed.
//!
//! If `spec/openapi-3.0.json` is missing, build the crate through
//! `tools/contract-clients/check.sh` (it exists only while check.sh runs).

use progenitor::{GenerationSettings, Generator};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/lib.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
    let spec_path = format!("{manifest_dir}/spec/openapi-3.0.json");
    println!("cargo:rerun-if-changed={spec_path}");

    let file = std::fs::File::open(&spec_path)
        .unwrap_or_else(|err| panic!("cannot open {spec_path}: {err}; build this crate with tools/contract-clients/check.sh, which creates it from the native API contract"));
    let spec: openapiv3::OpenAPI = serde_json::from_reader(file)
        .unwrap_or_else(|err| panic!("malformed OpenAPI document at {spec_path}: {err}"));
    if !spec.openapi.starts_with("3.0.") {
        panic!(
            "expected the progenitor input to be OpenAPI 3.0; got {} — feed the contract through tools/contract-clients/convert.py to-30",
            spec.openapi
        );
    }
    let dec = spec
        .components
        .as_ref()
        .and_then(|c| c.schemas.get("Decimal"))
        .expect("the contract must define components.schemas.Decimal (ADR-12); the exactness guard cannot run without it");
    let is_string = matches!(
        dec,
        openapiv3::ReferenceOr::Item(schema)
            if matches!(
                schema.schema_kind,
                openapiv3::SchemaKind::Type(openapiv3::Type::String(_))
            )
    );
    assert!(
        is_string,
        "the converted contract must keep the Decimal schema as a string (ADR-12)"
    );

    let settings = GenerationSettings::default();
    let mut generator = Generator::new(&settings);
    let tokens = generator
        .generate_tokens(&spec)
        .unwrap_or_else(|err| panic!("progenitor failed on the native API contract: {err}"));

    let out_dir = std::env::var("OUT_DIR").expect("cargo sets OUT_DIR");
    let out_file = format!("{out_dir}/codegen.rs");
    std::fs::write(&out_file, tokens.to_string())
        .unwrap_or_else(|err| panic!("cannot write the generated client to {out_file}: {err}"));
}