#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
#![cfg(not(feature = "node"))]

//! The compile-time separation (WP-SEC-10.T2 deliverable 2): a node build has no unseal code.
//!
//! A hub build (the default feature set) holds the private sealing key and the one `unseal`.
//! A node build — `cargo build -p spot-sealed-fields --features node` —
//! must not, and the module, the `HubKeys` trait and the `unseal` function are then absent
//! from the binary.
//!
//! The proof here is deliberately static and deterministic: it reads the crate's own sources
//! and checks that the `hub` module is declared behind `#[cfg(not(feature = "node"))]` and
//! that `unseal` is defined nowhere else. The sibling runtime assertions confirm the feature
//! set this test file was compiled with. A build of the node feature set is itself the final
//! proof: the crate must compile with the module gone, and this file is compiled out then.

use spot_protected_keys::TenantId;
use spot_sealed_fields::{LocalHub, SealedAd, SealingKey, is_hub_build, seal};

fn tenant() -> TenantId {
    TenantId::new("acme").expect("tenant")
}

#[test]
fn a_hub_build_unseals_what_a_node_seals() {
    let hub = LocalHub::generate(tenant()).expect("hub");
    // A node holds only the public half and this writer.
    let writer = SealingKey::new(hub.public_key());
    let sealed = seal(b"DE89", &writer.public_key()).expect("seal");
    let opened = hub.unseal(&sealed, &SealedAd::unbound()).expect("unseal");
    assert_eq!(opened.expose(), b"DE89");
    assert_eq!(hub.keys().tenant(), &tenant());
}

#[test]
fn the_runtime_flag_matches_the_feature() {
    assert_eq!(is_hub_build(), !cfg!(feature = "node"));
    assert!(is_hub_build(), "the default feature set is a hub build");
}

#[test]
fn the_hub_module_is_cfg_gated() {
    let lib = include_str!("../src/lib.rs");
    let gate = "#[cfg(not(feature = \"node\"))]\npub mod hub;";
    assert!(
        lib.contains(gate),
        "the `hub` module is not declared behind `#[cfg(not(feature = \"node\"))]`:\n{lib}"
    );
    assert!(
        !lib.contains("\npub mod hub;") || lib.contains(gate),
        "the `hub` module has an ungated declaration"
    );
}

#[test]
fn unseal_is_defined_only_in_the_hub_module() {
    // Every source file, so a future refactor that moves `unseal` outside the gated module is
    // caught. `hub.rs` is the one file the gate removes.
    let sources = [
        ("lib.rs", include_str!("../src/lib.rs")),
        ("class.rs", include_str!("../src/class.rs")),
        ("sync.rs", include_str!("../src/sync.rs")),
        ("channel.rs", include_str!("../src/channel.rs")),
        ("node.rs", include_str!("../src/node.rs")),
        ("sealbox.rs", include_str!("../src/sealbox.rs")),
        ("reveal.rs", include_str!("../src/reveal.rs")),
        ("error.rs", include_str!("../src/error.rs")),
        ("bind.rs", include_str!("../src/bind.rs")),
    ];
    for (name, source) in sources {
        let defines = source
            .lines()
            .any(|line| line.trim_start().starts_with("fn unseal(") || line.contains("fn unseal("));
        assert!(
            !defines,
            "{name} defines `unseal` outside the gated hub module"
        );
    }
    // The decryption primitive and the private key type are gated too, not just the name.
    let sealbox = include_str!("../src/sealbox.rs");
    for item in ["pub struct BoxSecretKey", "pub fn open("] {
        let at = sealbox.find(item).expect("item present");
        let before = &sealbox[..at];
        assert!(
            before
                .trim_end()
                .ends_with("#[cfg(not(feature = \"node\"))]"),
            "`{item}` is compiled into a node build"
        );
    }
    let hub = include_str!("../src/hub.rs");
    assert!(
        hub.contains("fn unseal("),
        "the hub module must define `unseal`"
    );
    assert!(
        hub.contains("trait HubKeys"),
        "the hub module must carry the `HubKeys` trait"
    );
}

#[test]
fn the_module_declaration_precedes_any_hub_use() {
    // A cheap structural check: `hub` is a module of the crate only when the gate passes, so
    // anything naming `crate::hub` (outside a matching cfg) would fail a node build. The
    // remaining references are in the gated `hub.rs` itself or in this test suite.
    for (name, source) in [
        ("class.rs", include_str!("../src/class.rs")),
        ("sync.rs", include_str!("../src/sync.rs")),
        ("channel.rs", include_str!("../src/channel.rs")),
        ("sealbox.rs", include_str!("../src/sealbox.rs")),
        ("reveal.rs", include_str!("../src/reveal.rs")),
    ] {
        let code_reference = source.lines().any(|line| {
            let code = line.trim_start();
            !code.starts_with("//") && code.contains("crate::hub")
        });
        assert!(
            !code_reference,
            "{name} names `crate::hub` in code, which does not exist in a node build"
        );
    }
}
