//! The privilege check: unknown imports, imports without their capability, unused
//! capabilities as warnings, and the policy caps.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod common;

use proptest::prelude::*;
use std::io::Cursor;
use std::io::Write as _;

use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use common::{
    GOOD_MANIFEST, HOST, MINIMAL_MANIFEST, READER_MANIFEST, caps_bundle, component, flat,
    full_good_bundle, got, opened, plain_component, reading_component, stranger_component,
};
use spot_bundle::{Bundle, BundleBuilder, Limits, Policy};

// --- privileges ------------------------------------------------------------------------------------

#[test]
fn import_without_capability_is_rejected() {
    let bytes = got(BundleBuilder::new(MINIMAL_MANIFEST.to_owned(), reading_component()).build());
    let bundle = opened(&bytes, &Limits::new());
    let report = bundle.check_privileges(&Policy::new());
    assert!(!report.is_ok());
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.code == "bundle.import_without_capability"),
        "got {:?}",
        flat(&report.errors)
    );
}

#[test]
fn import_with_capability_passes() {
    let bytes = got(BundleBuilder::new(READER_MANIFEST.to_owned(), reading_component()).build());
    let bundle = opened(&bytes, &Limits::new());
    let report = bundle.check_privileges(&Policy::new());
    assert!(
        report.is_ok(),
        "the reading bundle must pass: {:?}",
        flat(&report.errors)
    );
}

#[test]
fn unknown_import_is_rejected() {
    let bytes = got(BundleBuilder::new(MINIMAL_MANIFEST.to_owned(), stranger_component()).build());
    let bundle = opened(&bytes, &Limits::new());
    let report = bundle.check_privileges(&Policy::new());
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.code == "bundle.unknown_import" && e.message.contains("make-coffee")),
        "got {:?}",
        flat(&report.errors)
    );
}

#[test]
fn unused_declared_capability_is_a_warning() {
    // Declares reads the component never exercises (it imports `log` only).
    let bytes = caps_bundle(READER_MANIFEST);
    let bundle = opened(&bytes, &Limits::new());
    let report = bundle.check_privileges(&Policy::new());
    assert!(
        report.is_ok(),
        "unused capabilities must not fail: {:?}",
        flat(&report.errors)
    );
    assert!(
        report
            .unused_capabilities
            .iter()
            .any(|u| u.note.contains("DocType read")),
        "got {:?}",
        report.unused_capabilities
    );
}

#[test]
fn declared_unused_importless_bundle_reports_the_rest() {
    // A bundle declaring network + email + files that imports nothing exercising them:
    // unused capabilities for each, but the bundle itself validates.
    let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"

[capabilities]
network = ["api.example.com:443"]
email = true
files = "read"
"#;
    let bytes = caps_bundle(manifest);
    let bundle = opened(&bytes, &Limits::new());
    let lenient = Policy {
        allow_email: true,
        allow_wildcard_hosts: true,
        ..Policy::new()
    };
    let report = bundle.check_privileges(&lenient);
    assert!(
        report.is_ok(),
        "these are warnings only: {:?}",
        flat(&report.errors)
    );
    let notes = report
        .unused_capabilities
        .iter()
        .map(|u| u.note.clone())
        .collect::<Vec<_>>();
    assert!(notes.iter().any(|n| n.contains("network")), "got {notes:?}");
    assert!(notes.iter().any(|n| n.contains("e-mail")), "got {notes:?}");
    assert!(
        notes.iter().any(|n| n.contains("file-store")),
        "got {notes:?}"
    );
}

#[test]
fn wildcard_host_is_over_privileged() {
    let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"

[capabilities]
network = ["*:443"]
"#;
    let bytes = caps_bundle(manifest);
    let bundle = opened(&bytes, &Limits::new());
    let report = bundle.check_privileges(&Policy::new());
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.code == "bundle.over_privileged" && e.message.contains("wildcard")),
        "got {:?}",
        flat(&report.errors)
    );
    // The same bundle is fine under a policy that allows it.
    let lenient = Policy {
        allow_wildcard_hosts: true,
        ..Policy::new()
    };
    assert!(bundle.check_privileges(&lenient).is_ok());
}

#[test]
fn email_forbidden_by_policy() {
    let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"

[capabilities]
email = true
"#;
    let bytes = caps_bundle(manifest);
    let bundle = opened(&bytes, &Limits::new());
    // Default policy forbids e-mail.
    let report = bundle.check_privileges(&Policy::new());
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.code == "bundle.over_privileged" && e.message.contains("e-mail")),
        "got {:?}",
        flat(&report.errors)
    );
    let lenient = Policy {
        allow_email: true,
        ..Policy::new()
    };
    assert!(bundle.check_privileges(&lenient).is_ok());
}

#[test]
fn write_to_forbidden_doctype() {
    let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"

[capabilities]
doctypes = { write = ["General Ledger"] }
"#;
    let bytes = caps_bundle(manifest);
    let bundle = opened(&bytes, &Limits::new());
    let policy = Policy {
        forbidden_doctypes: vec!["General Ledger".to_owned()],
        ..Policy::new()
    };
    let report = bundle.check_privileges(&policy);
    assert!(
        report.errors.iter().any(|e| {
            e.code == "bundle.over_privileged"
                && e.path
                    .as_deref()
                    .unwrap_or_default()
                    .contains("General Ledger")
        }),
        "got {:?}",
        flat(&report.errors)
    );
}

#[test]
fn too_many_endpoints() {
    let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"

[capabilities]
network = ["a.example.com:1", "b.example.com:2"]
"#;
    let bytes = caps_bundle(manifest);
    let bundle = opened(&bytes, &Limits::new());
    let policy = Policy {
        max_network_endpoints: Some(1),
        allow_wildcard_hosts: true,
        ..Policy::new()
    };
    let report = bundle.check_privileges(&policy);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.code == "bundle.over_privileged" && e.message.contains("2 endpoints")),
        "got {:?}",
        flat(&report.errors)
    );
}

#[test]
fn write_doctypes_capped() {
    let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"

[capabilities]
doctypes = { write = ["Customer", "Supplier"] }
"#;
    let bytes = caps_bundle(manifest);
    let bundle = opened(&bytes, &Limits::new());
    let policy = Policy {
        max_write_doctypes: Some(1),
        ..Policy::new()
    };
    let report = bundle.check_privileges(&policy);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.code == "bundle.over_privileged" && e.message.contains("2 DocTypes")),
        "got {:?}",
        flat(&report.errors)
    );
}

#[test]
fn file_access_capped() {
    let manifest = r#"
[app]
name = "test_app"
version = "1.0.0"

[capabilities]
files = "read_write"
"#;
    let bytes = caps_bundle(manifest);
    let bundle = opened(&bytes, &Limits::new());
    let policy = Policy {
        max_file_access: Some(spot_manifest::FileAccess::Read),
        ..Policy::new()
    };
    let report = bundle.check_privileges(&policy);
    assert!(
        report
            .errors
            .iter()
            .any(|e| e.code == "bundle.over_privileged" && e.message.contains("ReadWrite")),
        "got {:?}",
        flat(&report.errors)
    );
}

// --- the builder -----------------------------------------------------------------------------------

#[test]
fn builder_is_deterministic() {
    let a = full_good_bundle();
    let b = full_good_bundle();
    assert_eq!(a, b, "the same inputs must pack byte-identically");
}

#[test]
fn builder_refuses_assets_outside_the_folder() {
    let built = BundleBuilder::new(GOOD_MANIFEST.to_owned(), plain_component())
        .asset("chroot/logo.svg", b"x".to_vec())
        .build();
    match built {
        Ok(_) => panic!("an asset outside assets/ must be refused"),
        Err(errs) => assert!(
            errs.iter().any(|e| e.code == "bundle.unexpected_entry"),
            "{errs:?}"
        ),
    }
}

#[test]
fn builder_refuses_escaping_asset_names() {
    let built = BundleBuilder::new(GOOD_MANIFEST.to_owned(), plain_component())
        .asset("assets/../../x", b"x".to_vec())
        .build();
    match built {
        Ok(_) => panic!("an escaping name must be refused"),
        Err(errs) => assert!(
            errs.iter().any(|e| e.code == "bundle.illegal_path"),
            "{errs:?}"
        ),
    }
}

// --- property tests --------------------------------------------------------------------------------

/// Every host-legal import name the tests may draw for the random-component property.
const KNOWN_IMPORTS: [&str; 4] = ["level", "log", "now", "get-doc"];

proptest! {
    /// A component importing a random host call, in a manifest that declares the matching
    /// capability half the time (by drawing the manifest after the import name): opening
    /// never panics, and the privilege check's verdict agrees with the capability table.
    #[test]
    fn privilege_check_agrees_with_the_table(
        import in proptest::sample::select(&KNOWN_IMPORTS),
        declare in proptest::bool::ANY,
    ) {
        let manifest = if declare && import == "get-doc" {
            READER_MANIFEST
        } else if import == "get-doc" {
            // Declares nothing: the import must be rejected.
            MINIMAL_MANIFEST
        } else {
            MINIMAL_MANIFEST
        };
        let bytes = got(BundleBuilder::new(manifest.to_owned(), component(import)).build());
        let bundle = opened(&bytes, &Limits::new());
        let report = bundle.check_privileges(&Policy::new());
        let denied = import == "get-doc" && !declare;
        prop_assert!(
            report.is_ok() == !denied,
            "import {import} declared={declare}: {:?}",
            flat(&report.errors)
        );
    }

    /// Arbitrary bundles of random junk entries: open either succeeds or returns problems,
    /// but never panics.
    #[test]
    fn open_never_panics_on_random_archives(seed in proptest::prelude::any::<u64>()) {
        let buffer = Cursor::new(Vec::new());
        let mut zip = ZipWriter::new(buffer);
        for i in 0..(seed % 5) {
            let name = format!("assets/f{}.css", i);
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(&[(i % 256) as u8]).unwrap();
        }
        zip.start_file("manifest.toml", SimpleFileOptions::default()).unwrap();
        zip.write_all(GOOD_MANIFEST.as_bytes()).unwrap();
        zip.start_file("app.wasm", SimpleFileOptions::default()).unwrap();
        zip.write_all(plain_component().as_slice()).unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        // The result is either way fine; the property is that it does not panic.
        let _ = Bundle::open(&bytes, &Limits::new(), &HOST);
        prop_assert!(true);
    }
}
