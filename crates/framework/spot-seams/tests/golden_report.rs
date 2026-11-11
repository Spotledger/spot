//! Golden files for the admin override listing (WP-EXT-03): the text and JSON forms are part
//! of the listing's contract, so they are pinned byte for byte. Regenerate deliberately:
//! `cargo test -p spot-seams --test golden_report -- --ignored` after an intentional change,
//! then review the diff.
#![allow(clippy::unwrap_used)]

mod common;

mod golden {
    use std::sync::Arc;

    use crate::common::*;
    use serde_json::json;
    use spot_app_api::{OverrideKind, StepOverride};

    fn report() -> spot_seams::OverrideReport {
        let mut b = base();
        b.add_override("ext", ov("core:b", OverrideKind::Wrap, Some("ext:x")));
        b.add_override("ext", ov("core:a", OverrideKind::Before, Some("ext:y")));
        b.add_override(
            "erp",
            StepOverride {
                target: sn("core:c"),
                kind: OverrideKind::Disable,
                by: None,
                reason: "no longer used by this site".to_owned(),
            },
        );
        b.add_seam_provider("tax", "tax:line", 1, Arc::new(Const(json!("v1"))));
        b.add_seam_provider("tax", "tax:line", 2, Arc::new(Const(json!("v2"))));
        b.replace_seam(
            "local_tax",
            "tax:line",
            1,
            Arc::new(Const(json!("local"))),
            "local tax rules",
        );
        b.build().unwrap().override_report()
    }

    #[test]
    fn the_text_form_matches_its_golden_file() {
        let text = report().to_text();
        assert_eq!(text, include_str!("golden/override_report.txt"));
    }

    #[test]
    fn the_json_form_matches_its_golden_file() {
        let s = serde_json::to_string_pretty(&report()).unwrap();
        assert_eq!(s, include_str!("golden/override_report.json"));
    }

    /// Writes the golden files. Ignored on purpose: run it only to refresh them after an
    /// intentional, reviewed change to the listing.
    #[test]
    #[ignore = "regenerates the golden files; run explicitly after a reviewed change"]
    fn write_golden_files() {
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/golden/override_report.txt"
            ),
            report().to_text(),
        )
        .unwrap();
        std::fs::write(
            concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/golden/override_report.json"
            ),
            serde_json::to_string_pretty(&report()).unwrap(),
        )
        .unwrap();
    }
}
