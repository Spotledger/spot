//! The statements the virtual users and the agents run, and the deterministic plan that picks
//! them. The fake agent provider is this plan: it never calls a model, it only decides what
//! the next tool call costs.

use tokio_postgres::Client;

/// One kind of call, ordered by cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A primary-key lookup.
    Lookup,
    /// A filtered, ordered, limited list.
    Filter,
    /// A report-shaped aggregate over half the table.
    Aggregate,
    /// A deliberately expensive full scan that hashes every row.
    Scan,
    /// The same scan, but the caller gives up after a few milliseconds (`Admission::cancel`).
    AbandonedScan,
    /// A statement that runs longer than any deadline (`pg_sleep`): it must be cancelled.
    Overdue,
}

impl Kind {
    /// The name used in the report.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Lookup => "read",
            Self::Filter => "list",
            Self::Aggregate => "report",
            Self::Scan => "scan",
            Self::AbandonedScan => "abandoned_scan",
            Self::Overdue => "overdue",
        }
    }
}

/// A small deterministic generator (xorshift), so a run's call plan is reproducible.
#[derive(Clone, Copy, Debug)]
pub struct Rng(u64);

impl Rng {
    /// A generator from `seed` (never stuck at zero).
    #[must_use]
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    /// The next value.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// The next value below `bound` (`bound` of 0 gives 0).
    pub fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            0
        } else {
            self.next_u64() % bound
        }
    }
}

/// The kind of the `n`-th call of agent `index`. Every `overdue_every`-th call exceeds its
/// deadline; the rest cycle through a fixed mix, shifted per agent so the agents do not move in
/// step.
#[must_use]
pub fn plan(index: u32, n: u64, overdue_every: u32) -> Kind {
    let slot = n + u64::from(index) * 3;
    let every = u64::from(overdue_every.max(2));
    if slot % every == every - 1 {
        return Kind::Overdue;
    }
    if slot % 25 == 12 {
        return Kind::AbandonedScan;
    }
    match slot % 10 {
        7 => Kind::Scan,
        3 => Kind::Aggregate,
        1 | 5 | 9 => Kind::Filter,
        _ => Kind::Lookup,
    }
}

/// Runs one call and returns how many rows it produced. The caller owns the deadline: it drops
/// or cancels this future, it is not enforced here.
///
/// `table` is the schema-qualified table the harness created (never user text); `rows` is the
/// table's size; `draw` is a fresh random value for the call's parameters; `sleep_secs` is how
/// long an [`Kind::Overdue`] statement sleeps.
///
/// # Errors
///
/// The driver's error, including SQLSTATE 57014 when the statement was cancelled.
pub async fn execute(
    client: &Client,
    kind: Kind,
    table: &str,
    rows: u64,
    draw: u64,
    sleep_secs: u64,
) -> Result<usize, tokio_postgres::Error> {
    let rows_i = i64::try_from(rows.max(1)).unwrap_or(i64::MAX);
    let id = 1 + i64::try_from(draw % rows.max(1)).unwrap_or(0);
    let found = match kind {
        Kind::Lookup => client
            .query(
                &format!(
                    "SELECT id, tenant, status, amount_cents, payload FROM {table} WHERE id = $1"
                ),
                &[&id],
            )
            .await?,
        Kind::Filter => {
            let status = ["draft", "open", "paid", "void", "late"][(draw % 5) as usize];
            let from = rows_i - rows_i / 10;
            client
                .query(
                    &format!(
                        "SELECT id, amount_cents FROM {table} WHERE status = $1 AND created_at >= $2 \
                         ORDER BY created_at LIMIT 100"
                    ),
                    &[&status, &from],
                )
                .await?
        }
        Kind::Aggregate => {
            let from = rows_i / 2;
            client
                .query(
                    &format!(
                        "SELECT status, count(*), sum(amount_cents) FROM {table} \
                         WHERE created_at >= $1 GROUP BY status"
                    ),
                    &[&from],
                )
                .await?
        }
        Kind::Scan | Kind::AbandonedScan => {
            let salt = id.to_string();
            client
                .query(
                    &format!(
                        "SELECT count(*), sum(length(md5(payload || id::text || $1::text))) FROM {table}"
                    ),
                    &[&salt],
                )
                .await?
        }
        // The seconds are a number the harness chose, never text from outside.
        Kind::Overdue => {
            client
                .query(&format!("SELECT pg_sleep({sleep_secs})"), &[])
                .await?
        }
    };
    Ok(found.len())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_is_deterministic_and_covers_every_kind() {
        let mut seen = std::collections::BTreeSet::new();
        for index in 0..5 {
            for n in 0..200 {
                assert_eq!(plan(index, n, 8), plan(index, n, 8));
                seen.insert(plan(index, n, 8).name());
            }
        }
        for kind in [
            Kind::Lookup,
            Kind::Filter,
            Kind::Aggregate,
            Kind::Scan,
            Kind::AbandonedScan,
            Kind::Overdue,
        ] {
            assert!(seen.contains(kind.name()), "{} never planned", kind.name());
        }
    }

    #[test]
    fn an_overdue_call_comes_exactly_once_per_cycle() {
        let overdue = (0..50).filter(|n| plan(0, *n, 50) == Kind::Overdue).count();
        assert_eq!(overdue, 1);
    }

    #[test]
    fn the_generator_never_sticks_and_is_reproducible() {
        let mut a = Rng::new(0);
        let mut b = Rng::new(0);
        let first = a.next_u64();
        assert_ne!(first, 0);
        assert_eq!(first, b.next_u64());
        assert_eq!(Rng::new(5).below(0), 0);
    }
}
