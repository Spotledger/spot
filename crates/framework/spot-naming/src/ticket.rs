//! Gapless allocation: the gap policy, the ticket, and the reserved block.

use spot_storage_contract::SeriesKey;

use crate::error::NamingError;
use crate::name::RecordName;
use crate::strategy::SeriesFormat;

/// What a caller may do around a series (ADR-059 "Numbering").
///
/// On this allocator every series is gapless under rollback, whatever the policy; the policy
/// decides what callers may deliberately *skip*, which is what the three gap policies of
/// ADR-059 differ in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GapPolicy {
    /// Allocate inside the business transaction; a rollback returns the number. Skipping and
    /// reserving are allowed.
    Tolerant,
    /// Like [`GapPolicy::Tolerant`], but [`crate::Namer::skip`] and [`crate::Namer::reserve`]
    /// are refused, and the name is committed through a [`NamingTicket`] so the allocation is
    /// the caller's last act before commit.
    GaplessStrict,
    /// Allocate a block of this many numbers for a bulk operation ([`crate::Namer::reserve`]),
    /// handed out by a [`Block`].
    Reserved(u32),
}

impl GapPolicy {
    /// Whether the policy refuses `skip` and `reserve`.
    pub fn is_strict(self) -> bool {
        matches!(self, Self::GaplessStrict)
    }
}

/// A reserved name whose allocation is committed by consuming the ticket.
///
/// A series name allocates its number when the ticket is built, inside the caller's
/// transaction. For [`GapPolicy::GaplessStrict`] the caller holds the ticket until just before
/// commit and then calls [`NamingTicket::commit_name`], which consumes it: the type system
/// makes "allocate, then issue last" a single, once-only step.
#[derive(Clone, Debug)]
pub struct NamingTicket {
    name: RecordName,
    number: Option<u64>,
    series: Option<SeriesKey>,
    policy: GapPolicy,
}

impl NamingTicket {
    /// Assembles a ticket (used by the namer).
    pub(crate) fn new(
        name: RecordName,
        number: Option<u64>,
        series: Option<SeriesKey>,
        policy: GapPolicy,
    ) -> Self {
        Self {
            name,
            number,
            series,
            policy,
        }
    }

    /// The name to write into the document.
    pub fn name(&self) -> &RecordName {
        &self.name
    }

    /// The allocated number, when the strategy took one from a series.
    pub fn number(&self) -> Option<u64> {
        self.number
    }

    /// The series the number came from, when the strategy took one.
    pub fn series(&self) -> Option<&SeriesKey> {
        self.series.as_ref()
    }

    /// The policy the ticket was built under.
    pub fn policy(&self) -> GapPolicy {
        self.policy
    }

    /// Commits the name, consuming the ticket. Call it as the last act before commit for a
    /// [`GapPolicy::GaplessStrict`] series.
    pub fn commit_name(self) -> RecordName {
        self.name
    }
}

/// A block of consecutive numbers reserved for a bulk operation.
///
/// The iterator yields the block's numbers and stops at its end; [`Block::next_number`] fails
/// with [`NamingError::BlockExhausted`] when a caller asks for one past the end, so a bulk
/// operation that consumes more numbers than it reserved is told, never silently given numbers
/// from the next block.
#[derive(Clone)]
pub struct Block {
    series: SeriesKey,
    format: SeriesFormat,
    first: u64,
    count: u32,
    consumed: u32,
}

impl Block {
    /// Assembles a block (used by the namer).
    pub(crate) fn new(series: SeriesKey, format: SeriesFormat, first: u64, count: u32) -> Self {
        Self {
            series,
            format,
            first,
            count,
            consumed: 0,
        }
    }

    /// The series the block came from.
    pub fn series(&self) -> &SeriesKey {
        &self.series
    }

    /// How many numbers the block holds in total.
    pub fn total(&self) -> u32 {
        self.count
    }

    /// How many numbers remain.
    pub fn remaining(&self) -> u32 {
        self.count.saturating_sub(self.consumed)
    }

    /// The next number, or [`NamingError::BlockExhausted`] once the block is spent.
    ///
    /// # Errors
    ///
    /// [`NamingError::BlockExhausted`] when every allocated number was consumed.
    pub fn next_number(&mut self) -> Result<u64, NamingError> {
        if self.consumed >= self.count {
            return Err(NamingError::BlockExhausted { count: self.count });
        }
        let number = self.number_at(self.consumed);
        self.consumed += 1;
        Ok(number)
    }

    /// The next number's name, or [`NamingError::BlockExhausted`] once the block is spent.
    ///
    /// # Errors
    ///
    /// [`NamingError::BlockExhausted`] when every allocated number was consumed, or
    /// [`NamingError::PatternInvalid`] if the bound plan cannot render.
    pub fn next_name(&mut self) -> Result<RecordName, NamingError> {
        let number = self.next_number()?;
        RecordName::new(&self.format.render(Some(number))?)
    }

    fn number_at(&self, offset: u32) -> u64 {
        self.first + u64::from(offset)
    }
}

impl Iterator for Block {
    type Item = u64;

    fn next(&mut self) -> Option<u64> {
        if self.consumed >= self.count {
            return None;
        }
        let number = self.number_at(self.consumed);
        self.consumed += 1;
        Some(number)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining() as usize;
        (remaining, Some(remaining))
    }
}

impl std::fmt::Debug for Block {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Block")
            .field("series", &self.series)
            .field("total", &self.count)
            .field("consumed", &self.consumed)
            .finish()
    }
}
