//! The driver: one question, "is what the store holds for this tenant and source a complete,
//! untampered journal?"

use std::collections::{BTreeMap, BTreeSet};

use spot_journal_manifest::{SegmentSummary, SignedManifest, VerifyingKeys, check_against, verify};
use spot_journal_segment::{SegmentReader, segment_hash};
use spot_journal_store::{
    JournalStore, Key, Layout, MANIFEST_PREFIX, ObjectEntry, SEGMENTS_PREFIX,
};

use crate::cursor::VerifyCursor;
use crate::error::VerifyError;
use crate::finding::{FindingKind, VerifyFinding, from_manifest};
use crate::report::{Expected, Mode, PinnedExpected, VerifyOptions, VerifyReport};

/// How many keys one `list` call asks for.
const PAGE: usize = 1000;

/// Verifies `expected.tenant`'s journal with the default options (a full pass, 256 MB budget).
///
/// With `expected.generation == None` this is **discovery only**: the newest manifest the store
/// lists is verified, so a rollback that deleted or hid the newest manifest passes. Use
/// [`verify_pinned`] when the generation is known.
///
/// # Errors
/// [`VerifyError`] when the question cannot be answered; everything wrong with the journal is a
/// finding in the report.
pub async fn verify_tenant<S: JournalStore + ?Sized>(
    store: &S,
    keys: &VerifyingKeys,
    expected: &Expected,
    mode: Mode,
) -> Result<VerifyReport, VerifyError> {
    verify_tenant_with(store, keys, expected, mode, VerifyOptions::default()).await
}

/// Verifies `expected.tenant`'s journal, reading no more than `options.budget_bytes` and skipping
/// what `options.cursor` says an earlier pass verified.
///
/// The expected tenant, source and generation come from the caller; the keys are the tenant's
/// public journal keys. Nothing is taken from the objects being checked: a manifest signed with
/// the right key but copied from another source or another generation is a finding.
///
/// Light mode reads the manifest and asks the store for the metadata of each listed segment (the
/// object's etag is the SHA-256 of its bytes, so the hash is compared without reading it). Deep
/// mode reads each segment too and checks its header against the manifest's summary, its block
/// checksums and its link to the segment before it. Segments of the tenant's prefix are listed
/// once; tail chunks are never looked at.
///
/// # Errors
/// [`VerifyError::Store`] when the store fails, [`VerifyError::ExpectedInvalid`] for an empty
/// source or generation zero, and [`VerifyError::BudgetTooSmall`] when the budget cannot cover the
/// manifest plus the next segment, so no run could make progress.
pub async fn verify_tenant_with<S: JournalStore + ?Sized>(
    store: &S,
    keys: &VerifyingKeys,
    expected: &Expected,
    mode: Mode,
    options: VerifyOptions,
) -> Result<VerifyReport, VerifyError> {
    if expected.source.trim().is_empty() {
        return Err(VerifyError::ExpectedInvalid("the expected source is empty"));
    }
    if expected.generation == Some(0) {
        return Err(VerifyError::ExpectedInvalid(
            "generations start at 1, so generation 0 cannot be expected",
        ));
    }
    run_pass(store, keys, expected, mode, options, false).await
}

/// Verifies the journal at the generation the caller expects, with the default options.
///
/// # Errors
/// As [`verify_pinned_with`].
pub async fn verify_pinned<S: JournalStore + ?Sized>(
    store: &S,
    keys: &VerifyingKeys,
    expected: &PinnedExpected,
    mode: Mode,
) -> Result<VerifyReport, VerifyError> {
    verify_pinned_with(store, keys, expected, mode, VerifyOptions::default()).await
}

/// Verifies the journal at `expected.generation`, as [`verify_tenant_with`] does, but the
/// generation is mandatory: that generation's manifest is the one verified and no listing decides
/// which. A missing manifest is [`FindingKind::MissingManifest`], never a fall back to an older
/// generation; a stored manifest of a higher generation is
/// [`FindingKind::NewerGenerationPresent`] (the pin is stale). The report has
/// [`VerifyReport::pinned`] set.
///
/// # Errors
/// [`VerifyError::ExpectedInvalid`] for an empty source or generation 0, otherwise as
/// [`verify_tenant_with`].
pub async fn verify_pinned_with<S: JournalStore + ?Sized>(
    store: &S,
    keys: &VerifyingKeys,
    expected: &PinnedExpected,
    mode: Mode,
    options: VerifyOptions,
) -> Result<VerifyReport, VerifyError> {
    expected.validate()?;
    run_pass(store, keys, &expected.as_expected(), mode, options, true).await
}

async fn run_pass<S: JournalStore + ?Sized>(
    store: &S,
    keys: &VerifyingKeys,
    expected: &Expected,
    mode: Mode,
    options: VerifyOptions,
    pinned: bool,
) -> Result<VerifyReport, VerifyError> {
    let mut run = Run {
        store,
        expected,
        pinned,
        mode,
        budget: options.budget_bytes,
        bytes_read: 0,
    };
    run.execute(keys, options.cursor.as_ref()).await
}

/// The state of one call.
struct Run<'a, S: ?Sized> {
    store: &'a S,
    expected: &'a Expected,
    pinned: bool,
    mode: Mode,
    budget: u64,
    bytes_read: u64,
}

impl<S: JournalStore + ?Sized> Run<'_, S> {
    fn finding(&self, generation: u64, first: u64, last: u64, kind: FindingKind) -> VerifyFinding {
        VerifyFinding {
            tenant: self.expected.tenant.as_str().to_owned(),
            generation,
            first_seq: first,
            last_seq: last,
            kind,
        }
    }

    /// The report of a pass that stopped at the manifest.
    fn stopped(&self, generation: u64, kind: FindingKind) -> VerifyReport {
        VerifyReport {
            tenant: self.expected.tenant.as_str().to_owned(),
            source: self.expected.source.clone(),
            generation,
            mode: self.mode,
            pinned: self.pinned,
            findings: vec![self.finding(generation, 0, 0, kind)],
            segments_listed: 0,
            segments_skipped: 0,
            segments_examined: 0,
            bytes_read: self.bytes_read,
            partial: false,
            cursor: VerifyCursor::empty(self.expected, generation, self.mode),
        }
    }

    /// Every object under `prefix`, paged.
    async fn list_all(&self, prefix: &str) -> Result<Vec<ObjectEntry>, VerifyError> {
        let mut all = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = self.store.list(prefix, after.as_deref(), PAGE).await?;
            let count = page.len();
            after = page.last().map(|entry| entry.key.clone());
            all.extend(page);
            if count < PAGE || after.is_none() {
                return Ok(all);
            }
        }
    }

    /// The newest generation whose manifest the store lists under the tenant's prefix.
    async fn newest_listed(&self) -> Result<Option<u64>, VerifyError> {
        let prefix = format!("{}/{MANIFEST_PREFIX}/", self.expected.tenant);
        let all = self.list_all(&prefix).await?;
        Ok(all
            .iter()
            .filter_map(|entry| match Key::parse(&entry.key) {
                Ok(Key::Manifest(key)) if key.tenant == self.expected.tenant => {
                    Some(key.generation)
                }
                _ => None,
            })
            .max())
    }

    /// The generation whose manifest is verified: the expected one, or the newest stored.
    async fn manifest_generation(&self) -> Result<Option<u64>, VerifyError> {
        if let Some(generation) = self.expected.generation {
            return Ok(Some(generation));
        }
        self.newest_listed().await
    }

    /// Runs the pass; for a pinned pass also reports a stored manifest of a higher generation.
    async fn execute(
        &mut self,
        keys: &VerifyingKeys,
        cursor: Option<&VerifyCursor>,
    ) -> Result<VerifyReport, VerifyError> {
        let mut report = self.verify_manifest(keys, cursor).await?;
        if self.pinned
            && let Some(newest) = self.newest_listed().await?
            && newest > report.generation
        {
            report
                .findings
                .push(self.finding(newest, 0, 0, FindingKind::NewerGenerationPresent));
            report.findings.sort_unstable();
            report.findings.dedup();
        }
        Ok(report)
    }

    async fn verify_manifest(
        &mut self,
        keys: &VerifyingKeys,
        cursor: Option<&VerifyCursor>,
    ) -> Result<VerifyReport, VerifyError> {
        let tenant = self.expected.tenant.as_str().to_owned();
        let Some(generation) = self.manifest_generation().await? else {
            return Ok(self.stopped(0, FindingKind::MissingManifest));
        };

        // The manifest: present, authentic, and the tenant's own.
        let manifest_key = Layout::manifest(&tenant, generation)?.to_key();
        let Some(meta) = self.store.head(&manifest_key).await? else {
            return Ok(self.stopped(generation, FindingKind::MissingManifest));
        };
        self.check_budget(meta.size, 0)?;
        let Some(object) = self.store.get(&manifest_key).await? else {
            return Ok(self.stopped(generation, FindingKind::MissingManifest));
        };
        self.bytes_read = self.bytes_read.saturating_add(length(&object.bytes));
        let signed = match SignedManifest::from_bytes(&object.bytes) {
            Ok(signed) => signed,
            Err(refusal) => {
                let kind = FindingKind::ManifestRejected {
                    reason: refusal.code(),
                };
                return Ok(self.stopped(generation, kind));
            }
        };
        let verified = match verify(&signed, keys) {
            Ok(verified) => verified,
            Err(refusal) => {
                let kind = FindingKind::ManifestRejected {
                    reason: refusal.code(),
                };
                return Ok(self.stopped(generation, kind));
            }
        };
        if verified.source() != self.expected.source || verified.generation() != generation {
            return Ok(self.stopped(generation, FindingKind::ForeignManifest));
        }

        // What the store holds under the tenant's segment prefix, all generations.
        let listing = self
            .list_all(&format!("{tenant}/{SEGMENTS_PREFIX}/"))
            .await?;
        let mut current: BTreeMap<(u64, u64), u64> = BTreeMap::new();
        let mut observed: Vec<SegmentSummary> = Vec::new();
        for entry in &listing {
            if let Ok(Key::Segment(key)) = Key::parse(&entry.key)
                && key.tenant == self.expected.tenant
            {
                if key.generation == generation {
                    current.insert((key.first_seq, key.last_seq), entry.size);
                } else {
                    observed.push(implied(
                        key.generation,
                        key.first_seq,
                        key.last_seq,
                        [0; 32],
                    ));
                }
            }
        }

        let segments = verified.segments();
        let mut findings: Vec<VerifyFinding> = Vec::new();

        // What an earlier pass already verified.
        let mut skip = 0_usize;
        if let Some(earlier) = cursor
            && earlier.is_about(self.expected, generation)
            && earlier.mode() >= self.mode
        {
            if earlier.still_covers(segments) {
                skip = earlier.verified_segments();
            } else {
                findings.push(self.finding(
                    generation,
                    1,
                    earlier.last_seq(),
                    FindingKind::ManifestRegressed,
                ));
            }
        }

        // The listed segments: examine each one the budget allows.
        let mut cut = segments.len();
        let mut examined = 0_usize;
        for (index, summary) in segments.iter().enumerate() {
            let size = current.get(&(summary.first_seq, summary.last_seq)).copied();
            let mut unexamined = index < skip || index >= cut || size.is_none();
            if let (false, Some(size), Mode::Deep) = (unexamined, size, self.mode) {
                let remaining = self.budget.saturating_sub(self.bytes_read);
                if size > remaining {
                    if examined == 0 {
                        return Err(VerifyError::BudgetTooSmall {
                            needed: self.bytes_read.saturating_add(size),
                            budget: self.budget,
                        });
                    }
                    cut = index;
                    unexamined = true;
                }
            }
            if unexamined {
                if size.is_some() {
                    observed.push(*summary);
                }
                continue;
            }
            let key =
                Layout::segment(&tenant, generation, summary.first_seq, summary.last_seq)?.to_key();
            examined += 1;
            match self.mode {
                Mode::Light => {
                    if let Some(meta) = self.store.head(&key).await? {
                        observed.push(SegmentSummary {
                            segment_hash: parse_hash(&meta.etag),
                            ..*summary
                        });
                    }
                }
                Mode::Deep => {
                    if let Some(object) = self.store.get(&key).await? {
                        self.bytes_read = self.bytes_read.saturating_add(length(&object.bytes));
                        let (seen, kinds) = self.inspect(summary, &object.bytes);
                        observed.push(seen);
                        for kind in kinds {
                            findings.push(self.finding(
                                generation,
                                summary.first_seq,
                                summary.last_seq,
                                kind,
                            ));
                        }
                    }
                }
            }
        }

        // Stored segments of this generation the manifest does not list.
        let listed: BTreeSet<(u64, u64)> = segments
            .iter()
            .map(|summary| (summary.first_seq, summary.last_seq))
            .collect();
        for (&(first, last), &size) in &current {
            if listed.contains(&(first, last)) {
                continue;
            }
            let key = Layout::segment(&tenant, generation, first, last)?.to_key();
            let remaining = self.budget.saturating_sub(self.bytes_read);
            if self.mode == Mode::Deep
                && size <= remaining
                && let Some(object) = self.store.get(&key).await?
            {
                self.bytes_read = self.bytes_read.saturating_add(length(&object.bytes));
                if self.foreign(&object.bytes) {
                    findings.push(self.finding(
                        generation,
                        first,
                        last,
                        FindingKind::ForeignSegment,
                    ));
                }
                observed.push(implied(
                    generation,
                    first,
                    last,
                    segment_hash(&object.bytes),
                ));
            } else if let Some(meta) = self.store.head(&key).await? {
                observed.push(implied(generation, first, last, parse_hash(&meta.etag)));
            }
        }

        findings.extend(
            check_against(&verified, &observed)
                .iter()
                .map(|finding| from_manifest(&tenant, finding)),
        );
        findings.sort_unstable();
        findings.dedup();

        // The cursor covers the clean leading segments this pass reached.
        let mut clean = 0_usize;
        for summary in segments.iter().take(cut) {
            if findings
                .iter()
                .any(|f| f.touches(generation, summary.first_seq, summary.last_seq))
            {
                break;
            }
            clean += 1;
        }
        let prefix = segments.get(..clean).unwrap_or_default();
        Ok(VerifyReport {
            tenant,
            source: self.expected.source.clone(),
            generation,
            mode: self.mode,
            pinned: self.pinned,
            findings,
            segments_listed: segments.len(),
            segments_skipped: skip,
            segments_examined: examined,
            bytes_read: self.bytes_read,
            partial: cut < segments.len(),
            cursor: VerifyCursor::after(self.expected, generation, self.mode, prefix),
        })
    }

    /// Refuses a read the budget cannot cover when nothing has been examined yet.
    fn check_budget(&self, size: u64, examined: usize) -> Result<(), VerifyError> {
        if examined == 0 && size > self.budget.saturating_sub(self.bytes_read) {
            return Err(VerifyError::BudgetTooSmall {
                needed: self.bytes_read.saturating_add(size),
                budget: self.budget,
            });
        }
        Ok(())
    }

    /// Whether the bytes are a segment that names another source than the expected one.
    fn foreign(&self, bytes: &[u8]) -> bool {
        SegmentReader::open(bytes)
            .is_ok_and(|reader| reader.header().source_id != self.expected.source)
    }

    /// Opens one stored segment and compares it with the manifest's summary of it.
    ///
    /// Returns what storage holds at that position (for the manifest comparison) and the
    /// findings of the checks that only a read can make.
    fn inspect(
        &self,
        summary: &SegmentSummary,
        bytes: &[u8],
    ) -> (SegmentSummary, Vec<FindingKind>) {
        let mut seen = SegmentSummary {
            segment_hash: segment_hash(bytes),
            ..*summary
        };
        let mut kinds = Vec::new();
        match SegmentReader::open(bytes) {
            Err(_) => kinds.push(FindingKind::SegmentCorrupt),
            Ok(reader) => {
                let header = reader.header();
                seen.entry_count = header.entry_count;
                seen.previous_hash = header.prev_hash;
                if header.source_id != self.expected.source {
                    kinds.push(FindingKind::ForeignSegment);
                } else {
                    if header.generation != summary.generation
                        || header.first_seq != summary.first_seq
                        || header.last_seq != summary.last_seq
                        || header.entry_count != summary.entry_count
                    {
                        kinds.push(FindingKind::HeaderMismatch);
                    }
                    if reader.verify_all().is_err() {
                        kinds.push(FindingKind::SegmentCorrupt);
                    } else if header.prev_hash != summary.previous_hash {
                        kinds.push(FindingKind::ChainBroken);
                    }
                }
            }
        }
        (seen, kinds)
    }
}

/// A summary of a stored segment whose range the key gives and nothing else is known.
fn implied(generation: u64, first_seq: u64, last_seq: u64, hash: [u8; 32]) -> SegmentSummary {
    SegmentSummary::new(
        generation,
        first_seq,
        last_seq,
        last_seq.saturating_sub(first_seq).saturating_add(1),
        hash,
        [0; 32],
    )
}

fn length(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

/// The 32 bytes an etag (64 lower-case hex digits) stands for; all zero when it is not one, which
/// no real segment hashes to.
fn parse_hash(etag: &str) -> [u8; 32] {
    let mut out = [0_u8; 32];
    let digits: Vec<u8> = etag
        .bytes()
        .map(|byte| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        })
        .collect::<Option<Vec<u8>>>()
        .unwrap_or_default();
    if digits.len() != 64 {
        return out;
    }
    for (slot, pair) in out.iter_mut().zip(digits.chunks(2)) {
        if let [high, low] = pair {
            *slot = (high << 4) | low;
        }
    }
    out
}
