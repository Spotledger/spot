//! The layout's rules: the exact key strings, round trips, ordering, and every refusal.

use super::block_on;
use crate::error::StoreError;
use crate::layout::{DIGITS, Key, Layout, MAX_TENANT_BYTES, ObjectKind, TenantId};
use crate::store::JournalStore;

fn gen1() -> String {
    Layout::padded(1)
}

#[test]
fn the_five_shapes_written_out() {
    let cases = [
        (
            Layout::manifest("acme", 7).unwrap(),
            "acme/manifest/00000000000000000007.json",
        ),
        (
            Layout::segment("acme", 7, 1, 4096).unwrap(),
            "acme/segments/00000000000000000007/00000000000000000001-00000000000000004096.seg",
        ),
        (
            Layout::tail_chunk("acme", 7, 3).unwrap(),
            "acme/tail/00000000000000000007/00000000000000000003.chunk",
        ),
        (
            Layout::snapshot("acme", 7, 4096).unwrap(),
            "acme/snapshots/00000000000000000007/00000000000000004096/",
        ),
        (
            Layout::blob("acme", &"ab".repeat(32)).unwrap(),
            "acme/blobs/abababababababababababababababababababababababababababababababab",
        ),
    ];
    for (key, expected) in cases {
        assert_eq!(key.to_key(), expected);
    }
}

#[test]
fn a_snapshot_object_hangs_under_its_prefix() {
    let prefix = Layout::snapshot("acme", 2, 15).unwrap().to_key();
    let object = Layout::snapshot_object("acme", 2, 15, "base.parquet")
        .unwrap()
        .to_key();
    assert_eq!(object, format!("{prefix}base.parquet"));
}

#[test]
fn every_shape_round_trips() {
    let keys = [
        Layout::manifest("acme", 0).unwrap(),
        Layout::manifest("acme", u64::MAX).unwrap(),
        Layout::segment("acme", 3, 0, 0).unwrap(),
        Layout::segment("acme", 3, 1, u64::MAX).unwrap(),
        Layout::tail_chunk("acme", 3, 9).unwrap(),
        Layout::snapshot("acme", 3, 9).unwrap(),
        Layout::snapshot_object("acme", 3, 9, "base.json").unwrap(),
        Layout::blob("acme", &"0f".repeat(32)).unwrap(),
    ];
    for key in keys {
        let raw = key.to_key();
        let parsed = Key::parse(&raw).unwrap_or_else(|error| {
            panic!("{raw} does not parse back: {error}");
        });
        assert_eq!(parsed, key, "parse(format(key)) is the identity");
        assert_eq!(parsed.to_key(), raw, "formatting a parsed key is stable");
    }
}

#[test]
fn lexical_order_is_numeric_order() {
    let mut numbers = [1_u64, 2, 10, 11, 99, 100, 1000, u64::MAX];
    numbers.sort_unstable();
    let keys: Vec<String> = numbers
        .iter()
        .map(|n| Layout::tail_chunk("acme", *n, *n).unwrap().to_key())
        .collect();
    let mut lexical = keys.clone();
    lexical.sort();
    assert_eq!(lexical, keys, "ascending keys are ascending numbers");

    // A generation's segments order by their first sequence number, then by their last.
    let ranges = [(1_u64, 2_u64), (1, 10), (2, 3), (10, 10)];
    let segments: Vec<String> = ranges
        .iter()
        .map(|(first, last)| Layout::segment("acme", 1, *first, *last).unwrap().to_key())
        .collect();
    let mut sorted = segments.clone();
    sorted.sort();
    assert_eq!(sorted, segments);
}

#[test]
fn the_padded_width_holds_every_u64() {
    assert_eq!(Layout::padded(u64::MAX).len(), DIGITS);
    assert_eq!(Layout::padded(0), "0".repeat(DIGITS));
    assert!(Layout::padded(7) < Layout::padded(70));
}

#[test]
fn a_tenant_id_is_one_safe_component() {
    for accepted in ["acme", "acme-eu", "acme_1", "a.b", "TENANT"] {
        assert_eq!(TenantId::new(accepted).unwrap().as_str(), accepted);
    }
    for refused in [
        "", ".", "..", "a/b", "../etc", "a..b", "a\\b", "a\nb", "a\u{0}b", "C:", "C:x", ".staging",
        ".hidden",
    ] {
        assert!(
            TenantId::new(refused).is_err(),
            "the tenant id {refused:?} must be refused"
        );
    }
    let too_long = "a".repeat(MAX_TENANT_BYTES + 1);
    assert!(TenantId::new(&too_long).is_err());
    assert!(TenantId::new(&"a".repeat(MAX_TENANT_BYTES)).is_ok());
}

#[test]
fn a_traversal_tenant_cannot_build_a_key() {
    for tenant in ["..", "../..", "../../etc", "..\\windows"] {
        assert!(
            matches!(Layout::manifest(tenant, 1), Err(StoreError::KeyInvalid(_))),
            "{tenant} must not build a key"
        );
    }
}

#[test]
fn a_traversal_key_never_reaches_the_filesystem() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("journal");
    let store = crate::folder::FolderStore::new(&root).unwrap();
    let escape = format!("../escaped/manifest/{}.json", gen1());
    let outcome = block_on(store.put_new(&escape, b"x".to_vec()));
    assert!(
        matches!(outcome, Err(StoreError::KeyInvalid(_))),
        "a traversal key is refused, got {outcome:?}"
    );
    assert!(
        !dir.path().join("escaped").exists(),
        "nothing escaped the root"
    );
    let mut names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, vec!["journal".to_owned()]);
}

#[test]
fn only_the_canonical_spelling_parses() {
    // Shadow the helper with its value so the inline format arguments below name the digits.
    let gen1 = gen1();
    let canonical = format!("acme/manifest/{gen1}.json");
    assert!(Key::parse(&canonical).is_ok());
    let refused = [
        // unpadded and over-padded numbers
        "acme/manifest/1.json".to_owned(),
        format!("acme/manifest/{}1.json", "0".repeat(DIGITS)),
        format!("acme/segments/{gen1}/1-2.seg"),
        // a reversed range, which no segment may hold
        format!(
            "acme/segments/{gen1}/{}-{}.seg",
            Layout::padded(2),
            Layout::padded(1)
        ),
        // wrong or missing suffix
        format!("acme/manifest/{gen1}"),
        format!("acme/segments/{gen1}/00000000000000000001-00000000000000000002.segx"),
        // an unknown prefix, a missing tenant, an extra component
        format!("acme/sealed/{gen1}.json"),
        format!("manifest/{gen1}.json"),
        format!("acme/manifest/{gen1}/{gen1}.json"),
        // not a number at all
        format!("acme/manifest/{}.json", "a".repeat(DIGITS)),
        // absolute, empty and traversal
        format!("/acme/manifest/{gen1}.json"),
        String::new(),
        format!("../acme/manifest/{gen1}.json"),
        format!("acme/../manifest/{gen1}.json"),
        // a snapshot without its position
        format!("acme/snapshots/{gen1}/"),
        // a snapshot with an empty component before its name
        format!("acme/snapshots/{gen1}/{gen1}//base.json"),
    ];
    for raw in refused {
        assert!(
            matches!(Key::parse(&raw), Err(StoreError::KeyInvalid(_))),
            "the key {raw:?} must be refused"
        );
    }
}

#[test]
fn a_blob_hash_is_64_lower_case_hex_digits() {
    assert!(Layout::blob("acme", &"af".repeat(32)).is_ok());
    for bad in [
        "",
        "ab",
        &"AF".repeat(32),
        &"zz".repeat(32),
        &"a".repeat(63),
        &"a".repeat(65),
    ] {
        assert!(
            matches!(Layout::blob("acme", bad), Err(StoreError::KeyInvalid(_))),
            "the hash {bad:?} must be refused"
        );
    }
}

#[test]
fn a_snapshot_name_is_one_component() {
    assert!(Layout::snapshot_object("acme", 1, 1, "base.json").is_ok());
    for bad in ["", ".", "..", "a/b", "a\\b", "a\u{1}b"] {
        assert!(
            matches!(
                Layout::snapshot_object("acme", 1, 1, bad),
                Err(StoreError::KeyInvalid(_))
            ),
            "the snapshot name {bad:?} must be refused"
        );
    }
}

#[test]
fn what_may_be_deleted_and_replaced() {
    let deletable = |key: Key| key.kind().is_deletable();
    assert!(!deletable(Layout::segment("acme", 1, 1, 1).unwrap()));
    assert!(!deletable(Layout::snapshot("acme", 1, 1).unwrap()));
    assert!(deletable(Layout::tail_chunk("acme", 1, 1).unwrap()));
    assert!(deletable(Layout::manifest("acme", 1).unwrap()));
    assert!(deletable(Layout::blob("acme", &"00".repeat(32)).unwrap()));

    let replaceable = |key: Key| key.kind().is_replaceable();
    assert!(replaceable(Layout::manifest("acme", 1).unwrap()));
    assert!(!replaceable(Layout::segment("acme", 1, 1, 1).unwrap()));
    assert!(!replaceable(Layout::tail_chunk("acme", 1, 1).unwrap()));
    assert!(!replaceable(Layout::snapshot("acme", 1, 1).unwrap()));
    assert!(!replaceable(
        Layout::blob("acme", &"00".repeat(32)).unwrap()
    ));
    assert_eq!(
        Layout::manifest("acme", 1).unwrap().kind(),
        ObjectKind::Manifest
    );
}

#[test]
fn a_key_reports_its_tenant() {
    let key = Layout::segment("acme-eu", 2, 3, 4).unwrap();
    assert_eq!(key.tenant().as_str(), "acme-eu");
    assert_eq!(key.to_string(), key.to_key());
}

#[test]
fn the_folder_store_creates_its_root() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("a").join("b");
    let store = crate::folder::FolderStore::new(&root).unwrap();
    assert_eq!(store.root(), root.as_path());
    assert!(root.join(crate::folder::STAGING_DIR).is_dir());
}
