//! The latency test: a metadata change reaches 'live' within a generous budget measured on the
//! CI runner, and the number is printed (WP-DX-02.T1 deliverable 6).
//!
//! The budget is deliberately generous (five seconds): this test is here to catch a pipeline
//! that does not swap at all or that blocks, not to benchmark a loaded machine.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use camino::Utf8PathBuf;
use spot_core::{Clock, SiteId, SystemClock};
use spot_devserver::classify::{Change, ChangeKind};
use spot_devserver::reload::{MetaStep, Reloader, Step};
use spot_devserver::watch::Op;

#[test]
fn a_metadata_change_is_live_within_the_budget() {
    let temp = tempfile::tempdir().unwrap();
    let root = Utf8PathBuf::from_path_buf(temp.path().to_path_buf()).unwrap();
    let meta_dir = root.join("meta");
    std::fs::create_dir_all(&meta_dir).unwrap();
    let file = meta_dir.join("widget.toml");
    std::fs::write(
        &file,
        "name = \"widget\"\nlabel = \"Widget\"\nnaming = \"WID-.#####\"\n\n[[fields]]\nname = \"title\"\nlabel = \"Title\"\ntype = \"text\"\n",
    )
    .unwrap();

    let meta = MetaStep::open(SiteId::new("acme.example").unwrap(), meta_dir).unwrap();
    let live = meta.live().clone();
    let reloader = Reloader::new(4).with_step(Arc::new(meta) as Arc<dyn Step>);
    let clock = SystemClock::new();

    std::fs::write(
        &file,
        "name = \"widget\"\nlabel = \"Widget One\"\nnaming = \"WID-.#####\"\n\n[[fields]]\nname = \"title\"\nlabel = \"Title\"\ntype = \"text\"\n",
    )
    .unwrap();
    let change = Change {
        path: file.clone(),
        relative: Utf8PathBuf::from("meta/widget.toml"),
        op: Op::Modified,
        kind: ChangeKind::Metadata,
    };

    let started = clock.monotonic();
    let summary = reloader.reload(&[change], &clock);
    let elapsed = clock.monotonic().saturating_sub(started);

    assert!(summary.all_ok(), "{summary:?}");
    let version = live.current().version().get();
    assert_eq!(version, 2, "the change is live");
    assert!(
        live.current().meta().definition("widget").is_some(),
        "the new metadata serves"
    );

    let budget = Duration::from_secs(5);
    println!(
        "WP-DX-02.T1 metadata reload latency: {} ms (record {} ms, budget {} ms)",
        elapsed.as_millis(),
        summary.records[0].duration_ms,
        budget.as_millis()
    );
    assert!(
        elapsed < budget,
        "metadata took {} ms, over the {} ms budget",
        elapsed.as_millis(),
        budget.as_millis()
    );
}
