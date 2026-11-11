//! An in-process TCP proxy between the backend and PostgreSQL, with scriptable faults.
//!
//! [`FaultProxy`] listens on a loopback port, connects each accepted connection to PostgreSQL and
//! relays bytes in both directions. Along the way it watches the PostgreSQL message stream
//! ([`crate::wire`]: type bytes and lengths only, never query text) and applies the faults
//! scheduled through [`FaultProxy::schedule`]: `pass`, `delay(ms)`, `drop_after_bytes(n)`,
//! `reset_now`, `blackhole`, `half_close` and `reset_when_message(type)`. Every fault that fires
//! is recorded in a [`FaultTimeline`] with its time.
//!
//! A fault is scheduled per connection ([`ConnSelector`]) and per direction
//! ([`Direction`]). Faults scheduled after a connection was opened still apply to it: each
//! direction re-reads the schedule as it relays. This is what lets a scenario arm a
//! commit-ack-loss fault only after the driver's setup transaction has finished.
//!
//! `blackhole` is a **global window** per direction: the first time it fires it silences that
//! direction for its duration, across every selected connection, and [`FaultProxy::clear`] heals
//! it — that is how a partition is healed. Every other fault is per connection.
//!
//! # Honest limit
//!
//! "Reset" closes the connection; on this platform that is a `FIN`/EOF the client sees as a
//! lost connection, not a forged TCP `RST` (which would need socket options the workspace forbids
//! without `unsafe`). For the platform's purposes the two are the same: the client did not hear
//! back from the commit, so its outcome is unknown and must be resolved by reading back.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use crate::faults::{ConnSelector, Direction, Fault, FaultTimeline, ScheduledFault, Trigger};
use crate::wire::{Frame, FrameScanner};

/// The largest chunk the relay reads at once.
const CHUNK: usize = 16 * 1024;

/// How the proxy is configured.
#[derive(Clone, Copy, Debug)]
pub struct ProxyConfig {
    /// The PostgreSQL address to relay to.
    pub upstream: SocketAddr,
    /// Read/write timeout for one relay read; a stalled connection is closed after it.
    pub idle_timeout: Duration,
}

impl ProxyConfig {
    /// A config relaying to `upstream`, with a 30-second idle timeout.
    pub fn new(upstream: SocketAddr) -> ProxyConfig {
        ProxyConfig {
            upstream,
            idle_timeout: Duration::from_secs(30),
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn dir_index(direction: Direction) -> usize {
    match direction {
        Direction::ClientToServer => 0,
        Direction::ServerToClient => 1,
    }
}

/// A scheduled fault plus its global fire count.
struct ArmedSpec {
    spec: ScheduledFault,
    fires: u32,
}

struct Shared {
    schedule: Mutex<Vec<ArmedSpec>>,
    timeline: Mutex<FaultTimeline>,
    next_conn: AtomicU64,
    /// Global blackhole deadline per direction (`[c2s, s2c]`).
    blackhole_until: Mutex<[Option<Instant>; 2]>,
    /// Global added latency per direction (`[c2s, s2c]`).
    delay: Mutex<[Duration; 2]>,
}

/// One scheduled fault as a single connection sees it: the spec plus its per-connection counters.
struct ConnFault {
    index: usize,
    spec: ScheduledFault,
    occurrences: u32,
    done: bool,
}

/// What a relay loop should do with a chunk after inspecting it.
#[derive(Clone, Copy, Debug)]
enum Action {
    Close,
    HalfClose,
    Blackhole(Duration),
    Delay(Duration),
}

/// The proxy: start it, schedule faults, point a backend at [`FaultProxy::addr`].
pub struct FaultProxy {
    addr: SocketAddr,
    shared: Arc<Shared>,
    accept: JoinHandle<()>,
}

impl FaultProxy {
    /// Binds a loopback port and starts relaying to `upstream`.
    pub async fn start(config: ProxyConfig) -> std::io::Result<FaultProxy> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let shared = Arc::new(Shared {
            schedule: Mutex::new(Vec::new()),
            timeline: Mutex::new(FaultTimeline::new()),
            next_conn: AtomicU64::new(1),
            blackhole_until: Mutex::new([None, None]),
            delay: Mutex::new([Duration::ZERO, Duration::ZERO]),
        });
        let accept_shared = shared.clone();
        let accept = tokio::spawn(async move {
            loop {
                let Ok((client, _)) = listener.accept().await else {
                    break;
                };
                let conn_id = accept_shared.next_conn.fetch_add(1, Ordering::Relaxed);
                let shared = accept_shared.clone();
                tokio::spawn(async move {
                    run_connection(client, conn_id, config, shared).await;
                });
            }
        });
        Ok(FaultProxy {
            addr,
            shared,
            accept,
        })
    }

    /// The loopback address a backend should connect to.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Adds a fault to the schedule. It applies to connections already open, from now on.
    pub fn schedule(&self, fault: ScheduledFault) {
        lock(&self.shared.schedule).push(ArmedSpec {
            spec: fault,
            fires: 0,
        });
    }

    /// Removes every scheduled fault, heals every blackhole and clears every delay: the fault
    /// ends here.
    pub fn clear(&self) {
        lock(&self.shared.schedule).clear();
        let mut bh = lock(&self.shared.blackhole_until);
        *bh = [None, None];
        let mut delay = lock(&self.shared.delay);
        *delay = [Duration::ZERO, Duration::ZERO];
    }

    /// The faults that have fired so far.
    pub fn events(&self) -> Vec<crate::faults::FaultEvent> {
        lock(&self.shared.timeline).events().to_vec()
    }

    /// The timeline rendered as text.
    pub fn timeline_text(&self) -> String {
        lock(&self.shared.timeline).to_text()
    }

    /// Stops accepting new connections. Open connections finish on their own.
    pub fn stop(&self) {
        self.accept.abort();
    }
}

impl Drop for FaultProxy {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// One proxied connection: connect upstream, apply immediate faults, then relay both ways.
async fn run_connection(client: TcpStream, conn_id: u64, config: ProxyConfig, shared: Arc<Shared>) {
    let Ok(server) = TcpStream::connect(config.upstream).await else {
        return;
    };
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();

    let mut c2s = DirectionState::new(Direction::ClientToServer, conn_id, shared.clone());
    let mut s2c = DirectionState::new(Direction::ServerToClient, conn_id, shared.clone());
    if apply_immediate(&mut c2s) || apply_immediate(&mut s2c) {
        return;
    }

    let a = relay(&mut cr, &mut sw, &mut c2s, config.idle_timeout);
    let b = relay(&mut sr, &mut cw, &mut s2c, config.idle_timeout);
    tokio::pin!(a);
    tokio::pin!(b);
    tokio::select! {
        _ = &mut a => {}
        _ = &mut b => {}
    }
}

/// One direction's live state.
struct DirectionState {
    direction: Direction,
    conn_id: u64,
    shared: Arc<Shared>,
    local: Vec<ConnFault>,
    scanner: FrameScanner,
    write_disabled: bool,
}

impl DirectionState {
    fn new(direction: Direction, conn_id: u64, shared: Arc<Shared>) -> DirectionState {
        DirectionState {
            direction,
            conn_id,
            shared,
            local: Vec::new(),
            scanner: FrameScanner::new(),
            write_disabled: false,
        }
    }

    /// Picks up faults scheduled after this connection opened.
    fn sync(&mut self) {
        let schedule = lock(&self.shared.schedule);
        for (index, armed) in schedule.iter().enumerate() {
            if armed.spec.direction != self.direction {
                continue;
            }
            if !conn_matches(armed.spec.conn, self.conn_id) {
                continue;
            }
            if self.local.iter().any(|c| c.index == index) {
                continue;
            }
            self.local.push(ConnFault {
                index,
                spec: armed.spec,
                occurrences: 0,
                done: false,
            });
        }
    }

    fn record(&self, fault: Fault, detail: String) {
        lock(&self.shared.timeline).record(self.conn_id, self.direction, fault, detail);
    }

    fn blackholed(&self) -> bool {
        let bh = lock(&self.shared.blackhole_until);
        bh[dir_index(self.direction)].is_some_and(|until| Instant::now() < until)
    }

    fn delay(&self) -> Duration {
        let delay = lock(&self.shared.delay);
        delay[dir_index(self.direction)]
    }

    fn set_delay(&self, delay: Duration) {
        let mut slot = lock(&self.shared.delay);
        slot[dir_index(self.direction)] = delay;
    }
}

fn conn_matches(selector: ConnSelector, conn_id: u64) -> bool {
    match selector {
        ConnSelector::Any => true,
        ConnSelector::Ordinal(n) => n == conn_id,
    }
}

/// Applies the immediate faults for a connection; returns true when it must close at once.
fn apply_immediate(state: &mut DirectionState) -> bool {
    let faults: Vec<ScheduledFault> = {
        let schedule = lock(&state.shared.schedule);
        schedule
            .iter()
            .filter(|armed| {
                armed.spec.direction == state.direction
                    && matches!(armed.spec.trigger, Trigger::Immediate)
                    && conn_matches(armed.spec.conn, state.conn_id)
            })
            .map(|armed| armed.spec)
            .collect()
    };
    for spec in faults {
        state.record(spec.fault, "immediate".to_owned());
        match spec.fault {
            Fault::Pass => {}
            Fault::Delay { ms } => state.set_delay(Duration::from_millis(ms)),
            Fault::Blackhole { ms } => {
                let mut bh = lock(&state.shared.blackhole_until);
                let idx = dir_index(state.direction);
                let until = Instant::now() + Duration::from_millis(ms);
                bh[idx] = Some(bh[idx].map_or(until, |existing| existing.max(until)));
            }
            Fault::HalfClose => state.write_disabled = true,
            Fault::Drop | Fault::Reset => return true,
        }
    }
    false
}

/// Relays one direction until it ends.
async fn relay(
    src: &mut tokio::net::tcp::OwnedReadHalf,
    dst: &mut tokio::net::tcp::OwnedWriteHalf,
    state: &mut DirectionState,
    idle_timeout: Duration,
) {
    let mut buf = vec![0u8; CHUNK];
    loop {
        let read = match tokio::time::timeout(idle_timeout, src.read(&mut buf)).await {
            Ok(Ok(0)) | Err(_) => break,
            Ok(Ok(n)) => n,
            Ok(Err(_)) => break,
        };
        state.sync();

        if state.blackholed() {
            continue;
        }

        let base = state.scanner.bytes_seen();
        let frames = state.scanner.feed(&buf[..read]);
        let end = state.scanner.bytes_seen();
        let decision = decide(state, &frames, base, end, read);

        if state.write_disabled {
            continue;
        }
        let delay = state.delay();
        if decision.relay_len > 0 {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if dst.write_all(&buf[..decision.relay_len]).await.is_err() {
                break;
            }
            let _ = dst.flush().await;
        }
        if let Some((fault, detail)) = decision.event {
            state.record(fault, detail);
        }
        match decision.action {
            Some(Action::Close) => break,
            Some(Action::HalfClose) => {
                state.write_disabled = true;
                let _ = dst.shutdown().await;
            }
            Some(Action::Blackhole(ms)) => {
                let mut bh = lock(&state.shared.blackhole_until);
                let idx = dir_index(state.direction);
                bh[idx] = Some(Instant::now() + ms);
            }
            Some(Action::Delay(ms)) => state.set_delay(ms),
            None => {}
        }
    }
}

/// What to relay and which fault (if any) fired.
struct Decision {
    relay_len: usize,
    action: Option<Action>,
    event: Option<(Fault, String)>,
}

/// The best firing fault in one chunk.
struct Candidate {
    len: usize,
    action: Action,
    fault: Fault,
    detail: String,
    index: usize,
}

fn decide(
    state: &mut DirectionState,
    frames: &[Frame],
    base: u64,
    end: u64,
    read: usize,
) -> Decision {
    let mut best: Option<Candidate> = None;
    {
        let mut schedule = lock(&state.shared.schedule);
        for cf in state.local.iter_mut() {
            if cf.done || matches!(cf.spec.fault, Fault::Pass) {
                continue;
            }
            if let Some(max) = cf.spec.max_fires
                && schedule[cf.index].fires >= max
            {
                cf.done = true;
                continue;
            }
            let action = action_for(cf.spec.fault);
            match cf.spec.trigger {
                Trigger::Immediate => {}
                Trigger::AfterBytes(n) => {
                    if end >= n {
                        let len = usize::try_from(n.saturating_sub(base).min(read as u64))
                            .unwrap_or(read);
                        schedule[cf.index].fires += 1;
                        cf.done = true;
                        consider(
                            &mut best,
                            cf.index,
                            len,
                            action,
                            cf.spec.fault,
                            format!("bytes={n}"),
                        );
                    }
                }
                Trigger::OnMessage(t) => {
                    for frame in frames {
                        if !t.observe(frame.kind.message(), &mut cf.occurrences) {
                            continue;
                        }
                        let len = if t.after {
                            usize::try_from(frame.end.saturating_sub(base)).unwrap_or(read)
                        } else {
                            usize::try_from(frame.start.saturating_sub(base)).unwrap_or(0)
                        };
                        schedule[cf.index].fires += 1;
                        if !t.repeat || cf.spec.max_fires.is_some() {
                            cf.done = true;
                        }
                        consider(
                            &mut best,
                            cf.index,
                            len,
                            action,
                            cf.spec.fault,
                            format!("message={}", t.message.name()),
                        );
                        if !t.repeat {
                            break;
                        }
                    }
                }
            }
        }
    }
    match best {
        None => Decision {
            relay_len: read,
            action: None,
            event: None,
        },
        Some(c) => {
            let len = if matches!(c.action, Action::Delay(_)) {
                read
            } else {
                c.len
            };
            Decision {
                relay_len: len.min(read),
                action: Some(c.action),
                event: Some((c.fault, c.detail)),
            }
        }
    }
}

fn action_for(fault: Fault) -> Action {
    match fault {
        Fault::Pass => Action::Close, // Pass is skipped in `decide`; this is unreachable.
        Fault::Delay { ms } => Action::Delay(Duration::from_millis(ms)),
        Fault::Drop | Fault::Reset => Action::Close,
        Fault::Blackhole { ms } => Action::Blackhole(Duration::from_millis(ms)),
        Fault::HalfClose => Action::HalfClose,
    }
}

fn consider(
    best: &mut Option<Candidate>,
    index: usize,
    len: usize,
    action: Action,
    fault: Fault,
    detail: String,
) {
    let replace = match best {
        None => true,
        Some(c) => len < c.len || (len == c.len && index < c.index),
    };
    if replace {
        *best = Some(Candidate {
            len,
            action,
            fault,
            detail,
            index,
        });
    }
}
