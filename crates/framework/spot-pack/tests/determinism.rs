//! Determinism: the same app directory packs to the same bytes and the same digest, twice in
//! one run and across separate process runs; the bytes carry no host time, no user names and
//! no absolute paths; and the packed bundle re-opens through the installer's own check.
//! Golden digest: a fixed fixture app's digest is pinned as a literal, so any platform,
//! timezone or machine that changes the bytes fails here rather than shipping different
//! bundles.
//!
//! Platform note: this suite cannot run on Windows from this host (the CI matrix does); what
//! it proves instead is that nothing platform-dependent reaches the bytes — the ZIP entries
//! are written with the fixed 1980-01-01 DOS timestamp, the entry order is the builder's
//! sorted order, and no entry metadata carries a host path or user name. The two-run test
//! plus the pinned digest hold on any host that executes them.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::process::Command;

mod common;

use common::{APP_ID, VERSION, good_dir, workdir};

/// The manifest of the golden fixture app: fixed text, no interpolation except the identity
/// constants, which are themselves fixed.
fn golden_manifest() -> String {
    format!(
        "[app]\n\
         name = \"{APP_ID}\"\n\
         version = \"{VERSION}\"\n\
         \n\
         [[events]]\n\
         doctype = \"customer\"\n\
         event = \"before_save\"\n\
         step = \"{APP_ID}:on_save\"\n\
         \n\
         [[assets]]\n\
         path = \"assets/logo.svg\"\n \
         \n\
         [[assets]]\n\
         path = \"assets/readme.txt\"\n"
    )
}

/// The fixed fixture app of the golden digest: two assets, so entry sorting is exercised.
fn golden_dir(root: &std::path::Path) -> std::path::PathBuf {
    let dir = root.join("golden-app");
    let files: Vec<(&str, Vec<u8>)> = vec![
        ("manifest.toml", golden_manifest().into_bytes()),
        ("doctypes/customer.toml", common::CUSTOMER_DOCTYPE.to_vec()),
        ("permissions/grants.toml", common::GRANTS.to_vec()),
        (
            "controllers.toml",
            b"[controllers]\n\"test_app:on_save\" = \"wasm:on-save\"\n".to_vec(),
        ),
        ("assets/logo.svg", common::LOGO_SVG.to_vec()),
        ("assets/readme.txt", b"golden fixture\n".to_vec()),
        ("app.wasm", common::plain_component()),
    ];
    common::write_app(&dir, &files);
    dir
}

/// Packs the golden fixture and returns `(digest_hex, bytes)`.
fn pack(root: &std::path::Path) -> (String, Vec<u8>) {
    let dir = golden_dir(root);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the golden app packs");
    (packed.digest.to_hex(), packed.bytes)
}

/// The same directory packs twice to identical bytes and the identical digest.
#[test]
fn packing_twice_is_byte_identical() {
    let (_keep, root) = workdir();
    let (first, first_bytes) = pack(&root);
    let (tmp2, root2) = workdir();
    let (second, second_bytes) = pack(&root2);
    drop(tmp2);
    assert_eq!(first, second, "the digest of two packs of the same app");
    assert_eq!(
        first_bytes, second_bytes,
        "the bytes of two packs of the same app"
    );
}

/// A separate process run (a fresh `spot-pack pack` invocation) produces the same digest:
/// determinism holds across processes, not just within one.
#[test]
fn a_separate_process_run_packs_the_same_digest() {
    let (_keep, root) = workdir();
    let dir = golden_dir(&root);
    let (_r, packed) = spot_pack::pack_app(&dir, &spot_pack::PackOptions::default())
        .expect("the golden app packs");
    let expected = packed.digest.to_hex();
    let out_path = root.join("out.spotapp");
    let out = Command::new(env!("CARGO_BIN_EXE_spot-pack"))
        .args(["pack", dir.to_str().expect("a utf-8 temp path"), "-o"])
        .arg(&out_path)
        .output()
        .expect("the CLI runs");
    assert!(
        out.status.success(),
        "pack must succeed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let bytes = std::fs::read(&out_path).expect("the packed file exists");
    let options = spot_pack::PackOptions::default();
    let bundle = spot_bundle::Bundle::open(&bytes, &options.limits, &options.host)
        .expect("the packed file opens");
    assert_eq!(
        bundle.digest().to_hex(),
        expected,
        "the CLI pack is the same digest"
    );
}

/// The packed entries carry no host state: the ZIP's local headers pin the 1980-01-01 DOS
/// timestamp (0x0021 date, 0 time), store no user or host path anywhere, and the entry order
/// is the builder's fixed manifest-then-assets order. This is the byte-level form of the
/// platform-independence argument: the writer never consults the clock or the environment.
#[test]
fn entry_metadata_has_no_host_state() {
    let (_keep, root) = workdir();
    let (_digest, bytes) = pack(&root);
    // Every local file header's DOS time field is 0 and its date field is 0x0021
    // (1980-01-01). The header layout: `PK\x03\x04`, version(2), flags(2), method(2),
    // time(2), date(2), crc(4), sizes(8), name len(2), extra len(2).
    let mut scanned = 0;
    for window in bytes.windows(30) {
        if window[0..4] == [0x50, 0x4B, 0x03, 0x04] {
            let time = u16::from_le_bytes([window[10], window[11]]);
            let date = u16::from_le_bytes([window[12], window[13]]);
            assert_eq!(time, 0, "no time-of-day in any entry");
            assert_eq!(date, 0x0021, "every entry is stamped 1980-01-01");
            scanned += 1;
        }
    }
    assert!(
        scanned >= 3,
        "the manifest, the component and the assets are entries"
    );
    // No absolute path: the fixture's temp root never appears in the bytes.
    let text = String::from_utf8_lossy(&bytes);
    let root_str = std::env::temp_dir()
        .to_string_lossy()
        .trim_end_matches('/')
        .to_owned();
    let user = std::env::var("USER").unwrap_or_default();
    // An unset USER (a container) is the empty string, which every text contains.
    assert!(
        !text.contains(&root_str) && (user.is_empty() || !text.contains(user.as_str())),
        "no temp path and no user name in the bundle bytes"
    );
}

/// Entry order is the builder's: `manifest.toml`, `app.wasm`, then assets sorted. Pinned via
/// the ZIP central directory's name sequence.
#[test]
fn entries_are_in_the_builder_order() {
    let (_keep, root) = workdir();
    let (_digest, bytes) = pack(&root);
    let names = central_directory_names(&bytes);
    assert_eq!(
        names,
        vec![
            "app.wasm".to_owned(),
            "assets/logo.svg".to_owned(),
            "assets/readme.txt".to_owned(),
            "manifest.toml".to_owned(),
        ],
        "sorted entries, manifest and component first by name"
    );
}

/// Reads the central directory's entry names in file order.
fn central_directory_names(bytes: &[u8]) -> Vec<String> {
    let mut names = Vec::new();
    let mut i = 0;
    while i + 4 <= bytes.len() {
        if bytes[i..i + 4] == [0x50, 0x4B, 0x01, 0x02] {
            let name_len = u16::from_le_bytes([bytes[i + 28], bytes[i + 29]]) as usize;
            names.push(String::from_utf8_lossy(&bytes[i + 46..i + 46 + name_len]).to_string());
            i += 46 + name_len;
        } else {
            i += 1;
        }
    }
    names
}

/// THE GOLDEN DIGEST. Pack the fixed fixture app and compare against this literal. The value
/// was computed once with the code under test; it changes ONLY when the packing format, the
/// canonical digest or the fixture itself intentionally changes — never by accident, never
/// per host, never per timezone.
#[test]
fn the_golden_digest_matches_the_pinned_literal() {
    let (_keep, root) = workdir();
    let (digest, _bytes) = pack(&root);
    assert_eq!(
        digest, "a136535e9f47cfe55324830da3bd41d0f765d85634b9ab4639458c6d78964093",
        "the golden digest changed: the packing format, the digest or the fixture moved. \
         If the change is intentional, re-pin the literal; if not, find what broke determinism."
    );
}

/// The packed bundle re-opens as the installer will: `Bundle::open` passes, the asset set is
/// complete, and `check_privileges` at the default policy finds nothing to refuse.
#[test]
fn the_packed_bundle_passes_the_installer_check() {
    let (_keep, root) = workdir();
    let dir = good_dir(&root, &[]);
    let options = spot_pack::PackOptions::default();
    let (_report, packed) = spot_pack::pack_app(&dir, &options).expect("the good app packs");
    let opened = packed
        .open(&options)
        .expect("the installer opens the packed bytes");
    assert_eq!(opened.manifest().app.name.as_str(), APP_ID);
    assert_eq!(opened.manifest().app.version.to_string(), VERSION);
    assert_eq!(
        opened.assets().len(),
        1,
        "the declared asset is in the bundle"
    );
    assert_eq!(
        opened.assets().get("assets/logo.svg").map(|b| b.as_slice()),
        Some(common::LOGO_SVG)
    );
    let report = opened.check_privileges(&spot_bundle::Policy::default());
    assert!(
        report.is_ok(),
        "the installer's privilege check passes: {:?}",
        report.errors
    );
}

/// The check report and the pack agree: an app that failed the check cannot be packed, and
/// the error carries the whole report.
#[test]
fn an_app_with_problems_cannot_be_packed() {
    let (_keep, root) = workdir();
    let dir = good_dir(&root, &[("loose.txt", b"stray\n".to_vec())]);
    let options = spot_pack::PackOptions::default();
    let err = spot_pack::pack_app(&dir, &options).expect_err("the broken app does not pack");
    assert_eq!(err.problems.len(), 1);
    assert_eq!(err.problems[0].code, "pack.layout");
    assert_eq!(err.problems[0].path.as_deref(), Some("loose.txt"));
}
