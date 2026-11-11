//! The fixture loader: `fixtures/release-1/` in, a memory store and a plan out.
//!
//! Loading is deliberately more than copying bytes:
//!
//! * every file under `objects/` must be a canonical journal key
//!   ([`spot_journal_store::Key::parse`]) and is stored under exactly that key, so the loader and
//!   the generator cannot disagree about the layout;
//! * the snapshot is **read in full** ([`spot_journal_snapshot::verify_deep`]: its index
//!   signature, every part's hash, every image line) before the planner is told it is verified —
//!   a snapshot only becomes a usable base once something has read it;
//! * the catalogue the planner takes is derived from the objects themselves (the manifest's
//!   segment summaries, the snapshot's signed index), not from a separate description file, so it
//!   cannot drift from the journal that was loaded.
//!
//! Time, keys and the store are all arguments or committed bytes; nothing here reads a clock, a
//! random source or the network, and the store is the in-memory reference backend.

use std::path::{Path, PathBuf};

use spot_journal_manifest::{SignedManifest, VerifyingKeys};
use spot_journal_rebuild::{RebuildKeys, RebuildOptions};
use spot_journal_rebuild_plan::{
    JournalCatalog, RebuildPlan, SegmentRange, SnapshotInfo, Target, plan,
};
use spot_journal_snapshot::{list_snapshots, verify_deep};
use spot_journal_store::{JournalStore, Key, Layout, MemoryStore, TenantId};

use crate::error::CompatError;
use crate::golden::Golden;
use crate::keys::FixtureKeys;
use crate::{
    FIXTURE_NAME, KEY_FILE, LAST_POSITION, SNAPSHOT_POSITION, SOURCE, TARGET_SCHEMA, TENANT,
    block_on,
};

/// The committed fixture directory of this checkout.
#[must_use]
pub fn default_fixture_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(FIXTURE_NAME)
}

/// A loaded frozen journal, ready to plan and rebuild.
pub struct Fixture {
    /// Every object of the journal, at its canonical key.
    pub store: MemoryStore,
    /// The manifest keys, from `key.pub`.
    pub manifest_keys: VerifyingKeys,
    /// The snapshot keys, from `key.pub`.
    pub snapshot_keys: FixtureKeys,
    /// What the journal holds, derived from the manifest and the snapshot index.
    pub catalog: JournalCatalog,
    /// The committed expectation, per position.
    pub golden: Golden,
}

impl Fixture {
    /// Loads the fixture in `root`.
    ///
    /// # Errors
    /// [`CompatError`] when a file is missing, an object key is not a journal key, the snapshot
    /// does not verify, the manifest does not decode, or the golden state does not parse.
    pub fn load(root: &Path) -> Result<Self, CompatError> {
        let key_text = std::fs::read_to_string(root.join(KEY_FILE))
            .map_err(|err| CompatError::new(format!("cannot read the public key: {err}")))?;
        let snapshot_keys = FixtureKeys::from_hex(&key_text)?;
        let manifest_keys = snapshot_keys.manifest_keys();

        let store = MemoryStore::new();
        for (key, bytes) in object_files(root)? {
            Key::parse(&key)?;
            block_on(store.put_new(&key, bytes))?;
        }

        let images = block_on(verify_deep(
            &store,
            TENANT,
            1,
            SNAPSHOT_POSITION,
            &snapshot_keys,
        ))
        .map_err(|err| {
            CompatError::new(format!("the fixture's snapshot does not verify: {err}"))
        })?;

        let manifest_key = Layout::manifest(TENANT, 1)?.to_key();
        let manifest = block_on(store.get(&manifest_key))?
            .ok_or_else(|| CompatError::new("the fixture has no manifest for generation 1"))?;
        let signed = SignedManifest::from_bytes(&manifest.bytes)?;
        let segments: Vec<SegmentRange> = signed
            .manifest
            .segments
            .iter()
            .map(|summary| SegmentRange {
                generation: summary.generation,
                first_seq: summary.first_seq,
                last_seq: summary.last_seq,
            })
            .collect();

        // The planner takes the snapshots the store lists, and only a snapshot with the signed
        // verified marker is trusted as a base (WP-JRN-05.T2). The fixture is loaded through that
        // path rather than around it, so the harness proves the listing too.
        let listed: Vec<SnapshotInfo> = block_on(list_snapshots(&store, TENANT, &snapshot_keys))?;
        let mut verified: Vec<SnapshotInfo> = listed
            .into_iter()
            .filter(|snapshot| snapshot.verified)
            .collect();
        if verified.len() != 1 {
            return Err(CompatError::new(format!(
                "the fixture must list exactly one verified snapshot, and lists {}",
                verified.len()
            )));
        }
        let snapshot = verified
            .pop()
            .ok_or_else(|| CompatError::new("the fixture lists no verified snapshot"))?;

        let golden_text = std::fs::read_to_string(root.join("golden-state.json"))
            .map_err(|err| CompatError::new(format!("cannot read golden-state.json: {err}")))?;
        let golden = Golden::parse(&golden_text)?;
        if golden.target_schema != TARGET_SCHEMA || golden.last.position != LAST_POSITION {
            return Err(CompatError::new(
                "the golden state describes another schema or another last position",
            ));
        }
        let at_snapshot = golden.checkpoint(SNAPSHOT_POSITION).ok_or_else(|| {
            CompatError::new("the golden state has no checkpoint at the snapshot")
        })?;
        if usize::try_from(images).ok() != Some(at_snapshot.records.len()) {
            return Err(CompatError::new(
                "the snapshot holds another number of images than the golden state at its position",
            ));
        }

        Ok(Self {
            store,
            manifest_keys,
            snapshot_keys,
            catalog: JournalCatalog {
                snapshots: vec![snapshot],
                segments,
                forks: Vec::new(),
            },
            golden,
        })
    }

    /// The plan a rebuild to `target` runs.
    ///
    /// # Errors
    /// [`CompatError`] when the journal and the target do not make a plan.
    pub fn plan(&self, target: Target) -> Result<RebuildPlan, CompatError> {
        Ok(plan(&self.catalog, None, target)?)
    }

    /// The keys the executor trusts: the fixture's throwaway public key for both the manifests and
    /// the snapshot index.
    #[must_use]
    pub fn rebuild_keys(&self) -> RebuildKeys<'_> {
        RebuildKeys {
            manifest: &self.manifest_keys,
            snapshot: &self.snapshot_keys,
        }
    }

    /// The rebuild options: this tenant and source, upcasting every image to the current schema,
    /// with the light verification, the prescan and audit replay on.
    ///
    /// # Errors
    /// [`CompatError`] when the tenant id is not usable.
    pub fn options(&self) -> Result<RebuildOptions<'static>, CompatError> {
        Ok(RebuildOptions::new(
            TenantId::new(TENANT)?,
            SOURCE,
            TARGET_SCHEMA,
        ))
    }
}

/// Every object file under `root/objects`, as `(key, bytes)`, sorted by key.
fn object_files(root: &Path) -> Result<Vec<(String, Vec<u8>)>, CompatError> {
    let objects = root.join("objects");
    let mut found: Vec<(String, Vec<u8>)> = Vec::new();
    walk(&objects, &objects, &mut found)?;
    found.sort_by(|a, b| a.0.cmp(&b.0));
    if found.is_empty() {
        return Err(CompatError::new(format!(
            "{} holds no objects",
            objects.display()
        )));
    }
    Ok(found)
}

/// Collects the files under `dir`, keyed by their path relative to `base` with `/` separators.
fn walk(base: &Path, dir: &Path, found: &mut Vec<(String, Vec<u8>)>) -> Result<(), CompatError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|err| CompatError::new(format!("cannot read {}: {err}", dir.display())))?;
    for entry in entries {
        let entry = entry
            .map_err(|err| CompatError::new(format!("cannot read {}: {err}", dir.display())))?;
        let path = entry.path();
        if path.is_dir() {
            walk(base, &path, found)?;
            continue;
        }
        let relative = path
            .strip_prefix(base)
            .map_err(|_| CompatError::new("a fixture file is outside the objects directory"))?;
        let key = relative
            .components()
            .map(|part| part.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<String>>()
            .join("/");
        let bytes = std::fs::read(&path)
            .map_err(|err| CompatError::new(format!("cannot read {}: {err}", path.display())))?;
        found.push((key, bytes));
    }
    Ok(())
}
