//! Compiled scripts and the bytecode cache.
//!
//! [`ScriptEngine::compile`](crate::ScriptEngine::compile) turns JavaScript source into a
//! [`Compiled`]: QuickJS bytecode (when it can be kept, see below), the source, and a hash of the
//! source. The cache holds `Compiled` values keyed by [`ScriptId`] (app, name and **version**) with
//! a size cap and least-recently-used eviction; a changed version is a new entry and an unchanged
//! one is never recompiled.
//!
//! # Why bytecode is interned for the life of the process
//!
//! QuickJS reads bytecode with `JS_READ_OBJ_ROM_DATA`: the runtime keeps pointers into the bytes,
//! so they must outlive it, and the only safe way to load them from this crate (it forbids
//! `unsafe`) is rquickjs' `Bundle` loader, which takes `&'static` data. The bytecode of a script
//! is therefore interned once, by content hash, and lives for the process. That memory is
//! **bounded**: past [`INTERN_BUDGET`] a script keeps no bytecode and runs from its source
//! (slower to start, equally correct). Evicting a cache entry frees the source and the map, not the
//! interned bytes, so a long-running process that compiles an unbounded number of *distinct*
//! sources stops caching bytecode rather than growing.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use crate::sha::sha256_hex;
use crate::sourcemap::SourceMap;

/// Most bytes of bytecode interned for the life of the process.
pub const INTERN_BUDGET: usize = 64 * 1024 * 1024;

/// The module name every script is registered under.
pub(crate) const MODULE_NAME: &str = "script";

/// Bytecode in the shape rquickjs' bundle loader takes.
pub(crate) type Bundle = &'static [(&'static str, &'static [u8])];

/// Identifies one version of one script of one app.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScriptId {
    /// The app that owns the script.
    pub app: String,
    /// The script's name within the app.
    pub name: String,
    /// The script's version: a new save is a new version.
    pub version: u64,
}

impl ScriptId {
    /// An id.
    pub fn new(app: &str, name: &str, version: u64) -> Self {
        Self {
            app: app.to_owned(),
            name: name.to_owned(),
            version,
        }
    }
}

/// A script ready to run.
#[derive(Debug)]
pub struct Compiled {
    pub(crate) id: ScriptId,
    pub(crate) source: Arc<str>,
    pub(crate) hash: String,
    pub(crate) bundle: Option<Bundle>,
    pub(crate) bytecode_len: usize,
    pub(crate) map: Option<SourceMap>,
}

impl Compiled {
    /// The script's id.
    pub fn id(&self) -> &ScriptId {
        &self.id
    }

    /// The SHA-256 of the source, as 64 lowercase hex digits.
    pub fn source_hash(&self) -> &str {
        &self.hash
    }

    /// The size of the QuickJS bytecode, in bytes; `0` if it was not kept (over the intern budget).
    pub fn bytecode_len(&self) -> usize {
        self.bytecode_len
    }

    /// Whether the script runs from bytecode (otherwise from source).
    pub fn has_bytecode(&self) -> bool {
        self.bundle.is_some()
    }

    /// Whether both scripts run from the very same interned bytecode (identical bytecode is stored
    /// once, whichever script, version or engine compiled it).
    pub fn shares_bytecode_with(&self, other: &Compiled) -> bool {
        match (self.bundle, other.bundle) {
            (Some(a), Some(b)) => std::ptr::eq(a, b),
            _ => false,
        }
    }

    fn weight(&self) -> usize {
        self.source.len() + self.bytecode_len + 256
    }
}

struct Interner {
    by_hash: HashMap<String, Bundle>,
    bytes: usize,
}

fn interner() -> &'static Mutex<Interner> {
    static INTERNER: OnceLock<Mutex<Interner>> = OnceLock::new();
    INTERNER.get_or_init(|| {
        Mutex::new(Interner {
            by_hash: HashMap::new(),
            bytes: 0,
        })
    })
}

/// Interns `bytes` (once per distinct content) for the life of the process, or `None` when the
/// budget is spent.
pub(crate) fn intern(bytes: Vec<u8>) -> Option<Bundle> {
    let key = sha256_hex(&bytes);
    let mut interner = interner().lock().ok()?;
    if let Some(bundle) = interner.by_hash.get(&key) {
        return Some(bundle);
    }
    if interner.bytes.checked_add(bytes.len())? > INTERN_BUDGET {
        return None;
    }
    interner.bytes += bytes.len();
    let data: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    let bundle: Bundle = Box::leak(Box::new([(MODULE_NAME, data)]));
    interner.by_hash.insert(key, bundle);
    Some(bundle)
}

/// Bytes of bytecode interned so far, process-wide.
pub fn interned_bytes() -> usize {
    interner().lock().map_or(0, |i| i.bytes)
}

/// The cache of compiled scripts.
pub(crate) struct Cache {
    entries: HashMap<ScriptId, Arc<Compiled>>,
    /// Least recently used first.
    order: VecDeque<ScriptId>,
    bytes: usize,
    cap: usize,
}

impl Cache {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            entries: HashMap::new(),
            order: VecDeque::new(),
            bytes: 0,
            cap,
        }
    }

    /// The cached script for `id` with this source hash, marking it recently used.
    pub(crate) fn get(&mut self, id: &ScriptId, hash: &str) -> Option<Arc<Compiled>> {
        let found = self.entries.get(id).filter(|c| c.hash == hash).cloned()?;
        self.touch(id);
        Some(found)
    }

    /// Stores `compiled`, replacing an entry with the same id, and evicts from the least recently
    /// used end until the cache is within its cap (the newest entry always stays).
    pub(crate) fn put(&mut self, compiled: Arc<Compiled>) {
        let id = compiled.id.clone();
        if let Some(old) = self.entries.remove(&id) {
            self.bytes -= old.weight();
            self.order.retain(|i| i != &id);
        }
        self.bytes += compiled.weight();
        self.entries.insert(id.clone(), compiled);
        self.order.push_back(id);
        while self.bytes > self.cap && self.order.len() > 1 {
            if let Some(oldest) = self.order.pop_front()
                && let Some(gone) = self.entries.remove(&oldest)
            {
                self.bytes -= gone.weight();
            }
        }
    }

    fn touch(&mut self, id: &ScriptId) {
        self.order.retain(|i| i != id);
        self.order.push_back(id.clone());
    }

    pub(crate) fn len(&self) -> usize {
        self.entries.len()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.bytes
    }

    pub(crate) fn contains(&self, id: &ScriptId) -> bool {
        self.entries.contains_key(id)
    }
}
