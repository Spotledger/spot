//! Runs every case of `tools/client-conformance/corpus/*.json` through the crate with the
//! default feature set. Files that declare `"requires": ["storage"]` are skipped here (and
//! counted); `tests/corpus_storage.rs` runs only them, with `--features storage`.
#![cfg(not(feature = "storage"))]
#![allow(missing_docs)]

use std::fs;
use std::path::PathBuf;

use serde_json::Value;
use spot_client_core::{Core, Input};

fn corpus_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../tools/client-conformance/corpus")
}

#[test]
fn every_corpus_case_passes() -> Result<(), String> {
    let mut files: Vec<PathBuf> = fs::read_dir(corpus_dir())
        .map_err(|e| e.to_string())?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    assert!(files.len() >= 5, "the corpus has at least five topic files");
    let mut cases_run = 0;
    let mut skipped = 0;
    for file in files {
        let text = fs::read_to_string(&file).map_err(|e| e.to_string())?;
        let doc: Value =
            serde_json::from_str(&text).map_err(|e| format!("{}: {e}", file.display()))?;
        if doc["requires"]
            .as_array()
            .is_some_and(|r| r.iter().any(|f| f == "storage"))
        {
            skipped += 1;
            continue;
        }
        let cases = doc["cases"]
            .as_array()
            .ok_or_else(|| format!("{}: no cases", file.display()))?;
        for case in cases {
            let name = format!("{} / {}", file.display(), case["name"]);
            let mut core = Core::new();
            for (n, step) in case["steps"]
                .as_array()
                .ok_or_else(|| format!("{name}: no steps"))?
                .iter()
                .enumerate()
            {
                let input: Input = serde_json::from_value(step["in"].clone())
                    .map_err(|e| format!("{name} step {n}: input: {e}"))?;
                let got = serde_json::to_value(core.handle(input)).map_err(|e| e.to_string())?;
                if got != step["out"] {
                    return Err(format!(
                        "{name} step {n}\n expected: {}\n      got: {got}",
                        step["out"]
                    ));
                }
            }
            cases_run += 1;
        }
    }
    println!(
        "corpus: ran {cases_run} cases, skipped {skipped} file(s) that require the storage feature"
    );
    assert!(cases_run >= 20, "ran {cases_run} cases");
    Ok(())
}
