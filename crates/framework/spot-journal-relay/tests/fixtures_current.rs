//! The committed tail chunk fixtures are present, untouched and still readable.
//!
//! `tests/fixtures/tail/` holds one chunk of each version with the decode it must produce, for a
//! second implementation and for a future change of this crate to test against (ADR-061
//! Appendix A, see the README there). The version 2 sealed form carries random nonces, so its
//! bytes cannot be rebuilt in a test: they are generated once by [`write_v2_fixture`] and checked
//! in. These tests fail, with the instruction to follow, when a file is missing, when the chunk
//! no longer matches the digest its expected file records (someone edited one of them), or when
//! either fixture stops decoding to the entries the expected file lists.
#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use spot_events_contract::envelope::Position;
use spot_journal_relay::TailKeys;
use spot_journal_relay::tail::{self, TailChunk};
use spot_journal_store::Layout;

const TENANT: &str = "acme";
const GENERATION: u64 = 1;
const BATCH_ID: u64 = 1;
const ENTRY_COUNT: u64 = 3;

const V1_CHUNK: &str = "v1-gen1-batch1.chunk";
const V1_EXPECTED: &str = "v1-gen1-batch1.expected.txt";
const V2_CHUNK: &str = "v2-gen1-batch1.chunk";
const V2_EXPECTED: &str = "v2-gen1-batch1.expected.txt";

const REGENERATE: &str = "run `cargo test -p spot-journal-relay --test fixtures_current write_v2_fixture -- --ignored` \
and commit both v2 files (only after a deliberate format change, see tests/fixtures/tail/README.md)";

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tail")
}

fn read_fixture(name: &str) -> Vec<u8> {
    std::fs::read(dir().join(name)).unwrap_or_else(|_| {
        panic!("the fixture tests/fixtures/tail/{name} is missing: {REGENERATE}")
    })
}

fn read_text(name: &str) -> String {
    String::from_utf8(read_fixture(name)).expect("an expected file is UTF-8")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn digest(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn object_key() -> String {
    Layout::tail_chunk(TENANT, GENERATION, BATCH_ID)
        .expect("a valid tail key")
        .to_key()
}

/// The envelope line of entry `seq`, the same three lines in both versions.
fn line_of(seq: u64) -> String {
    format!("{{\"g\":{GENERATION},\"seq\":{seq},\"data\":\"entry number {seq}\"}}")
}

fn entries() -> Vec<(Position, Vec<u8>)> {
    (1..=ENTRY_COUNT)
        .map(|seq| {
            (
                Position {
                    generation: GENERATION,
                    seq,
                },
                line_of(seq).into_bytes(),
            )
        })
        .collect()
}

/// The value of the `name: value` line of an expected file.
fn field<'a>(expected: &'a str, name: &str) -> Option<&'a str> {
    expected
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix(": "))
}

/// The `seq N: <line>` rows of an expected file, in order.
fn rows(expected: &str) -> Vec<(u64, String)> {
    expected
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix("seq ")?;
            let (seq, text) = rest.split_once(": ")?;
            Some((seq.parse().ok()?, text.to_owned()))
        })
        .collect()
}

fn assert_decodes_to_expected(chunk: &TailChunk, expected: &str, version: u8) {
    assert_eq!(chunk.version, version);
    assert_eq!(
        field(expected, "version"),
        Some(version.to_string().as_str())
    );
    assert_eq!(field(expected, "key"), Some(object_key().as_str()));
    assert_eq!(field(expected, "tenant").unwrap_or(TENANT), TENANT);
    assert_eq!(
        field(expected, "generation"),
        Some(GENERATION.to_string().as_str())
    );
    assert_eq!(
        field(expected, "batch_id"),
        Some(chunk.batch_id.to_string().as_str())
    );
    assert_eq!(
        field(expected, "entries"),
        Some(chunk.entries.len().to_string().as_str())
    );
    let listed = rows(expected);
    assert_eq!(listed.len(), chunk.entries.len());
    for ((seq, text), (position, line)) in listed.iter().zip(&chunk.entries) {
        assert_eq!(*seq, position.seq);
        assert_eq!(text.as_bytes(), line.as_slice());
    }
}

#[tokio::test]
async fn the_version_1_fixture_decodes_to_its_expected_file() {
    let bytes = read_fixture(V1_CHUNK);
    let expected = read_text(V1_EXPECTED);
    let chunk = tail::decode(TENANT, GENERATION, None, &object_key(), &bytes)
        .await
        .expect("the version 1 fixture decodes (it needs no keys)");
    assert_decodes_to_expected(&chunk, &expected, 1);
}

#[tokio::test]
async fn the_version_2_fixture_is_present_unedited_and_decodes_to_its_expected_file() {
    let bytes = read_fixture(V2_CHUNK);
    let expected = read_text(V2_EXPECTED);

    // The expected file records the digest of the chunk: an edit of either file is caught here.
    let recorded = field(&expected, "content_sha256")
        .unwrap_or_else(|| panic!("{V2_EXPECTED} has no content_sha256 line: {REGENERATE}"));
    assert_eq!(
        recorded,
        digest(&bytes),
        "{V2_CHUNK} does not match the digest in {V2_EXPECTED}: one of them was edited; {REGENERATE}"
    );

    // The fixed test tail keys of the tenant open it.
    let keys = TailKeys::for_tests(TENANT);
    let chunk = tail::decode(TENANT, GENERATION, Some(&keys), &object_key(), &bytes)
        .await
        .unwrap_or_else(|error| {
            panic!("{V2_CHUNK} no longer decodes ({error}): a reader change broke today's bytes")
        });
    assert_decodes_to_expected(&chunk, &expected, 2);
    assert_eq!(chunk.entries, entries());
    assert_eq!(
        field(&expected, "first_seq"),
        Some(chunk.first_seq().to_string().as_str())
    );
    assert_eq!(
        field(&expected, "last_seq"),
        Some(chunk.last_seq().to_string().as_str())
    );
}

/// Writes `v2-gen1-batch1.chunk` and its expected file. The sealed form carries random nonces, so
/// every run produces different bytes: run it by hand after a deliberate format change (an
/// ADR-061 Appendix A change), never in CI, and commit both files together. The guard above fails
/// until the pair is consistent.
#[tokio::test]
#[ignore = "writes tests/fixtures/tail/v2-gen1-batch1.chunk and its expected file (random nonces)"]
async fn write_v2_fixture() {
    let keys = TailKeys::for_tests(TENANT);
    let entries = entries();
    let bytes = tail::encode_v2(&keys, &object_key(), BATCH_ID, &entries)
        .await
        .expect("encodes");
    let chunk = tail::decode(TENANT, GENERATION, Some(&keys), &object_key(), &bytes)
        .await
        .expect("the fresh chunk decodes");
    let mut text = String::new();
    let _ = writeln!(text, "key: {}", object_key());
    let _ = writeln!(text, "version: 2");
    let _ = writeln!(text, "tenant: {TENANT}");
    let _ = writeln!(text, "generation: {GENERATION}");
    let _ = writeln!(text, "batch_id: {BATCH_ID}");
    let _ = writeln!(text, "entries: {}", chunk.entries.len());
    let _ = writeln!(text, "first_seq: {}", chunk.first_seq());
    let _ = writeln!(text, "last_seq: {}", chunk.last_seq());
    let _ = writeln!(text, "content_sha256: {}", digest(&bytes));
    let _ = writeln!(
        text,
        "keys: TailKeys::for_tests(\"{TENANT}\") (public test keys, never a real tenant key)"
    );
    for (position, line) in &chunk.entries {
        let _ = writeln!(
            text,
            "seq {}: {}",
            position.seq,
            String::from_utf8_lossy(line)
        );
    }
    std::fs::write(dir().join(V2_CHUNK), &bytes).expect("writes the chunk");
    std::fs::write(dir().join(V2_EXPECTED), text).expect("writes the expected file");
}
