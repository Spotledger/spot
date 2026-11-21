//! The file watcher: a platform [`EventSource`] plus a debouncing [`Watcher`].
//!
//! The platform source is `notify` — inotify on Fedora, `ReadDirectoryChangesW` on Windows —
//! behind the [`EventSource`] seam, so the debouncing logic is unit-tested with a fake source
//! ([`VecEventSource`]) and the real filesystem is exercised in an integration test.
//!
//! Events are **normalised** before they reach the reload pipeline:
//!
//! - `\` is folded to `/` and the path is lower-cased by [`path_key`](crate::paths::path_key), so
//!   one file has one debounce key on every platform.
//! - An editor's atomic save is create-temp → rename-temp → real file. The temp file's own
//!   events are dropped by [`is_atomic_temp`](crate::paths::is_atomic_temp); a rename marks the
//!   destination as a write ([`Op::Renamed`]).
//! - Events under [`IGNORED_DIRS`](crate::paths::IGNORED_DIRS) never reach a pending slot.
//!
//! Debouncing is driven by an **injected** monotonic time ([`Watcher::poll`] takes it), so the
//! tests never sleep and the host decides the cadence. A change is emitted once it has been
//! quiet for the configured debounce window.

use std::collections::BTreeMap;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use camino::Utf8PathBuf;
use notify::{RecommendedWatcher, RecursiveMode, Watcher as NotifyWatcher};

use crate::error::{DevError, DevResult};
use crate::paths;

/// What happened to a path, normalised across platforms.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    /// The path appeared.
    Created,
    /// The path's contents changed.
    Modified,
    /// The path disappeared.
    Removed,
    /// The path was renamed onto (the editor's atomic save, seen as a write).
    Renamed,
    /// An event the platform reported but the pipeline has no use for.
    Other,
}

impl Op {
    /// The lowercase name used in logs and status JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Modified => "modified",
            Self::Removed => "removed",
            Self::Renamed => "renamed",
            Self::Other => "other",
        }
    }
}

/// One normalised filesystem event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawEvent {
    /// The absolute or root-relative path the event names.
    pub path: Utf8PathBuf,
    /// What happened to it.
    pub op: Op,
    /// Whether the platform reported a rename (an editor's atomic save).
    pub is_rename: bool,
}

impl RawEvent {
    /// A plain event.
    #[must_use]
    pub fn new(path: impl Into<Utf8PathBuf>, op: Op) -> Self {
        Self {
            path: path.into(),
            op,
            is_rename: false,
        }
    }

    /// A rename onto `path` (the editor's atomic save).
    #[must_use]
    pub fn renamed(path: impl Into<Utf8PathBuf>) -> Self {
        Self {
            path: path.into(),
            op: Op::Renamed,
            is_rename: true,
        }
    }
}

/// A source of normalised filesystem events the [`Watcher`] polls.
pub trait EventSource: Send {
    /// Starts watching `roots` recursively.
    ///
    /// # Errors
    ///
    /// [`DevError::Watch`] when the platform watcher refuses a root.
    fn watch(&mut self, roots: &[Utf8PathBuf]) -> DevResult<()>;

    /// Every event that arrived since the last call; never blocks.
    fn drain(&mut self) -> Vec<RawEvent>;
}

/// The real [`EventSource`], over `notify`.
pub struct NotifyEventSource {
    watcher: RecommendedWatcher,
    rx: Receiver<RawEvent>,
}

impl NotifyEventSource {
    /// Starts a platform watcher. No root is watched until [`EventSource::watch`] is called.
    ///
    /// # Errors
    ///
    /// [`DevError::Watch`] when the platform backend cannot be created.
    pub fn new() -> DevResult<Self> {
        let (tx, rx) = mpsc::channel();
        let watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
            if let Ok(event) = result {
                for event in normalise(&event) {
                    // A closed receiver only means the watcher outlived the server; drop.
                    let _ = tx.send(event);
                }
            }
        })
        .map_err(|error| DevError::Watch {
            message: error.to_string(),
        })?;
        Ok(Self { watcher, rx })
    }
}

impl EventSource for NotifyEventSource {
    fn watch(&mut self, roots: &[Utf8PathBuf]) -> DevResult<()> {
        for root in roots {
            self.watcher
                .watch(root.as_std_path(), RecursiveMode::Recursive)
                .map_err(|error| DevError::Watch {
                    message: format!("cannot watch `{root}`: {error}"),
                })?;
        }
        Ok(())
    }

    fn drain(&mut self) -> Vec<RawEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.rx.try_recv() {
            out.push(event);
        }
        out
    }
}

/// Normalises one `notify` event: case-folded paths, rename-as-write, dropped access events.
fn normalise(event: &notify::Event) -> Vec<RawEvent> {
    let is_rename = matches!(
        event.kind,
        notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
    );
    let op = match event.kind {
        notify::EventKind::Create(_) => Op::Created,
        notify::EventKind::Remove(_) => Op::Removed,
        notify::EventKind::Modify(notify::event::ModifyKind::Name(_)) => Op::Renamed,
        notify::EventKind::Modify(_) => Op::Modified,
        notify::EventKind::Any => Op::Modified,
        notify::EventKind::Access(_) | notify::EventKind::Other => Op::Other,
    };
    event
        .paths
        .iter()
        .filter_map(|path| {
            Utf8PathBuf::from_path_buf(path.clone())
                .ok()
                .map(|path| RawEvent {
                    path,
                    op,
                    is_rename,
                })
        })
        .collect()
}

/// A fake [`EventSource`] for tests: events are pushed in and drained verbatim.
#[derive(Default)]
pub struct VecEventSource {
    events: Vec<RawEvent>,
    watched: Vec<Utf8PathBuf>,
    failure: Option<String>,
}

impl VecEventSource {
    /// An empty source.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues an event for the next drain.
    pub fn push(&mut self, event: RawEvent) {
        self.events.push(event);
    }

    /// Makes the next [`EventSource::watch`] fail with `message`.
    pub fn fail_watch(&mut self, message: impl Into<String>) {
        self.failure = Some(message.into());
    }

    /// The roots `watch` was last called with.
    #[must_use]
    pub fn watched(&self) -> &[Utf8PathBuf] {
        &self.watched
    }
}

impl EventSource for VecEventSource {
    fn watch(&mut self, roots: &[Utf8PathBuf]) -> DevResult<()> {
        if let Some(message) = self.failure.take() {
            return Err(DevError::Watch { message });
        }
        self.watched = roots.to_vec();
        Ok(())
    }

    fn drain(&mut self) -> Vec<RawEvent> {
        std::mem::take(&mut self.events)
    }
}

/// The watcher's debounce window and ignore list.
#[derive(Clone, Debug)]
pub struct WatcherConfig {
    /// How long a change must be quiet before it is emitted.
    pub debounce: Duration,
    /// Extra directory names to ignore, on top of [`IGNORED_DIRS`](crate::paths::IGNORED_DIRS).
    pub extra_ignored: Vec<String>,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(200),
            extra_ignored: Vec::new(),
        }
    }
}

/// One coalesced change waiting for its debounce window to close.
struct Pending {
    event: RawEvent,
    last_seen: Duration,
}

/// Debounces the events of an [`EventSource`] into one event per path.
pub struct Watcher<S: EventSource> {
    source: S,
    config: WatcherConfig,
    pending: BTreeMap<String, Pending>,
}

impl<S: EventSource> Watcher<S> {
    /// Wraps `source` with `config`.
    pub fn new(source: S, config: WatcherConfig) -> Self {
        Self {
            source,
            config,
            pending: BTreeMap::new(),
        }
    }

    /// The source, for a test to push events into.
    #[must_use]
    pub fn source(&self) -> &S {
        &self.source
    }

    /// The source, mutably, for a test to push events into.
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    /// Starts watching `roots`.
    ///
    /// # Errors
    ///
    /// [`DevError::Watch`] from the source.
    pub fn watch(&mut self, roots: &[Utf8PathBuf]) -> DevResult<()> {
        self.source.watch(roots)
    }

    /// Drains the source, coalesces by debounce key, and returns every change whose quiet
    /// window has closed at `now`. Sorted by path key, so the pipeline's order is deterministic.
    #[must_use]
    pub fn poll(&mut self, now: Duration) -> Vec<RawEvent> {
        for event in self.source.drain() {
            if self.ignored(&event.path) {
                continue;
            }
            let key = paths::path_key(&event.path);
            match self.pending.get_mut(&key) {
                Some(pending) => {
                    pending.event = event;
                    pending.last_seen = now;
                }
                None => {
                    self.pending.insert(
                        key,
                        Pending {
                            event,
                            last_seen: now,
                        },
                    );
                }
            }
        }
        let debounce = self.config.debounce;
        self.pending
            .extract_if(.., |_, pending| {
                now.saturating_sub(pending.last_seen) >= debounce
            })
            .map(|(_, pending)| pending.event)
            .collect()
    }

    /// How many paths are waiting for their debounce window to close.
    #[must_use]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Whether `path` is ignored: an ignored directory, an editor temp file, or on the extra
    /// list.
    fn ignored(&self, path: &camino::Utf8Path) -> bool {
        if paths::is_ignored(path) || paths::is_atomic_temp(path) {
            return true;
        }
        path.components().any(|component| match component {
            camino::Utf8Component::Normal(name) => {
                self.config.extra_ignored.iter().any(|extra| extra == name)
            }
            _ => false,
        })
    }
}
