//! One run: baseline, empty-server load, open the idle connections, load again while they are
//! held, shut down, judge.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::client::{self, Held};
use crate::config::{Budgets, RunConfig};
use crate::error::HarnessError;
use crate::report::{Accept, Descriptors, Latency, Memory, Outcome, Report, Setup, Shutdown};
use crate::server::{self, LIVE_PATH, PING_PATH};
use crate::stats::percentiles;
use crate::verdict::{self, GRACE_TOLERANCE_MS, Inputs};
use crate::{events, measure};

/// Which kind of idle connection a connect task opens.
#[derive(Clone, Copy)]
enum Kind {
    Http1,
    Http2,
    WebSocket,
}

/// The finished open of one idle connection.
enum Opened {
    Http1(tokio::net::TcpStream),
    Http2(tokio::net::TcpStream),
    WebSocket(tokio::net::TcpStream),
}

async fn open_one(addr: SocketAddr, kind: Kind, index: usize) -> Result<Opened, std::io::Error> {
    match kind {
        Kind::Http1 => {
            let mut stream = client::connect(addr).await?;
            let status = client::http1_get(&mut stream, addr, LIVE_PATH).await?;
            if status == 200 {
                Ok(Opened::Http1(stream))
            } else {
                Err(std::io::Error::other(format!("HTTP/1.1 answered {status}")))
            }
        }
        Kind::Http2 => Ok(Opened::Http2(client::http2_get(addr, LIVE_PATH).await?)),
        Kind::WebSocket => Ok(Opened::WebSocket(
            client::ws_join(addr, &server::token_for(index), &server::room_for(index)).await?,
        )),
    }
}

/// What the connect phase produced.
struct Connected {
    held: Held,
    errors: u64,
    first_error: Option<String>,
    elapsed: Duration,
}

async fn connect_all(addr: SocketAddr, config: &RunConfig) -> Connected {
    let (n1, n2, nw) = config.mix.split(config.connections);
    let kinds: Vec<Kind> = std::iter::repeat_n(Kind::Http1, n1)
        .chain(std::iter::repeat_n(Kind::Http2, n2))
        .chain(std::iter::repeat_n(Kind::WebSocket, nw))
        .collect();
    let started = Instant::now();
    let mut held = Held::default();
    let mut errors = 0_u64;
    let mut first_error = None;
    let mut set: JoinSet<Result<Opened, std::io::Error>> = JoinSet::new();
    let mut absorb = |result: Result<Result<Opened, std::io::Error>, tokio::task::JoinError>,
                      held: &mut Held| match result {
        Ok(Ok(Opened::Http1(s))) => held.http1.push(s),
        Ok(Ok(Opened::Http2(s))) => held.http2.push(s),
        Ok(Ok(Opened::WebSocket(s))) => held.websocket.push(s),
        Ok(Err(e)) => {
            errors += 1;
            first_error.get_or_insert_with(|| e.to_string());
        }
        Err(e) => {
            errors += 1;
            first_error.get_or_insert_with(|| e.to_string());
        }
    };
    for (index, kind) in kinds.into_iter().enumerate() {
        while set.len() >= config.connect_parallelism {
            if let Some(result) = set.join_next().await {
                absorb(result, &mut held);
            }
        }
        set.spawn(open_one(addr, kind, index));
    }
    while let Some(result) = set.join_next().await {
        absorb(result, &mut held);
    }
    Connected {
        held,
        errors,
        first_error,
        elapsed: started.elapsed(),
    }
}

/// The busy clients for `hold`: each on its own connection, at its share of `rps`. Returns the
/// latency of every successful request in microseconds, and how many requests failed.
async fn busy_load(addr: SocketAddr, clients: usize, rps: u32, hold: Duration) -> (Vec<u64>, u64) {
    let clients_u64 = u64::try_from(clients).unwrap_or(1).max(1);
    let interval = Duration::from_micros(1_000_000 * clients_u64 / u64::from(rps).max(1));
    let mut set: JoinSet<(Vec<u64>, u64)> = JoinSet::new();
    for index in 0..clients {
        set.spawn(async move {
            let mut samples = Vec::new();
            let mut errors = 0_u64;
            let Ok(mut stream) = client::connect(addr).await else {
                return (samples, 1);
            };
            let start = Instant::now();
            let stagger = interval
                .checked_mul(u32::try_from(index).unwrap_or(0))
                .and_then(|d| d.checked_div(u32::try_from(clients).unwrap_or(1)))
                .unwrap_or_default();
            tokio::time::sleep(stagger).await;
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            while start.elapsed() < hold {
                ticker.tick().await;
                let began = Instant::now();
                match client::http1_get(&mut stream, addr, PING_PATH).await {
                    Ok(200) => {
                        samples.push(u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX))
                    }
                    _ => errors += 1,
                }
            }
            (samples, errors)
        });
    }
    let (mut samples, mut errors) = (Vec::new(), 0_u64);
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((s, e)) => {
                samples.extend(s);
                errors += e;
            }
            Err(_) => errors += 1,
        }
    }
    (samples, errors)
}

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

/// Waits (briefly) for finished tasks to leave the runtime's count.
async fn settled_tasks(target: usize) -> usize {
    let mut now = alive_tasks();
    for _ in 0..40 {
        if now <= target {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        now = alive_tasks();
    }
    now
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Carries out one run and judges it against `budgets`.
///
/// Returns [`Outcome::Skipped`], without opening anything, when the process's descriptor limit is
/// below what the run needs; the limit is read and never raised.
///
/// # Errors
/// [`HarnessError`] when the settings are invalid or the server cannot be started or stopped.
pub async fn run(config: &RunConfig, budgets: &Budgets) -> Result<Outcome, HarnessError> {
    config.validate()?;
    let needed = config.descriptors_needed();
    let limit = measure::descriptor_limit();
    if let Some(limit) = limit
        && limit < needed
    {
        return Ok(Outcome::Skipped(format!(
            "the descriptor limit (RLIMIT_NOFILE soft) is {limit}, but {} idle connections need \
             {needed} (both ends live in this process, plus the busy clients and a margin of {}); \
             raise it with `ulimit -n` before the run, the harness never raises it",
            config.connections, config.descriptor_margin
        )));
    }
    let counting = events::install();
    let accept_before = events::accept_errors();
    let tasks_before = alive_tasks();
    let mut running = server::start(config).await?;
    let addr = running.addr;

    // The empty-server baseline: the same busy load before any idle connection exists.
    let baseline_rss = measure::rss_bytes();
    let baseline_fds = measure::open_descriptors();
    let (mut empty, empty_errors) =
        busy_load(addr, config.busy_clients, config.busy_rps, config.hold).await;

    // Open the idle connections.
    let connected = connect_all(addr, config).await;
    let connected_rss = measure::rss_bytes();
    let established = connected.held.len();
    let server_open = running.handle.open_connections();
    let server_ws = running.ws_sessions();

    // The same load with every idle connection held, answering pings so none is reaped.
    let keeper = {
        let pings = tokio::time::Duration::from_secs((config.ws_heartbeat_seconds / 2).max(1));
        (pings, connected.held.websocket.len())
    };
    let hold_task = busy_load(addr, config.busy_clients, config.busy_rps, config.hold);
    tokio::pin!(hold_task);
    let mut tick = tokio::time::interval(keeper.0);
    let (mut loaded, loaded_errors) = loop {
        tokio::select! {
            result = &mut hold_task => break result,
            _ = tick.tick() => connected.held.keep_alive(),
        }
    };
    let held_rss = measure::rss_bytes();
    let held_fds = measure::open_descriptors();
    let tasks_held = alive_tasks();

    // Shut down the way a deployment does on SIGTERM and time how long everything takes to close.
    let began = Instant::now();
    running.begin_shutdown();
    let patience =
        config.shutdown_grace + Duration::from_millis(GRACE_TOLERANCE_MS) + Duration::from_secs(2);
    let mut server_closed_ms = u64::MAX;
    while began.elapsed() < patience {
        // The clients answer the server's closing handshakes (HTTP/2 GOAWAY ping, WebSocket
        // close) while it drains, as real clients would.
        connected.held.keep_alive();
        if running.handle.open_connections() == 0 && running.ws_sessions() == 0 {
            server_closed_ms = millis(began.elapsed());
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let remaining = patience.saturating_sub(began.elapsed());
    let clients_still_open = connected.held.wait_closed(remaining).await;
    match tokio::time::timeout(patience, running.handle.join()).await {
        Ok(result) => {
            result.map_err(|e| HarnessError::new(format!("the server loop ended badly: {e}")))?
        }
        Err(_) => {
            return Err(HarnessError::new(
                "the server did not finish after shutdown",
            ));
        }
    }
    drop(connected.held);
    let tasks_after = settled_tasks(tasks_before).await;

    // Judge.
    let (n1, n2, nw) = config.mix.split(config.connections);
    let growth = |after: Option<u64>| match (baseline_rss, after) {
        (Some(base), Some(now)) => Some(now.saturating_sub(base)),
        _ => None,
    };
    let growth_bytes = match (growth(connected_rss), growth(held_rss)) {
        (Some(a), Some(b)) => Some(a.max(b)),
        _ => None,
    };
    let empty_p = percentiles(&mut empty);
    let loaded_p = percentiles(&mut loaded);
    let accept_errors = events::accept_errors().saturating_sub(accept_before);
    let inputs = Inputs {
        http_count: n1 + n2,
        ws_count: nw,
        established,
        connect_errors: connected.errors,
        server_open,
        server_ws,
        accept_errors,
        growth_bytes,
        empty_p99_us: empty_p.p99_us,
        loaded_p99_us: loaded_p.p99_us,
        busy_errors: empty_errors + loaded_errors,
        server_closed_ms,
        grace_ms: millis(config.shutdown_grace),
        clients_still_open,
        tasks_before,
        tasks_after,
    };
    let checks = verdict::evaluate(&inputs, budgets);
    let passed = checks.iter().all(|c| c.ok);
    let total = config.connections;
    let report = Report {
        setup: Setup {
            connections: total,
            http1: n1,
            http2: n2,
            websocket: nw,
            hold_seconds: config.hold.as_secs(),
            busy_clients: config.busy_clients,
            busy_rps: config.busy_rps,
            shutdown_grace_seconds: config.shutdown_grace.as_secs(),
        },
        memory: Memory {
            method: memory_method(),
            baseline_bytes: baseline_rss,
            connected_bytes: connected_rss,
            held_bytes: held_rss,
            growth_bytes,
            per_connection_bytes: growth_bytes
                .map(|g| g / u64::try_from(total.max(1)).unwrap_or(1)),
            budget_bytes: verdict::memory_budget_bytes(n1 + n2, nw, budgets),
        },
        descriptors: Descriptors {
            baseline: baseline_fds,
            held: held_fds,
            limit,
        },
        accept: Accept {
            time_to_last_ms: millis(connected.elapsed),
            established,
            connect_errors: connected.errors,
            first_connect_error: connected.first_error,
            accept_errors,
            accept_errors_counted: counting,
            server_open_connections: server_open,
            server_ws_sessions: server_ws,
        },
        latency: Latency {
            empty: empty_p,
            loaded: loaded_p,
            busy_errors: empty_errors + loaded_errors,
            p99_ceiling_us: verdict::p99_ceiling_us(empty_p.p99_us, budgets),
        },
        shutdown: Shutdown {
            server_closed_ms,
            clients_still_open,
            tasks_before,
            tasks_held,
            tasks_after,
        },
        checks,
        passed,
    };
    Ok(Outcome::Completed(Box::new(report)))
}

fn memory_method() -> &'static str {
    if cfg!(target_os = "linux") {
        "VmRSS of this process from /proc/self/status (server and clients together)"
    } else if cfg!(target_os = "macos") {
        "rss of this process from `ps -o rss=` (server and clients together)"
    } else if cfg!(target_os = "windows") {
        "WorkingSet64 of this process from PowerShell (server and clients together)"
    } else {
        "not available on this system"
    }
}
