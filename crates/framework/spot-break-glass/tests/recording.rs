#![allow(clippy::unwrap_used, clippy::expect_used)]
//! The recorder: hash chain, redaction, tamper detection, the size cap and replay.

mod common;

use common::*;
use proptest::prelude::*;
use spot_break_glass::{
    BreakGlassError, FileRecorder, Frame, FrameKind, MemoryRecorder, REDACTED, Recorder, replay,
    verify_recording,
};

fn header_len(session: &str, tenant: &str) -> usize {
    9 + session.len() + 4 + tenant.len()
}

#[test]
fn a_recording_round_trips_and_verifies_against_its_manifest() {
    let dir = TempDir::new("round-trip");
    let mut recorder = FileRecorder::create(dir.path(), SESSION, TENANT, 1000, 1 << 20).unwrap();
    recorder
        .append(FrameKind::Output, 1000, b"$ whoami\n", true)
        .unwrap();
    recorder
        .append(FrameKind::Input, 1001, b"root\n", true)
        .unwrap();
    recorder
        .append(FrameKind::Resize, 1002, &[0, 120, 0, 40], true)
        .unwrap();
    let digest = recorder.finish().unwrap();

    let replayed: Vec<Frame> = replay(&recorder_path(&dir, SESSION))
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(replayed.len(), 3);
    assert_eq!(replayed[0].kind, FrameKind::Output);
    assert_eq!(replayed[1].data, b"root\n");
    assert_eq!(replayed[2].resize_size(), Some((120, 40)));
    assert_eq!(replayed.last().unwrap().hash, digest);

    let verified =
        verify_recording(&recorder_path(&dir, SESSION), &manifest_path(&dir, SESSION)).unwrap();
    assert_eq!(verified, digest);
}

#[test]
fn a_no_echo_input_frame_is_redacted_and_the_secret_never_reaches_the_file() {
    let dir = TempDir::new("redaction");
    let secret = b"hunter2-the-password";
    let mut recorder = FileRecorder::create(dir.path(), SESSION, TENANT, 1000, 1 << 20).unwrap();
    recorder
        .append(FrameKind::Input, 1000, secret, false)
        .unwrap();
    recorder.finish().unwrap();

    let bytes = std::fs::read(recorder_path(&dir, SESSION)).unwrap();
    assert!(
        !bytes.windows(secret.len()).any(|w| w == secret),
        "the secret must not be in the recording"
    );
    let replayed: Vec<Frame> = replay(&recorder_path(&dir, SESSION))
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(replayed.len(), 1);
    assert!(replayed[0].redacted);
    assert_eq!(replayed[0].data, REDACTED);
}

#[test]
fn an_echoed_input_frame_keeps_its_bytes() {
    let mut recorder = MemoryRecorder::new(SESSION, 1 << 20);
    recorder
        .append(FrameKind::Input, 1000, b"ls -la\n", true)
        .unwrap();
    assert_eq!(recorder.frames()[0].data, b"ls -la\n");
    assert!(!recorder.frames()[0].redacted);
}

#[test]
fn tampering_with_a_recorded_byte_is_detected() {
    let dir = TempDir::new("tamper");
    let mut recorder = FileRecorder::create(dir.path(), SESSION, TENANT, 1000, 1 << 20).unwrap();
    recorder
        .append(FrameKind::Output, 1000, b"original output", true)
        .unwrap();
    recorder.finish().unwrap();

    let path = recorder_path(&dir, SESSION);
    let mut bytes = std::fs::read(&path).unwrap();
    let offset = header_len(SESSION, TENANT) + 31;
    bytes[offset] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();

    let replayed = replay(&path);
    assert!(!replayed.verified());
    let items: Vec<_> = replayed.collect();
    assert!(matches!(
        items.last(),
        Some(Err(BreakGlassError::RecordingTampered { frame: 1 }))
    ));

    let error = verify_recording(&path, &manifest_path(&dir, SESSION)).unwrap_err();
    assert!(matches!(
        error,
        BreakGlassError::RecordingTampered { frame: 1 }
    ));
}

#[test]
fn the_size_cap_writes_an_explicit_truncation_marker_and_stops() {
    let dir = TempDir::new("cap");
    let mut recorder = FileRecorder::create(dir.path(), SESSION, TENANT, 1000, 200).unwrap();
    recorder
        .append(FrameKind::Output, 1000, b"first", true)
        .unwrap();
    let error = recorder
        .append(FrameKind::Output, 1001, b"second that will not fit", true)
        .unwrap_err();
    assert_eq!(error, BreakGlassError::RecordingFull);
    assert!(recorder.truncated());
    assert_eq!(
        recorder
            .append(FrameKind::Output, 1002, b"third", true)
            .unwrap_err(),
        BreakGlassError::RecordingFull
    );

    let replayed = replay(&recorder_path(&dir, SESSION));
    assert!(replayed.verified(), "the marker is a valid chained frame");
    assert!(replayed.truncated());
    let frames = replayed.frames();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[1].kind, FrameKind::Truncation);
    assert_eq!(frames[1].dropped_after_seq(), Some(1));
}

fn recorder_path(dir: &TempDir, session: &str) -> std::path::PathBuf {
    FileRecorder::recording_path(dir.path(), session)
}

fn manifest_path(dir: &TempDir, session: &str) -> std::path::PathBuf {
    FileRecorder::manifest_path(dir.path(), session)
}

proptest! {
    #[test]
    fn frames_round_trip_through_their_bytes(
        seq in 1u64..100_000,
        at in -1_000_000i64..1_000_000,
        data in proptest::collection::vec(any::<u8>(), 0..128),
        redacted in any::<bool>(),
    ) {
        let frame = Frame::new(seq, at, FrameKind::Output, redacted, data, [7u8; 32]);
        let bytes = frame.to_bytes();
        let (decoded, used) = Frame::decode(&bytes).unwrap();
        prop_assert_eq!(used, bytes.len());
        prop_assert_eq!(decoded, frame.clone());
        prop_assert_eq!(frame.recompute_hash(), frame.hash);
    }
}

#[test]
fn a_session_id_cannot_escape_the_recording_directory() {
    use spot_break_glass::FileRecorder;
    let dir = std::env::temp_dir().join(format!("bg-escape-{}", std::process::id()));
    for bad in ["../evil", "a/b", ".hidden", "", "a\\b"] {
        assert!(
            FileRecorder::create(&dir, bad, "tenant-a", 0, 1024).is_err(),
            "{bad:?} must be refused"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
