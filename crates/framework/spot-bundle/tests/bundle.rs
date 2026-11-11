//! Opening and validating bundles: the shape rejections, path safety, the asset set,
//! the limits, and the canonical digest.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::io::Cursor;
use std::io::Write as _;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use common::{
    GOOD_MANIFEST, HOST, MINIMAL_MANIFEST, codes_of, core_module, flat, full_good_bundle, got,
    opened, plain_component, raw_zip,
};
use spot_bundle::{Bundle, BundleBuilder, Digest, Limits, Policy};

// --- the happy path -----------------------------------------------------------------------------

#[test]
fn good_bundle_opens_and_validates() {
    let bytes = full_good_bundle();
    let bundle = opened(&bytes, &Limits::new());
    assert_eq!(bundle.manifest().app.name.as_str(), "test_app");
    assert_eq!(bundle.manifest().assets.len(), 1);
    assert_eq!(
        bundle.assets().get("assets/logo.svg").map(|b| b.as_slice()),
        Some("<svg/>".as_bytes())
    );
    assert_eq!(bundle.component_imports(), &["log".to_owned()]);
    // No capability is declared, so nothing is unused: `log` carries no authority.
    let report = bundle.check_privileges(&Policy::new());
    assert!(
        report.is_ok(),
        "unexpected errors: {:?}",
        flat(&report.errors)
    );
    assert!(report.unused_capabilities.is_empty());
}

#[test]
fn digest_is_a_stable_golden() {
    let bytes = full_good_bundle();
    let bundle = opened(&bytes, &Limits::new());
    // The digest of exactly this content, frozen once; any change to the canonical
    // construction or the entries must update it deliberately.
    const GOLDEN: &str = "3aed5ed74179ad28ee5b8fdb00d77eb62b16b2b994465bf7e7490006e52c4cef";
    assert_eq!(bundle.digest().to_hex(), GOLDEN);
    // Reopening the same bytes does not move the digest; the hex round-trips.
    let again = opened(&bytes, &Limits::new()).digest();
    assert_eq!(again.to_hex(), GOLDEN);
    assert_eq!(GOLDEN.parse::<Digest>(), Ok(bundle.digest()));
}

#[test]
fn digest_ignores_packing_but_not_content() {
    let manifest = MINIMAL_MANIFEST;
    let comp = plain_component();
    // Deterministic pack.
    let a = BundleBuilder::new(manifest.to_owned(), comp.clone())
        .build()
        .unwrap_or_else(|e| panic!("must build: {e:?}"));
    // Same content, packed manually in the REVERSE order, Stored, with a different
    // compression setup than the builder uses.
    let reversed = {
        let buffer = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buffer);
        for (name, bytes) in [
            ("app.wasm", comp.as_slice()),
            ("manifest.toml", manifest.as_bytes()),
        ] {
            zip.start_file(
                name,
                SimpleFileOptions::default().compression_method(CompressionMethod::Stored),
            )
            .unwrap();
            zip.write_all(bytes).unwrap();
        }
        zip.finish().unwrap().into_inner()
    };
    let a_bundle = opened(&a, &Limits::new());
    let b_bundle = opened(&reversed, &Limits::new());
    assert_eq!(
        a_bundle.digest(),
        b_bundle.digest(),
        "packing order and compression must not move the digest"
    );
    // One changed byte in one entry: a different digest.
    let mut different_manifest = manifest.to_owned();
    different_manifest.push('\n');
    let c = BundleBuilder::new(different_manifest, comp.clone())
        .build()
        .unwrap_or_else(|e| panic!("must build: {e:?}"));
    let c_bundle = opened(&c, &Limits::new());
    assert_ne!(
        a_bundle.digest(),
        c_bundle.digest(),
        "a changed manifest byte must change the digest"
    );
}

// --- the shape rejections -----------------------------------------------------------------------

#[test]
fn missing_manifest_is_reported() {
    let bytes = {
        let buffer = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buffer);
        zip.start_file("app.wasm", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(plain_component().as_slice()).unwrap();
        zip.finish().unwrap().into_inner()
    };
    let codes = codes_of(&bytes, &Limits::new());
    assert!(codes.contains(&"bundle.missing_entry"), "got {codes:?}");
}

#[test]
fn missing_wasm_is_reported() {
    let bytes = {
        let buffer = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buffer);
        zip.start_file("manifest.toml", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(GOOD_MANIFEST.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    };
    let codes = codes_of(&bytes, &Limits::new());
    assert!(codes.contains(&"bundle.missing_entry"), "got {codes:?}");
}

#[test]
fn core_module_is_not_a_component() {
    let bytes = BundleBuilder::new(GOOD_MANIFEST.to_owned(), core_module())
        .asset("assets/logo.svg", b"<svg/>".to_vec())
        .build()
        .unwrap_or_else(|e| panic!("bundle must build: {e:?}"));
    let codes = codes_of(&bytes, &Limits::new());
    assert_eq!(codes, vec!["bundle.bad_component"]);
}

#[test]
fn not_webassembly_at_all_is_a_bad_component() {
    let bytes = BundleBuilder::new(GOOD_MANIFEST.to_owned(), b"certainly not wasm".to_vec())
        .asset("assets/logo.svg", b"<svg/>".to_vec())
        .build()
        .unwrap_or_else(|e| panic!("bundle must build: {e:?}"));
    let codes = codes_of(&bytes, &Limits::new());
    assert_eq!(codes, vec!["bundle.bad_component"]);
}

#[test]
fn stray_entry_is_unexpected() {
    let bytes = {
        let buffer = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buffer);
        for (name, data) in [
            ("manifest.toml", GOOD_MANIFEST.as_bytes()),
            ("app.wasm", plain_component().as_slice()),
            ("notes.txt", b"hi".as_slice()),
            ("assets/logo.svg", "<svg/>".as_bytes()),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    };
    let codes = codes_of(&bytes, &Limits::new());
    assert!(codes.contains(&"bundle.unexpected_entry"), "got {codes:?}");
}

#[test]
fn directory_entries_are_allowed_and_ignored() {
    let bytes = {
        let buffer = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buffer);
        // A real directory entry, before the files.
        zip.add_directory("assets", SimpleFileOptions::default())
            .unwrap();
        for (name, data) in [
            ("manifest.toml", GOOD_MANIFEST.as_bytes()),
            ("app.wasm", plain_component().as_slice()),
            ("assets/logo.svg", "<svg/>".as_bytes()),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    };
    let bundle = opened(&bytes, &Limits::new());
    assert!(bundle.assets().contains_key("assets/logo.svg"));
}

// --- path safety --------------------------------------------------------------------------------

#[test]
fn dotdot_path_is_illegal() {
    let bytes = {
        let buffer = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buffer);
        for (name, data) in [
            ("manifest.toml", GOOD_MANIFEST.as_bytes()),
            ("app.wasm", plain_component().as_slice()),
            ("assets/../secret.toml", b"x".as_slice()),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    };
    let codes = codes_of(&bytes, &Limits::new());
    assert!(codes.contains(&"bundle.illegal_path"), "got {codes:?}");
}

#[test]
fn backslash_path_is_illegal() {
    let bytes = {
        let buffer = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buffer);
        for (name, data) in [
            ("manifest.toml", GOOD_MANIFEST.as_bytes()),
            ("app.wasm", plain_component().as_slice()),
            ("assets\\logo.svg", b"x".as_slice()),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    };
    let codes = codes_of(&bytes, &Limits::new());
    assert!(codes.contains(&"bundle.illegal_path"), "got {codes:?}");
}

#[test]
fn case_colliding_names_are_duplicates() {
    let bytes = raw_zip(&[
        ("manifest.toml", MINIMAL_MANIFEST.as_bytes()),
        ("app.wasm", plain_component().as_slice()),
        ("assets/App.css", b"a".as_slice()),
        ("assets/app.css", b"b".as_slice()),
    ]);
    let codes = codes_of(&bytes, &Limits::new());
    assert!(codes.contains(&"bundle.duplicate_entry"), "got {codes:?}");
}

#[test]
fn exact_duplicate_names_are_duplicates() {
    // The zip writer refuses duplicates, so this archive is assembled by hand: local file
    // headers + data + central directory, duplicated entry, no data descriptor (sizes are
    // in the headers, CRC precomputed).
    let bytes = raw_zip(&[
        ("manifest.toml", MINIMAL_MANIFEST.as_bytes()),
        ("manifest.toml", MINIMAL_MANIFEST.as_bytes()),
        ("app.wasm", plain_component().as_slice()),
    ]);
    let codes = codes_of(&bytes, &Limits::new());
    assert!(codes.contains(&"bundle.duplicate_entry"), "got {codes:?}");
}

// --- the asset set --------------------------------------------------------------------------------

#[test]
fn undeclared_asset_is_rejected() {
    let manifest = MINIMAL_MANIFEST; // declares no assets
    let bytes = {
        let buffer = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buffer);
        for (name, data) in [
            ("manifest.toml", manifest.as_bytes()),
            ("app.wasm", plain_component().as_slice()),
            ("assets/logo.svg", "<svg/>".as_bytes()),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    };
    let codes = codes_of(&bytes, &Limits::new());
    assert!(codes.contains(&"bundle.undeclared_asset"), "got {codes:?}");
}

#[test]
fn declared_missing_asset_is_rejected() {
    let manifest = GOOD_MANIFEST; // declares assets/logo.svg
    let bytes = {
        let buffer = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buffer);
        for (name, data) in [
            ("manifest.toml", manifest.as_bytes()),
            ("app.wasm", plain_component().as_slice()),
        ] {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(data).unwrap();
        }
        zip.finish().unwrap().into_inner()
    };
    let errs = match Bundle::open(&bytes, &Limits::new(), &HOST) {
        Ok(_) => panic!("expected rejection"),
        Err(errs) => errs,
    };
    assert!(
        errs.iter()
            .any(|e| e.code == "bundle.missing_asset"
                && e.path.as_deref() == Some("assets/logo.svg")),
        "got {:?}",
        flat(&errs)
    );
}

// --- the manifest passthrough ---------------------------------------------------------------------

#[test]
fn bad_manifest_code_passes_through() {
    // `name` missing entirely: spot-manifest says manifest.missing_key.
    let bytes = BundleBuilder::new("[app]\nversion = \"1.0.0\"\n".to_owned(), plain_component())
        .build()
        .unwrap_or_else(|e| panic!("bundle must build: {e:?}"));
    let errs = match Bundle::open(&bytes, &Limits::new(), &HOST) {
        Ok(_) => panic!("expected rejection"),
        Err(errs) => errs,
    };
    let [one] = errs.as_slice() else {
        panic!("expected exactly one error, got {:?}", flat(&errs));
    };
    assert_eq!(one.code, "bundle.bad_manifest");
    assert_eq!(one.path.as_deref(), Some("manifest.toml"));
    assert!(
        one.message.contains("manifest.missing_key"),
        "the manifest's own code must pass through, got {one}"
    );
}

// --- the host version ------------------------------------------------------------------------------

#[test]
fn requires_host_not_satisfied_is_rejected() {
    let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
requires_host = ">=9.0"
"#;
    let bytes = got(BundleBuilder::new(manifest.to_owned(), plain_component()).build());
    let codes = codes_of(&bytes, &Limits::new());
    assert_eq!(codes, vec!["bundle.host_required"]);
}

#[test]
fn requires_host_satisfied_opens() {
    let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"
requires_host = ">=0.1"
"#;
    let bytes = got(BundleBuilder::new(manifest.to_owned(), plain_component()).build());
    got(Bundle::open(&bytes, &Limits::new(), &HOST));
}

// --- limits ----------------------------------------------------------------------------------------

#[test]
fn zip_bomb_is_refused_without_allocating() {
    // A small archive: one entry that inflates to far beyond the 256 MiB total.
    let bomb = {
        let buffer = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buffer);
        zip.start_file("manifest.toml", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(GOOD_MANIFEST.as_bytes()).unwrap();
        // DEFLATE of ~300 MiB of zeros compresses to a few hundred KiB.
        zip.start_file("app.wasm", SimpleFileOptions::default())
            .unwrap();
        let mut written = 0_u64;
        let block = [0_u8; 65_536];
        while written < 300 * 1024 * 1024 {
            zip.write_all(&block).unwrap();
            written += block.len() as u64;
        }
        zip.finish().unwrap().into_inner()
    };
    let small_limits = Limits {
        max_compressed_bytes: 64 * 1024 * 1024,
        max_total_bytes: 8 * 1024 * 1024,
        max_entries: 4096,
        max_file_bytes: 128 * 1024 * 1024,
    };
    let codes = codes_of(&bomb, &small_limits);
    assert!(codes.contains(&"bundle.too_large"), "got {codes:?}");
}

#[test]
fn too_many_entries_is_reported() {
    let manifest = MINIMAL_MANIFEST;
    let comp = plain_component();
    let tiny = Limits {
        max_compressed_bytes: 64 * 1024 * 1024,
        max_total_bytes: 256 * 1024 * 1024,
        max_entries: 1,
        max_file_bytes: 128 * 1024 * 1024,
    };
    let bytes = got(BundleBuilder::new(manifest.to_owned(), comp.clone()).build());
    let codes = codes_of(&bytes, &tiny);
    assert!(codes.contains(&"bundle.too_many_entries"), "got {codes:?}");
}

#[test]
fn not_an_archive_at_all() {
    let codes = codes_of(
        b"this is no zip file at all, not even a little",
        &Limits::new(),
    );
    assert_eq!(codes, vec!["bundle.bad_archive"]);
}
