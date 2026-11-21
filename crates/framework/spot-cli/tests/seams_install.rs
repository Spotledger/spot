//! WP-EXT-03.T3: the seam conflict check an install runs before it writes anything. A planned
//! app set that builds passes, and re-planning the installed set is not a conflict. The refusal
//! text itself comes from the registry's conflicts (`spot-seams` tests); a conflicting fixture
//! needs two native controllers that provide one seam, which no shipped app does. No database.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]
mod common;

use spot_storage_contract::RecordId;
use std::path::PathBuf;

struct Root(PathBuf);
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn root() -> Root {
    let root =
        Root(std::env::temp_dir().join(format!("spot_cli_seams_inst_{}", RecordId::new_v7())));
    common::copy_dir(&common::repo().join("apps/sample"), &root.0.join("sample"));
    std::fs::create_dir_all(root.0.join("sites/demo")).unwrap();
    std::fs::write(
        root.0.join("common.toml"),
        "apps = ['sample']\n[server]\nbind = ['127.0.0.1:0']\n[postgres]\nadmin_url = 'postgres://spot@localhost/postgres'\n",
    )
    .unwrap();
    std::fs::write(
        root.0.join("sites/demo/site.toml"),
        "site = 'demo'\ndatabase = 'demo'\napps = ['platform']\n",
    )
    .unwrap();
    root
}

fn names(apps: &[&str]) -> Vec<String> {
    apps.iter().map(|a| (*a).to_owned()).collect()
}

#[test]
fn a_planned_set_without_conflicts_passes_and_nothing_is_written() {
    let root = root();
    let loaded = spot_cli::config::load(&root.0, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == "demo")
        .unwrap()
        .clone();
    let before = std::fs::read_to_string(root.0.join("sites/demo/site.toml")).unwrap();
    spot_cli::apps::check_seam_conflicts(&root.0, &loaded, &site, &names(&["platform", "sample"]))
        .expect("the sample app conflicts with nothing");
    let planned =
        spot_cli::apps::effective(&root.0, &loaded, &site, &names(&["platform", "sample"]))
            .expect("a planned set is checked, then built");
    assert!(planned.definition("task").is_some());
    assert_eq!(
        std::fs::read_to_string(root.0.join("sites/demo/site.toml")).unwrap(),
        before,
        "the check writes nothing"
    );
}

#[test]
fn the_installed_set_planned_again_is_not_a_conflict() {
    let root = root();
    let loaded = spot_cli::config::load(&root.0, false).unwrap();
    let site = loaded
        .sites
        .iter()
        .find(|s| s.site == "demo")
        .unwrap()
        .clone();
    spot_cli::apps::check_seam_conflicts(&root.0, &loaded, &site, &site.apps).unwrap();
    spot_cli::apps::effective(&root.0, &loaded, &site, &site.apps).unwrap();
}
