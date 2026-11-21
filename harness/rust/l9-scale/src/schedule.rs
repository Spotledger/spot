//! The open-loop arrival schedule.
//!
//! Arrivals are planned from the profile alone: operation `n` is due at a time fixed before
//! the run starts, whatever the server does. A slow server therefore cannot slow the load
//! down, and the runner measures every latency from the *intended* start (the correction for
//! coordinated omission).
//!
//! The arrival rate climbs linearly through the ramp and then holds. Each arrival goes to the
//! next session (in round-robin order) that has finished thinking; an arrival that finds every
//! session still thinking is *shed* and counted, so a profile with too few sessions for its
//! rate says so instead of silently under-delivering.

use crate::profile::{Mix, Op, Profile, Think};
use crate::rng::Rng;

/// One planned operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arrival {
    /// The 0-based position in the schedule.
    pub seq: u64,
    /// When it is due, in nanoseconds from the start of the run.
    pub at_ns: u64,
    /// The session that performs it.
    pub session: u32,
    /// What it does.
    pub op: Op,
    /// Whether it falls in the steady phase (the budgeted window) and not in the ramp.
    pub steady: bool,
}

/// The planner. It is an iterator that ends when the run's duration is over.
pub struct Scheduler {
    rng: Rng,
    mix: Mix,
    think: Think,
    target_milli_tps: u64,
    ramp_ns: u64,
    total_ns: u64,
    next_at: u64,
    seq: u64,
    cursor: usize,
    ready_at: Vec<u64>,
    shed: u64,
}

const NS_PER_SECOND_MILLI: u128 = 1_000_000_000_000;

impl Scheduler {
    /// A schedule for `profile`.
    pub fn new(profile: &Profile) -> Self {
        Self {
            rng: Rng::new(profile.seed),
            mix: profile.mix.clone(),
            think: profile.think.clone(),
            target_milli_tps: profile.target_milli_tps(),
            ramp_ns: profile.ramp_ns(),
            total_ns: profile.total_ns(),
            next_at: 0,
            seq: 0,
            cursor: 0,
            ready_at: vec![0; profile.sessions as usize],
            shed: 0,
        }
    }

    /// The arrival rate at `at_ns`, in thousandths of an operation per second: linear through
    /// the ramp (never below a tenth of the target, so the first gap is finite), then flat.
    pub fn rate_at(&self, at_ns: u64) -> u64 {
        rate_at(self.target_milli_tps, self.ramp_ns, at_ns)
    }

    /// How many arrivals found every session thinking and were dropped from the schedule.
    pub fn shed(&self) -> u64 {
        self.shed
    }

    fn pick_session(&mut self, at_ns: u64) -> Option<usize> {
        let n = self.ready_at.len();
        for step in 0..n {
            let index = (self.cursor + step) % n;
            if self
                .ready_at
                .get(index)
                .is_some_and(|ready| *ready <= at_ns)
            {
                return Some(index);
            }
        }
        None
    }
}

/// See [`Scheduler::rate_at`].
pub fn rate_at(target_milli_tps: u64, ramp_ns: u64, at_ns: u64) -> u64 {
    if ramp_ns == 0 || at_ns >= ramp_ns {
        return target_milli_tps;
    }
    let floor = ramp_ns / 10;
    let t = u128::from(at_ns.max(floor));
    let rate = u128::from(target_milli_tps) * t / u128::from(ramp_ns);
    u64::try_from(rate).unwrap_or(target_milli_tps).max(1)
}

impl Iterator for Scheduler {
    type Item = Arrival;

    fn next(&mut self) -> Option<Arrival> {
        loop {
            let at_ns = self.next_at;
            if at_ns >= self.total_ns {
                return None;
            }
            let rate = u128::from(self.rate_at(at_ns).max(1));
            let gap = u64::try_from(NS_PER_SECOND_MILLI / rate)
                .unwrap_or(u64::MAX)
                .max(1);
            self.next_at = at_ns.saturating_add(gap);
            // The operation and the pause are drawn for every slot, shed or not, so the
            // sequence of operations does not shift when the session count changes.
            let roll = self.rng.below(self.mix.total());
            let op = self.mix.pick(roll);
            let think = self.think.sample_ns(&mut self.rng);
            let Some(session) = self.pick_session(at_ns) else {
                self.shed += 1;
                continue;
            };
            if let Some(slot) = self.ready_at.get_mut(session) {
                *slot = at_ns.saturating_add(think);
            }
            self.cursor = (session + 1) % self.ready_at.len();
            let seq = self.seq;
            self.seq += 1;
            return Some(Arrival {
                seq,
                at_ns,
                session: u32::try_from(session).unwrap_or(u32::MAX),
                op,
                steady: at_ns >= self.ramp_ns,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn profile(sessions: u32, ramp: u64, steady: u64, tps: &str, think_ms: u64) -> Profile {
        Profile::from_toml(&format!(
            "name = \"t\"\ntier = \"x\"\nsessions = {sessions}\nramp_seconds = {ramp}\n\
             steady_seconds = {steady}\ntarget_tps = \"{tps}\"\n\
             [think]\nkind = \"fixed\"\nms = {think_ms}\n[mix]\nopen_form = 3\nlist = 1\n"
        ))
        .unwrap()
    }

    #[test]
    fn the_steady_rate_is_exact_and_arrivals_are_evenly_spaced() {
        let p = profile(50, 0, 10, "20", 0);
        let arrivals: Vec<Arrival> = Scheduler::new(&p).collect();
        assert_eq!(arrivals.len(), 200);
        assert!(arrivals.iter().all(|a| a.steady));
        assert!(
            arrivals
                .windows(2)
                .all(|w| w[1].at_ns - w[0].at_ns == 50_000_000)
        );
        assert_eq!(arrivals[0].at_ns, 0);
        assert!(arrivals.iter().enumerate().all(|(i, a)| a.seq == i as u64));
    }

    #[test]
    fn the_same_seed_gives_the_same_schedule_and_another_seed_a_different_one() {
        let p = profile(10, 1, 4, "30", 100);
        let a: Vec<Arrival> = Scheduler::new(&p).collect();
        let b: Vec<Arrival> = Scheduler::new(&p).collect();
        assert_eq!(a, b);
        let mut other = p.clone();
        other.seed = 99;
        let c: Vec<Arrival> = Scheduler::new(&other).collect();
        assert_ne!(a, c);
    }

    #[test]
    fn the_ramp_climbs_and_only_the_steady_phase_is_marked_steady() {
        let p = profile(100, 4, 4, "100", 0);
        let arrivals: Vec<Arrival> = Scheduler::new(&p).collect();
        let in_first_ramp_second = arrivals.iter().filter(|a| a.at_ns < 1_000_000_000).count();
        let in_last_ramp_second = arrivals
            .iter()
            .filter(|a| (3_000_000_000..4_000_000_000).contains(&a.at_ns))
            .count();
        assert!(in_last_ramp_second > in_first_ramp_second * 2);
        assert!(
            arrivals
                .iter()
                .all(|a| a.steady == (a.at_ns >= 4_000_000_000))
        );
        let steady = arrivals.iter().filter(|a| a.steady).count();
        assert!((399..=400).contains(&steady), "{steady}");
    }

    #[test]
    fn the_mix_is_followed_over_a_long_schedule() {
        let p = profile(1000, 0, 100, "100", 0);
        let arrivals: Vec<Arrival> = Scheduler::new(&p).collect();
        let lists = arrivals.iter().filter(|a| a.op == Op::List).count();
        let forms = arrivals.iter().filter(|a| a.op == Op::OpenForm).count();
        assert_eq!(lists + forms, arrivals.len());
        assert!(forms > lists * 2 && forms < lists * 4, "{forms} vs {lists}");
    }

    #[test]
    fn sessions_take_turns_and_think_before_their_next_turn() {
        let p = profile(4, 0, 2, "10", 1_000);
        let mut scheduler = Scheduler::new(&p);
        let first: Vec<u32> = (0..4)
            .filter_map(|_| scheduler.next())
            .map(|a| a.session)
            .collect();
        assert_eq!(first, [0, 1, 2, 3]);
        // 4 sessions thinking 1 s at 10/s: the next 6 arrivals of the first second find
        // everybody thinking and are shed, and the second second is served again.
        let rest: Vec<Arrival> = scheduler.by_ref().collect();
        assert!(rest.iter().all(|a| a.at_ns >= 1_000_000_000));
        assert_eq!(scheduler.shed(), 6 + 6);
    }

    #[test]
    fn a_session_is_never_given_work_while_it_is_still_thinking() {
        let p = profile(3, 0, 5, "20", 400);
        let mut last: [Option<u64>; 3] = [None; 3];
        for a in Scheduler::new(&p) {
            if let Some(prev) = last[a.session as usize] {
                assert!(a.at_ns - prev >= 400_000_000);
            }
            last[a.session as usize] = Some(a.at_ns);
        }
    }

    #[test]
    fn the_rate_function_is_linear_with_a_floor() {
        assert_eq!(rate_at(10_000, 0, 5), 10_000);
        assert_eq!(rate_at(10_000, 1_000, 1_000), 10_000);
        assert_eq!(rate_at(10_000, 1_000, 500), 5_000);
        assert_eq!(rate_at(10_000, 1_000, 0), 1_000);
        assert_eq!(rate_at(1, 1_000_000, 0), 1);
    }
}
