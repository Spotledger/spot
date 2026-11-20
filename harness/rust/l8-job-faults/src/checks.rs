//! The checks of the brief, (a) to (f), on the truth and what the stores hold.

use std::collections::{BTreeMap, BTreeSet};

use spot_jobs::model::JobState;

/// How an operation ended, after resolution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fate {
    /// The document row exists: the operation committed.
    Committed,
    /// The document row does not exist after recovery: it did not commit.
    Absent,
    /// The engine refused it cleanly.
    Refused,
}

/// One operation of the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TruthOp {
    /// The document id as text: the job's idempotency key.
    pub key: String,
    /// How it ended.
    pub fate: Fate,
}

/// The oracle of a run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Truth {
    /// Every operation.
    pub ops: Vec<TruthOp>,
}

/// What the job store says about one job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobView {
    /// The idempotency key.
    pub key: String,
    /// The state.
    pub state: JobState,
    /// Attempts made.
    pub attempt: u32,
    /// Attempts allowed.
    pub max_attempts: u32,
}

/// One failed check, naming the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Violation {
    /// `a` to `f`.
    pub check: char,
    /// The key (or `-`).
    pub key: String,
    /// What is wrong.
    pub detail: String,
}

fn v(check: char, key: &str, detail: impl Into<String>) -> Violation {
    Violation {
        check,
        key: key.to_owned(),
        detail: detail.into(),
    }
}

/// Runs (a) to (e). `effects` maps a key to the number of effect rows it has.
pub fn check_all(
    truth: &Truth,
    jobs: &[JobView],
    effects: &BTreeMap<String, u32>,
) -> Vec<Violation> {
    let mut out = Vec::new();
    let committed: BTreeSet<&str> = truth
        .ops
        .iter()
        .filter(|o| o.fate == Fate::Committed)
        .map(|o| o.key.as_str())
        .collect();
    let mut by_key: BTreeMap<&str, Vec<&JobView>> = BTreeMap::new();
    for j in jobs {
        by_key.entry(j.key.as_str()).or_default().push(j);
    }
    // (a) every committed operation's job succeeded and has exactly one effect row.
    for key in &committed {
        match by_key.get(key).map(Vec::as_slice) {
            None | Some([]) => out.push(v('a', key, "no job for a committed operation")),
            Some([job]) => {
                if job.state != JobState::Succeeded {
                    out.push(v(
                        'a',
                        key,
                        format!("job is {:?}, not Succeeded", job.state),
                    ));
                }
            }
            Some(_) => {}
        }
        match effects.get(*key).copied().unwrap_or(0) {
            1 => {}
            0 => out.push(v('a', key, "no effect row")),
            n => out.push(v('a', key, format!("{n} effect rows"))),
        }
    }
    // (b) nothing exists for a refused or uncommitted operation.
    for op in truth.ops.iter().filter(|o| o.fate != Fate::Committed) {
        if by_key.contains_key(op.key.as_str()) {
            out.push(v(
                'b',
                &op.key,
                "a job exists for an operation that did not commit",
            ));
        }
        if effects.get(&op.key).copied().unwrap_or(0) > 0 {
            out.push(v(
                'b',
                &op.key,
                "an effect exists for an operation that did not commit",
            ));
        }
    }
    for key in effects.keys().filter(|k| !committed.contains(k.as_str())) {
        if !truth.ops.iter().any(|o| &o.key == key) {
            out.push(v('b', key, "an effect for a key no operation ever used"));
        }
    }
    // (c) nothing is still pending, running or retrying.
    for j in jobs {
        if !matches!(
            j.state,
            JobState::Succeeded | JobState::Dead | JobState::Cancelled
        ) {
            out.push(v('c', &j.key, format!("job left {:?}", j.state)));
        }
    }
    // (d) attempts are within the allowed number.
    for j in jobs {
        if j.attempt > j.max_attempts {
            out.push(v(
                'd',
                &j.key,
                format!("{} attempts of {}", j.attempt, j.max_attempts),
            ));
        }
    }
    // (e) the intake did not double-insert: one job per distinct committed key.
    for (key, list) in &by_key {
        if list.len() > 1 {
            out.push(v('e', key, format!("{} jobs for one key", list.len())));
        }
    }
    if jobs.len() != committed.len() && by_key.values().all(|l| l.len() == 1) {
        out.push(v(
            'e',
            "-",
            format!("{} jobs for {} committed keys", jobs.len(), committed.len()),
        ));
    }
    out
}

/// Check (f), the committed-steps side (WP-TST-08.T8): a step is never recorded done without its
/// effect and never twice, and a committed operation's published outbox position is delivered
/// exactly once.
///
/// - `markers` maps a key to the number of done markers recorded for it (the committed-steps
///   marker store).
/// - `effects` is the number of effect rows per key, as in [`check_all`].
/// - `published` maps a key to how many times its outbox position was published to the intake.
///
/// A key with more than one marker, a marker with no effect row, a committed key published not
/// exactly once, or an uncommitted key published at all is a violation naming the key.
pub fn check_f(
    truth: &Truth,
    markers: &BTreeMap<String, u32>,
    effects: &BTreeMap<String, u32>,
    published: &BTreeMap<String, u32>,
) -> Vec<Violation> {
    let mut out = Vec::new();
    for (key, count) in markers {
        if *count > 1 {
            out.push(v('f', key, format!("{count} done markers for one step")));
        }
        if *count >= 1 && effects.get(key).copied().unwrap_or(0) == 0 {
            out.push(v('f', key, "a step recorded done without its effect"));
        }
    }
    let committed: BTreeSet<&str> = truth
        .ops
        .iter()
        .filter(|o| o.fate == Fate::Committed)
        .map(|o| o.key.as_str())
        .collect();
    for key in &committed {
        match published.get(*key).copied().unwrap_or(0) {
            1 => {}
            0 => out.push(v(
                'f',
                key,
                "a committed outbox position was never published",
            )),
            n => out.push(v('f', key, format!("published {n} times"))),
        }
    }
    for (key, n) in published {
        if *n > 0 && !committed.contains(key.as_str()) {
            out.push(v(
                'f',
                key,
                "published for an operation that did not commit",
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(key: &str, state: JobState) -> JobView {
        JobView {
            key: key.to_owned(),
            state,
            attempt: 1,
            max_attempts: 3,
        }
    }

    fn clean() -> (Truth, Vec<JobView>, BTreeMap<String, u32>) {
        let truth = Truth {
            ops: vec![
                TruthOp {
                    key: "k1".into(),
                    fate: Fate::Committed,
                },
                TruthOp {
                    key: "k2".into(),
                    fate: Fate::Committed,
                },
                TruthOp {
                    key: "k3".into(),
                    fate: Fate::Absent,
                },
                TruthOp {
                    key: "k4".into(),
                    fate: Fate::Refused,
                },
            ],
        };
        let jobs = vec![
            job("k1", JobState::Succeeded),
            job("k2", JobState::Succeeded),
        ];
        let effects = BTreeMap::from([("k1".to_owned(), 1), ("k2".to_owned(), 1)]);
        (truth, jobs, effects)
    }

    fn failed(t: &Truth, j: &[JobView], e: &BTreeMap<String, u32>) -> Vec<char> {
        let mut c: Vec<char> = check_all(t, j, e).iter().map(|x| x.check).collect();
        c.dedup();
        c
    }

    #[test]
    fn a_clean_truth_passes() {
        let (t, j, e) = clean();
        assert!(check_all(&t, &j, &e).is_empty());
    }

    #[test]
    fn a_missing_job_or_effect_fails_a() {
        let (t, mut j, e) = clean();
        j.pop();
        assert!(failed(&t, &j, &e).contains(&'a'));
        let (t, j, mut e) = clean();
        e.remove("k1");
        assert!(failed(&t, &j, &e).contains(&'a'));
    }

    #[test]
    fn a_duplicate_effect_fails_a() {
        let (t, j, mut e) = clean();
        e.insert("k1".into(), 2);
        assert!(failed(&t, &j, &e).contains(&'a'));
    }

    #[test]
    fn a_job_for_an_uncommitted_operation_fails_b() {
        let (t, mut j, e) = clean();
        j.push(job("k3", JobState::Succeeded));
        assert!(failed(&t, &j, &e).contains(&'b'));
        let (t, j, mut e) = clean();
        e.insert("k4".into(), 1);
        assert!(failed(&t, &j, &e).contains(&'b'));
    }

    #[test]
    fn a_job_left_running_fails_c() {
        let (t, mut j, e) = clean();
        j[0].state = JobState::Running;
        assert!(failed(&t, &j, &e).contains(&'c'));
    }

    #[test]
    fn too_many_attempts_fail_d() {
        let (t, mut j, e) = clean();
        j[0].attempt = 4;
        assert!(failed(&t, &j, &e).contains(&'d'));
    }

    #[test]
    fn a_double_insert_fails_e() {
        let (t, mut j, e) = clean();
        j.push(job("k1", JobState::Succeeded));
        assert!(failed(&t, &j, &e).contains(&'e'));
    }

    type Counts = BTreeMap<String, u32>;

    fn clean_f() -> (Truth, Counts, Counts, Counts) {
        let (truth, _, effects) = clean();
        let markers = BTreeMap::from([("k1".to_owned(), 1), ("k2".to_owned(), 1)]);
        let published = BTreeMap::from([("k1".to_owned(), 1), ("k2".to_owned(), 1)]);
        (truth, markers, effects, published)
    }

    #[test]
    fn a_clean_run_passes_f() {
        let (t, m, e, p) = clean_f();
        assert!(check_f(&t, &m, &e, &p).is_empty());
    }

    #[test]
    fn a_marker_without_its_effect_fails_f() {
        let (t, m, mut e, p) = clean_f();
        e.remove("k1");
        let found = check_f(&t, &m, &e, &p);
        assert!(found.iter().any(|x| x.check == 'f' && x.key == "k1"));
    }

    #[test]
    fn a_step_recorded_twice_fails_f() {
        let (t, mut m, e, p) = clean_f();
        m.insert("k2".into(), 2);
        assert!(!check_f(&t, &m, &e, &p).is_empty());
    }

    #[test]
    fn a_duplicated_publish_fails_f() {
        let (t, m, e, mut p) = clean_f();
        p.insert("k1".into(), 2);
        let found = check_f(&t, &m, &e, &p);
        assert!(
            found
                .iter()
                .any(|x| x.key == "k1" && x.detail.contains("2 times"))
        );
    }

    #[test]
    fn a_lost_publish_fails_f() {
        let (t, m, e, mut p) = clean_f();
        p.remove("k2");
        let found = check_f(&t, &m, &e, &p);
        assert!(
            found
                .iter()
                .any(|x| x.key == "k2" && x.detail.contains("never published"))
        );
    }

    #[test]
    fn a_publish_for_an_uncommitted_operation_fails_f() {
        let (t, m, e, mut p) = clean_f();
        p.insert("k3".into(), 1);
        assert!(!check_f(&t, &m, &e, &p).is_empty());
    }
}
