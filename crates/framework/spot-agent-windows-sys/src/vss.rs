//! Volume Shadow Copy: the [`VssApi`] trait the modules drive, a complete in-memory
//! [`FakeVss`] the Linux test suite runs against, and a thin real implementation through
//! `vssadmin` behind `#[cfg(windows)]` (see [`crate::windows`]).
//!
//! A shadow copy is created for a non-empty [`VolumeSet`] with an optional [`WriterSet`]
//! (the writers to quiesce; an empty set means the VSS default, every writer). It is
//! listed, exposed read-only, and deleted. Nothing here reads the wall clock: the creation
//! instant is an argument ([`VssApi::create`]), so the retention policy is deterministic in
//! tests.
//!
//! The design is the same as WP-ADM-05 part 1: a trait with a fake that runs everywhere,
//! and only the thin real implementation gated on Windows.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::error::{WindowsSysError, WindowsSysResult};

/// A volume to shadow, for example `C:\`. Validated on construction: non-empty, trimmed,
/// at most 260 characters, and free of control characters and NUL.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VolumeId(String);

impl VolumeId {
    /// Validates and wraps a volume name.
    ///
    /// # Errors
    /// [`WindowsSysError::VssFailed`] when the text is empty, padded, too long or carries
    /// a control character.
    pub fn new(text: &str) -> WindowsSysResult<Self> {
        if text.is_empty() || text.trim() != text {
            return Err(WindowsSysError::VssFailed {
                reason: "a volume name must be non-empty and unpadded".to_owned(),
            });
        }
        if text.chars().count() > 260 {
            return Err(WindowsSysError::VssFailed {
                reason: "a volume name is at most 260 characters".to_owned(),
            });
        }
        if text.chars().any(|c| c.is_control()) {
            return Err(WindowsSysError::VssFailed {
                reason: "a volume name carries no control characters".to_owned(),
            });
        }
        Ok(VolumeId(text.to_owned()))
    }

    /// The volume name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A non-empty set of volumes to shadow together.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeSet(Vec<VolumeId>);

impl VolumeSet {
    /// Builds a volume set.
    ///
    /// # Errors
    /// [`WindowsSysError::VolumeSetEmpty`] when no volume is given.
    pub fn new(volumes: Vec<VolumeId>) -> WindowsSysResult<Self> {
        if volumes.is_empty() {
            return Err(WindowsSysError::VolumeSetEmpty);
        }
        Ok(VolumeSet(volumes))
    }

    /// One volume.
    #[must_use]
    pub fn one(volume: VolumeId) -> Self {
        VolumeSet(vec![volume])
    }

    /// The volumes, in order.
    #[must_use]
    pub fn as_slice(&self) -> &[VolumeId] {
        &self.0
    }

    /// How many volumes the set holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set is empty (never true for a constructed set).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The VSS writers to quiesce for a shadow copy. An empty set means the VSS default: every
/// writer participates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WriterSet(Vec<String>);

impl WriterSet {
    /// The default: every writer participates.
    #[must_use]
    pub fn all() -> Self {
        WriterSet(Vec::new())
    }

    /// Exactly these writer names.
    #[must_use]
    pub fn named(names: Vec<String>) -> Self {
        WriterSet(names)
    }

    /// The named writers, empty for "all".
    #[must_use]
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }
}

/// The id VSS assigned to a shadow copy.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShadowCopyId(String);

impl ShadowCopyId {
    /// Wraps an id as VSS reported it.
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        ShadowCopyId(text.into())
    }

    /// The id as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One shadow copy as the API reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowCopy {
    /// The shadow copy id.
    pub id: ShadowCopyId,
    /// The volumes it covers.
    pub volumes: Vec<VolumeId>,
    /// When it was created, in whole milliseconds since the Unix epoch. Supplied by the
    /// caller (the coordinator passes the injected clock's instant); never read here.
    pub created_at_unix_ms: u64,
}

/// One step of the VSS API, for failure injection in tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VssStep {
    /// [`VssApi::create`].
    Create,
    /// [`VssApi::list`].
    List,
    /// [`VssApi::delete`].
    Delete,
    /// [`VssApi::expose_read_only`].
    Expose,
    /// [`VssApi::unexpose`].
    Unexpose,
    /// [`VssApi::volume_of`].
    VolumeOf,
}

/// The Volume Shadow Copy service, as the modules use it.
///
/// The real implementation is [`crate::windows::WindowsVss`] (`#[cfg(windows)]`); tests
/// use [`FakeVss`]. Every method is synchronous: `vssadmin` runs as a child process and
/// the trait stays object-safe.
pub trait VssApi: Send + Sync {
    /// Creates a shadow copy of `volumes`, quiescing `writers`.
    ///
    /// # Errors
    /// [`WindowsSysError::VssFailed`] when VSS refuses the request.
    fn create(
        &self,
        volumes: &VolumeSet,
        writers: &WriterSet,
        at_unix_ms: u64,
    ) -> WindowsSysResult<ShadowCopy>;

    /// Lists the shadow copies VSS currently holds.
    ///
    /// # Errors
    /// [`WindowsSysError::VssFailed`] when VSS cannot be queried.
    fn list(&self) -> WindowsSysResult<Vec<ShadowCopy>>;

    /// Deletes one shadow copy.
    ///
    /// # Errors
    /// [`WindowsSysError::ShadowNotFound`] when the id is unknown, else
    /// [`WindowsSysError::VssFailed`].
    fn delete(&self, id: &ShadowCopyId) -> WindowsSysResult<()>;

    /// Exposes `id` read-only and returns the path inside the shadow that corresponds to
    /// `original` (a directory or file the caller already holds). The caller reads data
    /// through the returned path; the shadow is never written.
    ///
    /// # Errors
    /// [`WindowsSysError::ShadowNotFound`] when the id is unknown, else
    /// [`WindowsSysError::VssFailed`].
    fn expose_read_only(&self, id: &ShadowCopyId, original: &Path) -> WindowsSysResult<PathBuf>;

    /// Removes the read-only exposure of `id`. Idempotent: exposing was optional.
    ///
    /// # Errors
    /// [`WindowsSysError::VssFailed`] when VSS refuses.
    fn unexpose(&self, id: &ShadowCopyId) -> WindowsSysResult<()>;

    /// The volume that holds `path`, so a caller can choose what to shadow.
    ///
    /// # Errors
    /// [`WindowsSysError::VssFailed`] when the volume cannot be resolved.
    fn volume_of(&self, path: &Path) -> WindowsSysResult<VolumeId>;
}

/// How many shadow copies to keep. The oldest beyond `keep` are deleted, oldest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionPolicy {
    /// The number of newest shadow copies to keep.
    pub keep: usize,
}

impl RetentionPolicy {
    /// A policy that keeps `keep` newest shadow copies.
    #[must_use]
    pub fn keep(keep: usize) -> Self {
        RetentionPolicy { keep }
    }
}

/// Applies `policy`: lists the shadow copies and deletes the oldest until `keep` remain.
/// Returns the ids it deleted, oldest first. Ordering is by creation instant, then id, so
/// the result is deterministic.
///
/// # Errors
/// The first [`WindowsSysError`] from [`VssApi::list`] or [`VssApi::delete`].
pub fn apply_retention(
    vss: &dyn VssApi,
    policy: &RetentionPolicy,
) -> WindowsSysResult<Vec<ShadowCopyId>> {
    let mut all = vss.list()?;
    all.sort_by(|a, b| {
        a.created_at_unix_ms
            .cmp(&b.created_at_unix_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    if all.len() <= policy.keep {
        return Ok(Vec::new());
    }
    let remove = all.len() - policy.keep;
    let mut deleted = Vec::with_capacity(remove);
    for shadow in all.into_iter().take(remove) {
        vss.delete(&shadow.id)?;
        deleted.push(shadow.id);
    }
    Ok(deleted)
}

/// The in-memory VSS used by every test on every platform. It records the calls it
/// receives, holds the shadow copies it created, and can be told to fail at one step.
#[derive(Debug)]
pub struct FakeVss {
    state: Mutex<FakeState>,
}

#[derive(Debug)]
struct FakeState {
    shadows: Vec<ShadowCopy>,
    exposed: BTreeMap<String, PathBuf>,
    next: u64,
    fail: Option<VssStep>,
    shadow_root: PathBuf,
    volume: VolumeId,
    expose_calls: u64,
    unexpose_calls: u64,
}

impl FakeVss {
    /// A fake that maps every path to `volume` and exposes shadows under a temporary root
    /// the caller controls.
    #[must_use]
    pub fn new(volume: VolumeId, shadow_root: impl Into<PathBuf>) -> Self {
        FakeVss {
            state: Mutex::new(FakeState {
                shadows: Vec::new(),
                exposed: BTreeMap::new(),
                next: 0,
                fail: None,
                shadow_root: shadow_root.into(),
                volume,
                expose_calls: 0,
                unexpose_calls: 0,
            }),
        }
    }

    /// Makes the named step fail once the fake is used.
    pub fn fail_at(&self, step: VssStep) {
        self.lock().fail = Some(step);
    }

    /// How many shadow copies the fake still holds.
    #[must_use]
    pub fn count(&self) -> usize {
        self.lock().shadows.len()
    }

    /// The ids of the shadow copies the fake still holds, in creation order.
    #[must_use]
    pub fn ids(&self) -> Vec<ShadowCopyId> {
        self.lock()
            .shadows
            .iter()
            .map(|shadow| shadow.id.clone())
            .collect()
    }

    /// How many times `expose_read_only` was called.
    #[must_use]
    pub fn expose_calls(&self) -> u64 {
        self.lock().expose_calls
    }

    /// How many times `unexpose` was called.
    #[must_use]
    pub fn unexpose_calls(&self) -> u64 {
        self.lock().unexpose_calls
    }

    /// Whether the named step is set to fail.
    fn should_fail(&self, step: VssStep) -> bool {
        let mut state = self.lock();
        if state.fail == Some(step) {
            state.fail = None;
            true
        } else {
            false
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl VssApi for FakeVss {
    fn create(
        &self,
        volumes: &VolumeSet,
        _writers: &WriterSet,
        at_unix_ms: u64,
    ) -> WindowsSysResult<ShadowCopy> {
        if self.should_fail(VssStep::Create) {
            return Err(WindowsSysError::VssFailed {
                reason: "the fake was told to fail at create".to_owned(),
            });
        }
        let mut state = self.lock();
        state.next += 1;
        let shadow = ShadowCopy {
            id: ShadowCopyId::new(format!("shadow-{}", state.next)),
            volumes: volumes.as_slice().to_vec(),
            created_at_unix_ms: at_unix_ms,
        };
        state.shadows.push(shadow.clone());
        Ok(shadow)
    }

    fn list(&self) -> WindowsSysResult<Vec<ShadowCopy>> {
        if self.should_fail(VssStep::List) {
            return Err(WindowsSysError::VssFailed {
                reason: "the fake was told to fail at list".to_owned(),
            });
        }
        Ok(self.lock().shadows.clone())
    }

    fn delete(&self, id: &ShadowCopyId) -> WindowsSysResult<()> {
        if self.should_fail(VssStep::Delete) {
            return Err(WindowsSysError::VssFailed {
                reason: "the fake was told to fail at delete".to_owned(),
            });
        }
        let mut state = self.lock();
        let before = state.shadows.len();
        state.shadows.retain(|shadow| shadow.id != *id);
        state.exposed.remove(id.as_str());
        if state.shadows.len() == before {
            return Err(WindowsSysError::ShadowNotFound {
                id: id.as_str().to_owned(),
            });
        }
        Ok(())
    }

    fn expose_read_only(&self, id: &ShadowCopyId, original: &Path) -> WindowsSysResult<PathBuf> {
        if self.should_fail(VssStep::Expose) {
            return Err(WindowsSysError::VssFailed {
                reason: "the fake was told to fail at expose".to_owned(),
            });
        }
        let mut state = self.lock();
        if !state.shadows.iter().any(|shadow| shadow.id == *id) {
            return Err(WindowsSysError::ShadowNotFound {
                id: id.as_str().to_owned(),
            });
        }
        state.expose_calls += 1;
        // The fake's shadow root mirrors the volume root: the caller's path is reproduced
        // under the root by its last component, which is all the tests need.
        let name = original
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mapped = state.shadow_root.join(name);
        state.exposed.insert(id.as_str().to_owned(), mapped.clone());
        Ok(mapped)
    }

    fn unexpose(&self, id: &ShadowCopyId) -> WindowsSysResult<()> {
        if self.should_fail(VssStep::Unexpose) {
            return Err(WindowsSysError::VssFailed {
                reason: "the fake was told to fail at unexpose".to_owned(),
            });
        }
        let mut state = self.lock();
        state.unexpose_calls += 1;
        state.exposed.remove(id.as_str());
        Ok(())
    }

    fn volume_of(&self, _path: &Path) -> WindowsSysResult<VolumeId> {
        if self.should_fail(VssStep::VolumeOf) {
            return Err(WindowsSysError::VssFailed {
                reason: "the fake was told to fail at volume_of".to_owned(),
            });
        }
        Ok(self.lock().volume.clone())
    }
}
