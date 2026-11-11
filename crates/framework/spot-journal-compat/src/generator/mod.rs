//! The fixture generator: the frozen journal's bytes, and the golden state they must rebuild into.
//!
//! [`build`] produces every byte the fixture directory holds, in memory, and [`write`] puts them
//! on disk. Both are deterministic — no clock, no random source, no filesystem read decides
//! anything — so the determinism test can regenerate the fixture and compare it with the
//! committed bytes file by file. A format change that would silently alter an old journal
//! therefore turns the test red instead of quietly rewriting history.
//!
//! What it writes, under `fixtures/release-1/`:
//!
//! * `objects/<key>` — the store's key space as files: two sealed segments, the signed manifest of
//!   generation 1, and the snapshot at position 5 (two parts and its signed `index.json`). The
//!   keys are `spot-journal-store`'s own `Layout`, so the loader rebuilds exactly the tree a real
//!   journal has.
//! * `key.pub` — the TEST-ONLY public key, 64 hex digits.
//! * `golden-state.json` — the state the rebuild must produce, per position (see
//!   [`crate::Golden`]).
//!
//! The snapshot is taken against the manifest as it stood at position 5 (one segment), which is
//! what a snapshot is: the manifest hash it records is the one that was current when the
//! database read was taken, not the one that lists segments written later.

mod events;
pub mod key;

use std::fs;
use std::path::Path;

use serde_json::Value;
use spot_events_contract::envelope::Position;
use spot_image_upcast::Image;
use spot_journal_manifest::{Manifest, SegmentSummary, append_segments, manifest_hash, sign};
use spot_journal_segment::{SealedSegment, SegmentConfig, SegmentWriter};
use spot_journal_snapshot::{ImageRecord, SnapshotParams, SnapshotWriter, mark_verified};
use spot_journal_store::{JournalStore, Layout, MemoryStore};

use crate::error::CompatError;
use crate::golden::{Checkpoint, Golden, Record};
use crate::json::hex;
use crate::keys::KEY_ID;
use crate::registry::today;
use crate::{
    FIXTURE_NAME, KEY_FILE, LAST_POSITION, SEGMENT_ONE_LAST, SNAPSHOT_POSITION, SOURCE,
    TARGET_SCHEMA, TENANT, block_on,
};

/// When the frozen release issued its manifest, in Unix milliseconds (a fixture constant, not a
/// clock).
pub const ISSUED_AT_MILLIS: i64 = 1_772_000_000_000;

/// When the frozen release took its snapshot, in Unix milliseconds.
pub const TAKEN_AT_MILLIS: i64 = 1_772_000_600_000;

/// How many uncompressed bytes a snapshot part holds before it is closed: small, so the fixture's
/// snapshot has more than one part and the reader's part-by-part verification is exercised.
const SNAPSHOT_PART_BYTES: usize = 300;

/// How many uncompressed bytes a segment block holds before it is closed: small, so each of the
/// two segments has more than one block.
const SEGMENT_BLOCK_TARGET: usize = 400;

/// Every file the fixture directory holds, as `(path, bytes)` with `/` separators.
pub struct Generated {
    /// The files, sorted by path.
    pub files: Vec<(String, Vec<u8>)>,
}

impl Generated {
    /// The bytes of `path`, when the fixture holds it.
    #[must_use]
    pub fn file(&self, path: &str) -> Option<&[u8]> {
        self.files
            .iter()
            .find(|(name, _)| name == path)
            .map(|(_, bytes)| bytes.as_slice())
    }
}

/// The two sealed segments of the frozen journal, chained in generation 1.
///
/// The first holds positions `1..=SEGMENT_ONE_LAST`, the second the rest; the second's header
/// commits to the hash of the first's bytes, so a rebuild cannot quietly read them out of order.
fn sealed_segments(script: &[events::Event]) -> Result<Vec<SealedSegment>, CompatError> {
    let split = usize::try_from(SEGMENT_ONE_LAST)
        .map_err(|_| CompatError::new("the first segment's range does not fit memory"))?;
    let mut sealed: Vec<SealedSegment> = Vec::new();
    for chunk in [&script[..split], &script[split..]] {
        let mut config = match sealed.last() {
            Some(previous) => SegmentConfig::after(SOURCE, 1, previous.hash),
            None => SegmentConfig::first(SOURCE, 1),
        };
        config.block_target = SEGMENT_BLOCK_TARGET;
        let mut writer = SegmentWriter::new(config)?;
        for event in chunk {
            let line = events::line(event);
            writer.push(
                Position {
                    generation: 1,
                    seq: event.position,
                },
                line.as_bytes(),
            )?;
        }
        sealed.push(writer.seal(["1", "2", "3"])?);
    }
    Ok(sealed)
}

/// The summary a manifest lists for `segment`.
fn summary(segment: &SealedSegment) -> SegmentSummary {
    let header = &segment.header;
    SegmentSummary::new(
        header.generation,
        header.first_seq,
        header.last_seq,
        header.entry_count,
        segment.hash,
        header.prev_hash,
    )
}

/// The state after `upto` positions, upcast to the schema the rebuilt database must be in.
///
/// The fold runs over the frozen script, not over the rebuilt journal: the golden state is an
/// independent statement of what the position holds, so a rebuild that misorders, skips or
/// re-versions an entry disagrees with it.
fn golden_state(script: &[events::Event], upto: u64) -> Result<Vec<Record>, CompatError> {
    let registry = today()?;
    let mut records = Vec::new();
    for stored in events::stored_state(script, upto) {
        let image = Image::new(stored.entity.clone(), stored.schema, stored.body.clone());
        let upcast = registry.upcast(image, TARGET_SCHEMA)?;
        records.push(Record {
            entity: stored.entity,
            id: stored.id,
            version: stored.version,
            schema: upcast.entity_schema(),
            body: upcast.body().clone(),
        });
    }
    Ok(records)
}

/// Writes the snapshot of the state at [`SNAPSHOT_POSITION`] into `store`.
fn write_snapshot(
    store: &MemoryStore,
    script: &[events::Event],
    manifest_hash_at_snapshot: String,
) -> Result<(), CompatError> {
    let params = SnapshotParams {
        tenant: TENANT.to_owned(),
        generation: 1,
        position: SNAPSHOT_POSITION,
        manifest_hash: manifest_hash_at_snapshot,
        taken_at_millis: TAKEN_AT_MILLIS,
    };
    let signer = key::FixtureSigner::new();
    block_on(async {
        let mut writer =
            SnapshotWriter::new(store, params, &signer)?.with_part_bytes(SNAPSHOT_PART_BYTES)?;
        writer
            .write_part(
                events::stored_state(script, SNAPSHOT_POSITION)
                    .into_iter()
                    .map(|stored| ImageRecord {
                        entity: stored.entity,
                        id: stored.id,
                        version: stored.version,
                        entity_schema: stored.schema,
                        image: Value::Object(stored.body),
                    }),
            )
            .await?;
        writer.finish().await?;
        // The rebuild test marked the snapshot verified (WP-JRN-05.T2's signed marker): that is
        // what makes it a base a planner may restore, and the fixture carries it for the same
        // reason a real journal would.
        mark_verified(store, TENANT, 1, SNAPSHOT_POSITION, &signer.keys(), &signer).await?;
        Ok::<(), CompatError>(())
    })
}

/// Every object `store` holds, as `(key, bytes)`, sorted by key.
fn objects(store: &MemoryStore) -> Result<Vec<(String, Vec<u8>)>, CompatError> {
    let mut found: Vec<(String, Vec<u8>)> = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = block_on(store.list("", after.as_deref(), 1000))?;
        if page.is_empty() {
            break;
        }
        after = page.last().map(|entry| entry.key.clone());
        for entry in page {
            let object = block_on(store.get(&entry.key))?
                .ok_or_else(|| CompatError::new(format!("the object {} is gone", entry.key)))?;
            found.push((entry.key, object.bytes));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Every byte the fixture directory holds, in memory.
///
/// # Errors
/// [`CompatError`] when the journal cannot be sealed, signed or snapshotted, or a document cannot
/// be upcast — a bug in this crate, never bad input.
pub fn build() -> Result<Generated, CompatError> {
    let script = events::script();
    let segments = sealed_segments(&script)?;
    let summaries: Vec<SegmentSummary> = segments.iter().map(summary).collect();

    let first = Manifest::first(SOURCE, ISSUED_AT_MILLIS)?;
    let at_snapshot = append_segments(&first, &summaries[..1])?;
    let full = append_segments(&first, &summaries)?;
    let signer = key::FixtureSigner::new();
    let signed = sign(&full, signer.signing_key(), KEY_ID);

    let store = MemoryStore::new();
    block_on(store.put_new(&Layout::manifest(TENANT, 1)?.to_key(), signed.to_bytes()))?;
    for segment in &segments {
        let header = &segment.header;
        let key =
            Layout::segment(TENANT, header.generation, header.first_seq, header.last_seq)?.to_key();
        block_on(store.put_new(&key, segment.bytes.clone()))?;
    }
    write_snapshot(&store, &script, hex(&manifest_hash(&at_snapshot)))?;

    let mut files: Vec<(String, Vec<u8>)> = objects(&store)?
        .into_iter()
        .map(|(key, bytes)| (format!("objects/{key}"), bytes))
        .collect();
    files.push((
        KEY_FILE.to_owned(),
        format!("{}\n", signer.keys().to_hex()).into_bytes(),
    ));

    let mut prefixes: Vec<Checkpoint> = Vec::new();
    for position in 1..=LAST_POSITION {
        prefixes.push(Checkpoint::new(position, golden_state(&script, position)?));
    }
    let last = golden_state(&script, LAST_POSITION)?;
    let golden = Golden::new(
        TARGET_SCHEMA,
        Checkpoint::new(LAST_POSITION, last),
        prefixes,
    );
    files.push((
        "golden-state.json".to_owned(),
        golden.to_text()?.into_bytes(),
    ));
    files.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Generated { files })
}

/// Writes the fixture into `dir`, creating it when it is absent, and returns how many files it
/// wrote (a file whose bytes already match is left alone).
///
/// # Errors
/// [`CompatError`] when the fixture cannot be built or a file cannot be written.
pub fn write(dir: &Path) -> Result<usize, CompatError> {
    let generated = build()?;
    let mut written = 0;
    for (name, bytes) in &generated.files {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|err| {
                CompatError::new(format!("cannot create {}: {err}", parent.display()))
            })?;
        }
        if fs::read(&path).is_ok_and(|existing| existing == *bytes) {
            continue;
        }
        fs::write(&path, bytes)
            .map_err(|err| CompatError::new(format!("cannot write {}: {err}", path.display())))?;
        written += 1;
    }
    Ok(written)
}

/// The default fixture directory, when the binary is run with no argument.
#[must_use]
pub fn default_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(FIXTURE_NAME)
}
