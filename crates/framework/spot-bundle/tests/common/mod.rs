//! Fixtures shared by the bundle tests: the host, the manifests, wat-built components,
//! the unpack helpers, and the hand-assembled raw ZIP for cases the zip writer refuses.
#![allow(missing_docs, dead_code, clippy::unwrap_used, clippy::expect_used)]

use spot_bundle::{Bundle, BundleBuilder, BundleError, HostInfo, Limits};

/// Host version the tests open bundles for: the platform is at 0.1.0.
pub const HOST: HostInfo = HostInfo {
    version: semver::Version {
        major: 0,
        minor: 1,
        patch: 0,
        pre: semver::Prerelease::EMPTY,
        build: semver::BuildMetadata::EMPTY,
    },
};

/// Unwrap for tests without tripping the workspace's `unwrap_used` lint.
#[track_caller]
pub fn got<T, E: std::fmt::Debug>(result: Result<T, E>) -> T {
    match result {
        Ok(value) => value,
        Err(err) => panic!("expected Ok, got {err:?}"),
    }
}

/// Renders a rejection for assertions.
pub fn flat(errs: &[BundleError]) -> Vec<String> {
    errs.iter()
        .map(|e| format!("{} | {}", e.code, e.message))
        .collect()
}

/// Unwraps `Bundle::open`, failing with the codes when rejected.
#[track_caller]
pub fn opened(bytes: &[u8], limits: &Limits) -> Bundle {
    match Bundle::open(bytes, limits, &HOST) {
        Ok(bundle) => bundle,
        Err(errs) => panic!(
            "expected the bundle to open, got:\n{}",
            flat(&errs).join("\n")
        ),
    }
}

/// Asserts `Bundle::open` rejects with exactly these codes (order-insensitive).
#[track_caller]
pub fn codes_of(bytes: &[u8], limits: &Limits) -> Vec<&'static str> {
    match Bundle::open(bytes, limits, &HOST) {
        Ok(_) => panic!("expected rejection, the bundle opened"),
        Err(errs) => errs.iter().map(|e| e.code).collect(),
    }
}

// --- the test component -----------------------------------------------------------------------

/// A minimal component assembled from WebAssembly text: it imports what `import` names and
/// exports nothing callable (the validator never calls into the bundle; it only parses and
/// validates the binary).
pub fn component(import: &str) -> Vec<u8> {
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
    string-encoding=utf8))
)"#
    );
    wat::parse_str(&text)
        .unwrap_or_else(|err| panic!("the fixture does not assemble: {err}\n{text}"))
}

/// The component the happy-path bundles carry: it imports `log`, which carries no
/// authority, so no capability is needed for it.
pub fn plain_component() -> Vec<u8> {
    component("log")
}

/// A component that imports `get-doc`, which needs DocType read access.
pub fn reading_component() -> Vec<u8> {
    component("get-doc")
}

/// A core module, not a component: the header check must refuse it.
pub fn core_module() -> Vec<u8> {
    wat::parse_str("(module (func (export \"f\") nop))")
        .unwrap_or_else(|err| panic!("the core module does not assemble: {err}"))
}

/// A component that imports something the host does not provide.
pub fn stranger_component() -> Vec<u8> {
    component("make-coffee")
}

// --- the manifest ------------------------------------------------------------------------------

/// The manifest of the happy path: no capabilities beyond `log`, one declared asset.
pub const GOOD_MANIFEST: &str = r#"
[app]
name = "test_app"
version = "1.0.0"

[[assets]]
path = "assets/logo.svg"
"#;

/// The manifest without capabilities, declaring nothing.
pub const MINIMAL_MANIFEST: &str = r#"
[app]
name = "test_app"
version = "1.0.0"
"#;

/// A manifest for the reading component.
pub const READER_MANIFEST: &str = r#"
[app]
name = "test_app"
version = "1.0.0"

[capabilities]
doctypes = { read = ["Customer"] }
"#;

/// Writes a capability-manifest bundle with no assets, through the real builder.
pub fn caps_bundle(manifest: &str) -> Vec<u8> {
    BundleBuilder::new(manifest.to_owned(), plain_component())
        .build()
        .unwrap_or_else(|e| panic!("bundle must build: {e:?}"))
}

/// Writes a deterministic bundle through the real builder.
pub fn good_bundle(manifest: &str, component: &[u8]) -> Vec<u8> {
    BundleBuilder::new(manifest.to_owned(), component.to_vec())
        .asset("assets/logo.svg", "<svg/>".as_bytes().to_vec())
        .build()
        .unwrap_or_else(|e| panic!("bundle must build: {e:?}"))
}

/// The every-piece-in-place bundle the first tests use.
pub fn full_good_bundle() -> Vec<u8> {
    good_bundle(GOOD_MANIFEST, &plain_component())
}

/// Hand-assembles a Stored ZIP from `(name, data)` pairs, exactly as listed — duplicates,
/// odd orders, whatever the tests need that the zip writer will not write.
pub fn raw_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    fn crc32(data: &[u8]) -> u32 {
        // The IEEE CRC-32 the ZIP format pins (slice-by-8 not needed for test sizes).
        let mut table = [0_u32; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
            *entry = c;
        }
        let mut crc = 0xFFFF_FFFF_u32;
        for &b in data {
            crc = table[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
        }
        crc ^ 0xFFFF_FFFF
    }

    let mut out = Vec::new();
    let mut central = Vec::new();
    let mut offset = 0_u32;
    for (name, data) in entries {
        let name = name.as_bytes();
        let crc = crc32(data);
        let header = {
            let mut h = Vec::new();
            h.extend_from_slice(&[0x50, 0x4B, 0x03, 0x04]); // local file header
            h.extend_from_slice(&20_u16.to_le_bytes()); // version needed
            h.extend_from_slice(&0_u16.to_le_bytes()); // flags
            h.extend_from_slice(&0_u16.to_le_bytes()); // Stored
            h.extend_from_slice(&0_u16.to_le_bytes()); // time
            h.extend_from_slice(&0x0021_u16.to_le_bytes()); // 1980-01-01
            h.extend_from_slice(&crc.to_le_bytes());
            h.extend_from_slice(&(data.len() as u32).to_le_bytes()); // compressed
            h.extend_from_slice(&(data.len() as u32).to_le_bytes()); // uncompressed
            h.extend_from_slice(&(name.len() as u16).to_le_bytes());
            h.extend_from_slice(&0_u16.to_le_bytes()); // extra len
            h.extend_from_slice(name);
            h
        };
        out.extend_from_slice(&header);
        out.extend_from_slice(data);
        let rec = {
            let mut c = Vec::new();
            c.extend_from_slice(&[0x50, 0x4B, 0x01, 0x02]); // central directory header
            c.extend_from_slice(&20_u16.to_le_bytes()); // version made by
            c.extend_from_slice(&20_u16.to_le_bytes()); // version needed
            c.extend_from_slice(&0_u16.to_le_bytes()); // flags
            c.extend_from_slice(&0_u16.to_le_bytes()); // Stored
            c.extend_from_slice(&0_u16.to_le_bytes()); // time
            c.extend_from_slice(&0x0021_u16.to_le_bytes()); // 1980-01-01
            c.extend_from_slice(&crc.to_le_bytes());
            c.extend_from_slice(&(data.len() as u32).to_le_bytes());
            c.extend_from_slice(&(data.len() as u32).to_le_bytes());
            c.extend_from_slice(&(name.len() as u16).to_le_bytes());
            c.extend_from_slice(&0_u16.to_le_bytes()); // extra
            c.extend_from_slice(&0_u16.to_le_bytes()); // comment
            c.extend_from_slice(&0_u16.to_le_bytes()); // disk
            c.extend_from_slice(&0_u16.to_le_bytes()); // internal attrs
            c.extend_from_slice(&0_u32.to_le_bytes()); // external attrs
            c.extend_from_slice(&offset.to_le_bytes()); // local header offset
            c.extend_from_slice(name);
            c
        };
        central.extend_from_slice(&rec);
        offset += header.len() as u32 + data.len() as u32;
    }
    let central_offset = offset;
    out.extend_from_slice(&central);
    out.extend_from_slice(&[0x50, 0x4B, 0x05, 0x06]); // EOCD
    out.extend_from_slice(&0_u16.to_le_bytes()); // disk
    out.extend_from_slice(&0_u16.to_le_bytes()); // cd disk
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(central.len() as u32).to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0_u16.to_le_bytes()); // comment len
    out
}
