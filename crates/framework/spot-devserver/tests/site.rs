//! The scratch development site: create, seed, reset and snapshot/restore
//! (WP-DX-02.T1 deliverable 3).
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use camino::Utf8PathBuf;
use spot_devserver::site::{DevLayout, ScratchSiteControl, SiteControl};

fn fixtures(root: &camino::Utf8Path) -> camino::Utf8PathBuf {
    let dir = root.join("source-fixtures");
    std::fs::create_dir_all(dir.join("widget")).unwrap();
    std::fs::write(dir.join("widget/one.json"), "{}").unwrap();
    std::fs::write(dir.join("widget/two.json"), "{}").unwrap();
    std::fs::write(dir.join("README"), "fixtures").unwrap();
    dir
}

#[test]
fn a_site_is_created_seeded_reset_and_restored() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let source = fixtures(&root);
    let control = ScratchSiteControl::new(
        DevLayout::new(root.clone(), "acme-dev").unwrap(),
        Some(source.clone()),
    );

    let site = control.create().unwrap();
    assert!(site.dir.is_dir());

    let seeded = control.seed_fixtures(&source).unwrap();
    assert_eq!(seeded, 3);

    // Mutate the mutable data and snapshot, then come back to it.
    std::fs::write(site.data_dir.join("state.db"), "original").unwrap();
    let snapshot = control.snapshot("before-experiment").unwrap();
    assert!(snapshot.join("data/state.db").is_file());
    std::fs::write(site.data_dir.join("state.db"), "experiment").unwrap();
    std::fs::write(site.fixtures_dir.join("widget/three.json"), "{}").unwrap();

    control.restore("before-experiment").unwrap();
    assert_eq!(
        std::fs::read_to_string(site.data_dir.join("state.db")).unwrap(),
        "original"
    );
    assert!(!site.fixtures_dir.join("widget/three.json").exists());

    // Reset throws away the mutation and re-seeds from the fixture source.
    std::fs::write(site.data_dir.join("scratch"), "x").unwrap();
    control.reset().unwrap();
    assert!(!site.data_dir.join("scratch").exists());
    assert!(site.fixtures_dir.join("widget/one.json").is_file());

    let ops: Vec<String> = control.operations().into_iter().map(|op| op.op).collect();
    assert!(ops.contains(&"create".to_owned()));
    assert!(ops.contains(&"reset".to_owned()));
    assert!(ops.contains(&"restore".to_owned()));
}

#[test]
fn unsafe_snapshot_labels_and_missing_snapshots_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let control = ScratchSiteControl::new(
        DevLayout::new(root, "acme-dev").unwrap(),
        None::<Utf8PathBuf>,
    );
    control.create().unwrap();
    assert!(control.snapshot("../escape").is_err());
    assert!(control.restore("nope").is_err());
}
