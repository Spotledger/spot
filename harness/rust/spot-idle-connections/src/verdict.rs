//! Judging a run against the budgets. Pure: numbers in, checks out, so the comparison itself is
//! tested without a server.

use crate::config::Budgets;
use crate::report::Check;

/// Scheduling tolerance on the shutdown grace, milliseconds. The accept loop aborts what is left
/// at exactly the grace, so a close a few scheduling ticks later is still the grace being kept.
pub const GRACE_TOLERANCE_MS: u64 = 500;

/// Everything the verdict needs from a run.
#[derive(Debug, Clone)]
pub struct Inputs {
    /// Idle HTTP/1.1 and HTTP/2 connections requested.
    pub http_count: usize,
    /// Idle WebSocket connections requested.
    pub ws_count: usize,
    /// Connections established.
    pub established: usize,
    /// Failed client connects or handshakes.
    pub connect_errors: u64,
    /// HTTP connections the server counted open once all were established.
    pub server_open: usize,
    /// WebSocket sessions the realtime bus counted.
    pub server_ws: usize,
    /// Accept errors the server logged.
    pub accept_errors: u64,
    /// Resident growth over baseline, when it could be read.
    pub growth_bytes: Option<u64>,
    /// Empty-server busy p99, microseconds.
    pub empty_p99_us: u64,
    /// Loaded busy p99, microseconds.
    pub loaded_p99_us: u64,
    /// Failed busy requests.
    pub busy_errors: u64,
    /// Milliseconds until the server counted no open connection (`u64::MAX` if it never did).
    pub server_closed_ms: u64,
    /// The shutdown grace, milliseconds.
    pub grace_ms: u64,
    /// Idle client sockets still open at the end.
    pub clients_still_open: usize,
    /// Tokio tasks alive before the server started.
    pub tasks_before: usize,
    /// Tokio tasks alive after shutdown.
    pub tasks_after: usize,
}

/// The memory the mix may grow by: each kind at its own ceiling.
#[must_use]
pub fn memory_budget_bytes(http_count: usize, ws_count: usize, budgets: &Budgets) -> u64 {
    let http = u64::try_from(http_count).unwrap_or(u64::MAX);
    let ws = u64::try_from(ws_count).unwrap_or(u64::MAX);
    http.saturating_mul(budgets.memory.http_idle_kib)
        .saturating_add(ws.saturating_mul(budgets.memory.ws_idle_kib))
        .saturating_mul(1024)
}

/// The busy p99 may be at most the empty one times the factor plus the extra, microseconds.
#[must_use]
pub fn p99_ceiling_us(empty_p99_us: u64, budgets: &Budgets) -> u64 {
    empty_p99_us
        .saturating_mul(budgets.latency.p99_factor)
        .saturating_add(budgets.latency.p99_extra_ms.saturating_mul(1000))
}

/// Judges a run: one [`Check`] per budget, in a fixed order.
#[must_use]
pub fn evaluate(i: &Inputs, budgets: &Budgets) -> Vec<Check> {
    let wanted = i.http_count + i.ws_count;
    let mut checks = Vec::with_capacity(7);
    checks.push(Check {
        name: "connections",
        ok: i.established == wanted
            && i.connect_errors <= budgets.faults.connect_errors
            && i.server_open >= i.http_count
            && i.server_ws >= i.ws_count,
        detail: format!(
            "{} of {wanted} established, {} connect errors (ceiling {}), server counted {} HTTP \
             connections (need {}) and {} WebSocket sessions (need {})",
            i.established,
            i.connect_errors,
            budgets.faults.connect_errors,
            i.server_open,
            i.http_count,
            i.server_ws,
            i.ws_count
        ),
    });
    checks.push(Check {
        name: "accept_errors",
        ok: i.accept_errors <= budgets.faults.accept_errors,
        detail: format!(
            "{} accept errors (ceiling {})",
            i.accept_errors, budgets.faults.accept_errors
        ),
    });
    let budget = memory_budget_bytes(i.http_count, i.ws_count, budgets);
    checks.push(match i.growth_bytes {
        Some(growth) => Check {
            name: "memory",
            ok: growth <= budget,
            detail: format!("grew {growth} bytes, budget {budget} bytes"),
        },
        None => Check {
            name: "memory",
            ok: true,
            detail: "resident memory cannot be read on this system; not judged".to_owned(),
        },
    });
    let ceiling = p99_ceiling_us(i.empty_p99_us, budgets);
    checks.push(Check {
        name: "latency",
        ok: i.loaded_p99_us <= ceiling,
        detail: format!(
            "loaded p99 {} us, empty p99 {} us, ceiling {ceiling} us",
            i.loaded_p99_us, i.empty_p99_us
        ),
    });
    checks.push(Check {
        name: "busy_errors",
        ok: i.busy_errors <= budgets.faults.busy_errors,
        detail: format!(
            "{} failed busy requests (ceiling {})",
            i.busy_errors, budgets.faults.busy_errors
        ),
    });
    checks.push(Check {
        name: "shutdown",
        ok: i.server_closed_ms <= i.grace_ms.saturating_add(GRACE_TOLERANCE_MS)
            && i.clients_still_open == 0,
        detail: format!(
            "server closed everything in {} ms (grace {} ms, tolerance {GRACE_TOLERANCE_MS} ms), \
             {} client sockets still open",
            i.server_closed_ms, i.grace_ms, i.clients_still_open
        ),
    });
    let leaked = i.tasks_after.saturating_sub(i.tasks_before);
    checks.push(Check {
        name: "tasks",
        ok: u64::try_from(leaked).unwrap_or(u64::MAX) <= budgets.faults.leaked_tasks,
        detail: format!(
            "{} tasks before, {} after: {leaked} leaked (ceiling {})",
            i.tasks_before, i.tasks_after, budgets.faults.leaked_tasks
        ),
    });
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    fn budgets() -> Budgets {
        Budgets::shipped().unwrap_or_else(|e| panic!("{e}"))
    }

    fn good() -> Inputs {
        Inputs {
            http_count: 150,
            ws_count: 50,
            established: 200,
            connect_errors: 0,
            server_open: 150,
            server_ws: 50,
            accept_errors: 0,
            growth_bytes: Some(10 * 1024 * 1024),
            empty_p99_us: 2_000,
            loaded_p99_us: 3_000,
            busy_errors: 0,
            server_closed_ms: 120,
            grace_ms: 10_000,
            clients_still_open: 0,
            tasks_before: 3,
            tasks_after: 3,
        }
    }

    fn failing(inputs: &Inputs) -> Vec<&'static str> {
        evaluate(inputs, &budgets())
            .into_iter()
            .filter(|c| !c.ok)
            .map(|c| c.name)
            .collect()
    }

    #[test]
    fn a_good_run_passes_every_check_in_order() {
        let checks = evaluate(&good(), &budgets());
        let names: Vec<_> = checks.iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            [
                "connections",
                "accept_errors",
                "memory",
                "latency",
                "busy_errors",
                "shutdown",
                "tasks"
            ]
        );
        assert!(checks.iter().all(|c| c.ok), "{checks:?}");
    }

    #[test]
    fn memory_is_judged_per_kind_at_its_own_ceiling() {
        // 150 HTTP at 100 KiB + 50 WebSockets at 200 KiB = 25,000 KiB.
        assert_eq!(memory_budget_bytes(150, 50, &budgets()), 25_000 * 1024);
        let mut run = good();
        run.growth_bytes = Some(25_000 * 1024);
        assert!(failing(&run).is_empty());
        run.growth_bytes = Some(25_000 * 1024 + 1);
        assert_eq!(failing(&run), ["memory"]);
        run.growth_bytes = None;
        assert!(
            failing(&run).is_empty(),
            "an unreadable number is reported, not failed"
        );
    }

    #[test]
    fn latency_ceiling_is_twice_the_empty_p99_plus_twenty_milliseconds() {
        assert_eq!(p99_ceiling_us(2_000, &budgets()), 24_000);
        let mut run = good();
        run.loaded_p99_us = 24_000;
        assert!(failing(&run).is_empty());
        run.loaded_p99_us = 24_001;
        assert_eq!(failing(&run), ["latency"]);
    }

    #[test]
    fn every_fault_budget_is_a_hard_zero_by_default() {
        let mut run = good();
        run.accept_errors = 1;
        assert_eq!(failing(&run), ["accept_errors"]);
        let mut run = good();
        run.busy_errors = 1;
        assert_eq!(failing(&run), ["busy_errors"]);
        let mut run = good();
        run.tasks_after = 4;
        assert_eq!(failing(&run), ["tasks"]);
        let mut run = good();
        run.established = 199;
        run.connect_errors = 1;
        assert_eq!(failing(&run), ["connections"]);
        let mut run = good();
        run.server_open = 149;
        assert_eq!(failing(&run), ["connections"]);
        let mut run = good();
        run.server_ws = 49;
        assert_eq!(failing(&run), ["connections"]);
    }

    #[test]
    fn shutdown_must_beat_the_grace_and_leave_no_client_open() {
        let mut run = good();
        run.server_closed_ms = 10_500;
        assert!(failing(&run).is_empty(), "within the tolerance");
        run.server_closed_ms = 10_501;
        assert_eq!(failing(&run), ["shutdown"]);
        let mut run = good();
        run.clients_still_open = 1;
        assert_eq!(failing(&run), ["shutdown"]);
        let mut run = good();
        run.server_closed_ms = u64::MAX;
        assert_eq!(failing(&run), ["shutdown"]);
    }
}
