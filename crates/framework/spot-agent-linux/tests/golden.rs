//! Golden tests: each distribution family's transcript is replayed against the real modules
//! over the recording fake, and every mutating scenario is re-planned afterwards (idempotency:
//! the second plan is empty).
//!
//! The transcripts under `tests/transcripts/{fedora,rhel,ubuntu}/` are written by hand from the
//! tools' documented behaviour; they carry no copyrighted text.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use common::{argv, assert_subsequence, context, module_for, read_transcript, seed, validated};

#[test]
fn fedora_transcript_replays() {
    replay("fedora");
}

#[test]
fn rhel_transcript_replays() {
    replay("rhel");
}

#[test]
fn ubuntu_transcript_replays() {
    replay("ubuntu");
}

/// Replays one distribution's transcript.
fn replay(distro: &str) {
    let transcript = read_transcript(distro, "operations");
    assert_eq!(transcript.distro, distro);
    assert!(
        !transcript.description.is_empty(),
        "{distro}: the transcript describes itself"
    );
    assert!(
        !transcript.scenarios.is_empty(),
        "{distro}: the transcript has scenarios"
    );
    for scenario in &transcript.scenarios {
        // A fresh host per scenario, so the scenarios are independent.
        let system = seed(&transcript);
        let context = context(&system);
        let module = module_for(&context, &scenario.op);
        let args = validated(scenario);

        let preview = module
            .plan(&args)
            .unwrap_or_else(|error| panic!("{}: plan failed: {error}", scenario.name));
        let planned: Vec<Vec<String>> = preview
            .steps
            .iter()
            .map(|step| argv(&step.command))
            .collect();
        assert_eq!(
            planned, scenario.expect_argv,
            "{}: the planned commands",
            scenario.name
        );

        module
            .apply(&args)
            .unwrap_or_else(|error| panic!("{}: apply failed: {error}", scenario.name));

        let recorded: Vec<Vec<String>> = system.commands().iter().map(argv).collect();
        assert_subsequence(&recorded, &scenario.expect_argv, &scenario.name);

        if scenario.idempotent {
            let second = module
                .plan(&args)
                .unwrap_or_else(|error| panic!("{}: second plan failed: {error}", scenario.name));
            assert!(
                second.is_empty(),
                "{}: the second plan must be empty, got {:?}",
                scenario.name,
                second.steps
            );
        }
    }
}
