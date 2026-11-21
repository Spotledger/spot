//! The comparison itself: one streaming merge of two sorted summary streams (ADR-061 s8).
//!
//! [`compare`] holds one record per side, the sort key of the record it saw last on each side,
//! the per-entity counters and the last difference it recorded. It never collects a side, so a
//! tenant's million records cost the same resident memory as its ten.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::error::AuditError;
use crate::result::{AuditOutcome, AuditResult, DifferenceKind, EntityCount, FirstDifference};
use crate::summary::{DerivedEntities, RecordSummary};

/// Which side a stream came from, so an error names it.
#[derive(Clone, Copy)]
enum Side {
    /// The live database.
    Live,
    /// The scratch rebuild.
    Rebuilt,
}

impl Side {
    /// The side's name, as the error message spells it.
    fn name(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Rebuilt => "rebuilt",
        }
    }
}

/// The differences of one entity, counted.
#[derive(Clone, Copy, Default)]
struct Differences {
    /// Records only the live database holds.
    missing: u64,
    /// Records only the rebuild holds.
    extra: u64,
    /// Records both hold at different versions.
    version: u64,
    /// Records both hold at the same version with a different digest.
    content: u64,
}

/// One side's stream, with the checks and the counting every side needs.
struct Walker<'a, I> {
    /// The side's records, in sort order.
    records: I,
    /// Which side this is.
    side: Side,
    /// The entities the catalog flags derived.
    derived: &'a DerivedEntities,
    /// The key of the last record accepted, to prove the stream is sorted as promised.
    previous: Option<(String, String)>,
    /// How many records this side holds per entity, derived entities excluded.
    counts: BTreeMap<String, u64>,
    /// The derived entities this side actually held, and which were therefore skipped.
    skipped: BTreeSet<String>,
}

impl<'a, I> Walker<'a, I>
where
    I: Iterator<Item = RecordSummary>,
{
    /// A walker over one side.
    fn new(records: I, side: Side, derived: &'a DerivedEntities) -> Self {
        Self {
            records,
            side,
            derived,
            previous: None,
            counts: BTreeMap::new(),
            skipped: BTreeSet::new(),
        }
    }

    /// The next record to compare: derived entities are skipped and remembered, and a stream that
    /// is not strictly ascending by entity then id is refused instead of producing nonsense.
    fn next_record(&mut self) -> Result<Option<RecordSummary>, AuditError> {
        loop {
            let Some(record) = self.records.next() else {
                return Ok(None);
            };
            if self.derived.contains(&record.entity) {
                self.skipped.insert(record.entity);
                continue;
            }
            let ascending = match &self.previous {
                None => true,
                Some((entity, id)) => {
                    record.entity.as_str() > entity.as_str()
                        || (record.entity.as_str() == entity.as_str()
                            && record.id.as_str() > id.as_str())
                }
            };
            if !ascending {
                return Err(AuditError::StreamNotSorted {
                    side: self.side.name(),
                    entity: record.entity.clone(),
                    id: record.id.clone(),
                });
            }
            *self.counts.entry(record.entity.clone()).or_insert(0) += 1;
            self.previous = Some((record.entity.clone(), record.id.clone()));
            return Ok(Some(record));
        }
    }
}

/// Compares the live database with the scratch rebuild, both streams sorted by entity and then id
/// (see [`RecordSummary`]), skipping the entities the catalog flags derived.
///
/// The comparison is one pass over each side in constant memory: it holds at most one record per
/// side and the sort key of the previous record, and reports the **first differing record in sort
/// order**, which ADR-061 s8 requires.
///
/// # Errors
///
/// [`AuditError::StreamNotSorted`] when a side is not strictly ascending by entity then id. A
/// merge over unsorted input would report one record as both missing and extra, so it is refused
/// rather than reported.
pub fn compare<L, R>(
    live: L,
    rebuilt: R,
    derived: &DerivedEntities,
) -> Result<AuditResult, AuditError>
where
    L: IntoIterator<Item = RecordSummary>,
    R: IntoIterator<Item = RecordSummary>,
{
    let mut live = Walker::new(live.into_iter(), Side::Live, derived);
    let mut rebuilt = Walker::new(rebuilt.into_iter(), Side::Rebuilt, derived);
    let mut differences: BTreeMap<String, Differences> = BTreeMap::new();
    let mut first: Option<FirstDifference> = None;

    let mut left = live.next_record()?;
    let mut right = rebuilt.next_record()?;
    loop {
        match (left.take(), right.take()) {
            (None, None) => break,
            (Some(record), None) => {
                count(&mut differences, &record.entity).missing += 1;
                first.get_or_insert_with(|| FirstDifference::missing(&record));
                left = live.next_record()?;
            }
            (None, Some(record)) => {
                count(&mut differences, &record.entity).extra += 1;
                first.get_or_insert_with(|| FirstDifference::extra(&record));
                right = rebuilt.next_record()?;
            }
            (Some(l), Some(r)) => match l.sort_key().cmp(&r.sort_key()) {
                Ordering::Less => {
                    count(&mut differences, &l.entity).missing += 1;
                    first.get_or_insert_with(|| FirstDifference::missing(&l));
                    right = Some(r);
                    left = live.next_record()?;
                }
                Ordering::Greater => {
                    count(&mut differences, &r.entity).extra += 1;
                    first.get_or_insert_with(|| FirstDifference::extra(&r));
                    left = Some(l);
                    right = rebuilt.next_record()?;
                }
                Ordering::Equal => {
                    if l.version != r.version {
                        count(&mut differences, &l.entity).version += 1;
                        first.get_or_insert_with(|| {
                            FirstDifference::compared(&l, &r, DifferenceKind::VersionDiffers)
                        });
                    } else if l.content_digest != r.content_digest {
                        count(&mut differences, &l.entity).content += 1;
                        first.get_or_insert_with(|| {
                            FirstDifference::compared(&l, &r, DifferenceKind::ContentDiffers)
                        });
                    }
                    left = live.next_record()?;
                    right = rebuilt.next_record()?;
                }
            },
        }
    }

    let (live_counts, live_skipped) = (live.counts, live.skipped);
    let (rebuilt_counts, rebuilt_skipped) = (rebuilt.counts, rebuilt.skipped);

    let mut names: BTreeSet<&str> = BTreeSet::new();
    names.extend(live_counts.keys().map(String::as_str));
    names.extend(rebuilt_counts.keys().map(String::as_str));
    names.extend(differences.keys().map(String::as_str));

    let mut per_entity: BTreeMap<String, EntityCount> = BTreeMap::new();
    let mut totals = EntityCount::default();
    for name in names {
        let counted = differences.get(name).copied().unwrap_or_default();
        let count = EntityCount {
            live: live_counts.get(name).copied().unwrap_or(0),
            rebuilt: rebuilt_counts.get(name).copied().unwrap_or(0),
            missing_in_rebuild: counted.missing,
            extra_in_rebuild: counted.extra,
            version_differs: counted.version,
            content_differs: counted.content,
        };
        totals.add(&count);
        per_entity.insert(name.to_owned(), count);
    }

    let mut derived_skipped: Vec<String> = live_skipped.union(&rebuilt_skipped).cloned().collect();
    derived_skipped.sort();

    let outcome = match first {
        Some(first_difference) => AuditOutcome::Mismatch { first_difference },
        None => AuditOutcome::Match,
    };
    Ok(AuditResult {
        outcome,
        per_entity,
        totals,
        derived_skipped,
    })
}

/// Compares every entity, with nothing flagged derived.
///
/// # Errors
///
/// As [`compare`].
pub fn compare_all<L, R>(live: L, rebuilt: R) -> Result<AuditResult, AuditError>
where
    L: IntoIterator<Item = RecordSummary>,
    R: IntoIterator<Item = RecordSummary>,
{
    compare(live, rebuilt, &DerivedEntities::none())
}

/// The difference counts of one entity.
fn count<'a>(
    differences: &'a mut BTreeMap<String, Differences>,
    entity: &str,
) -> &'a mut Differences {
    differences.entry(entity.to_owned()).or_default()
}

impl FirstDifference {
    /// A record only the live database holds.
    fn missing(record: &RecordSummary) -> Self {
        Self {
            entity: record.entity.clone(),
            id: record.id.clone(),
            kind: DifferenceKind::MissingInRebuild,
            live_version: Some(record.version),
            rebuilt_version: None,
        }
    }

    /// A record only the rebuild holds.
    fn extra(record: &RecordSummary) -> Self {
        Self {
            entity: record.entity.clone(),
            id: record.id.clone(),
            kind: DifferenceKind::ExtraInRebuild,
            live_version: None,
            rebuilt_version: Some(record.version),
        }
    }

    /// A record both sides hold, at different versions or with different content.
    fn compared(live: &RecordSummary, rebuilt: &RecordSummary, kind: DifferenceKind) -> Self {
        Self {
            entity: live.entity.clone(),
            id: live.id.clone(),
            kind,
            live_version: Some(live.version),
            rebuilt_version: Some(rebuilt.version),
        }
    }
}
