//! The tenant-side registry and archive (WP-ADM-11.T2); see the parent module for the design.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use spot_break_glass::audit::{ENDED, EXPIRED, RECORDING, REVOKED};
use spot_break_glass::{
    BreakGlassError, BreakGlassEvent, CustomerView, FrameKind, Principal, PrincipalKind,
    SessionSummary, hex, replay_bytes,
};
use spot_core::Clock;

use super::ports::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Over {
    Ended,
    Expired,
    Revoked,
}

#[derive(Clone, Debug)]
struct Entry {
    window: Window,
    over: Option<Over>,
    kill_pending: bool,
}

#[derive(Default)]
struct Inner {
    loaded: bool,
    entries: BTreeMap<String, Entry>,
    outbox: Vec<BreakGlassEvent>,
    last_retention: i64,
}

/// The manifest beside a sealed recording: the digest and the facts, no content.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct ArchiveManifest {
    tenant: String,
    session: String,
    created_at: i64,
    digest: String,
    frames: u64,
    truncated: bool,
    bytes: u64,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn window_key(session: &str) -> String {
    format!("registry/{session}.json")
}
fn rec_key(session: &str) -> String {
    format!("recordings/{session}.rec")
}
fn manifest_key(session: &str) -> String {
    format!("recordings/{session}.manifest")
}

/// The tenant-side registry and archive of one site.
pub struct BreakGlassHost {
    tenant: String,
    clock: Arc<dyn Clock>,
    parts: Parts,
    settings: Settings,
    inner: Mutex<Inner>,
}

impl BreakGlassHost {
    /// A host for `tenant`. Nothing is read until [`BreakGlassHost::load`].
    pub fn new(tenant: &str, clock: Arc<dyn Clock>, parts: Parts, settings: Settings) -> Self {
        Self {
            tenant: tenant.to_owned(),
            clock,
            parts,
            settings,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn now(&self) -> i64 {
        self.clock.now_utc().unix_timestamp()
    }

    fn state(&self) -> MutexGuard<'_, Inner> {
        // The guarded data is plain; a panic elsewhere cannot leave it half-updated.
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// The names of the required parts that are not wired.
    pub fn missing(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.parts.log.is_none() {
            out.push("event_log");
        }
        if self.parts.store.is_none() {
            out.push("object_store");
        }
        if self.parts.sealer.is_none() {
            out.push("sealer");
        }
        if self.parts.duties.is_none() {
            out.push("duties");
        }
        out
    }

    fn log(&self) -> std::result::Result<&Arc<dyn EventLog>, HostError> {
        self.parts
            .log
            .as_ref()
            .ok_or(HostError::NotWired("event_log"))
    }
    fn store(&self) -> std::result::Result<&Arc<dyn ObjectStore>, HostError> {
        self.parts
            .store
            .as_ref()
            .ok_or(HostError::NotWired("object_store"))
    }
    fn sealer(&self) -> std::result::Result<&Arc<dyn Sealer>, HostError> {
        self.parts
            .sealer
            .as_ref()
            .ok_or(HostError::NotWired("sealer"))
    }

    fn require_loaded(&self) -> std::result::Result<(), HostError> {
        if self.state().loaded {
            Ok(())
        } else {
            Err(HostError::NotWired("registry"))
        }
    }

    /// Fail closed: no duty check, an error from it, or a "no" are all a refusal.
    async fn require(
        &self,
        caller: &Caller,
        duties: &[&str],
    ) -> std::result::Result<(), HostError> {
        if caller.tenant != self.tenant {
            return Err(HostError::Denied);
        }
        let Some(check) = self.parts.duties.as_ref() else {
            return Err(HostError::Denied);
        };
        for duty in duties {
            match check.holds(&self.tenant, &caller.user, duty).await {
                Ok(true) => return Ok(()),
                Ok(false) => {}
                Err(_) => return Err(HostError::Denied),
            }
        }
        Err(HostError::Denied)
    }

    /// The customer's view, rebuilt from the chain.
    async fn view(&self) -> std::result::Result<CustomerView, HostError> {
        let entries = self.log()?.entries().await?;
        CustomerView::collect(&entries).map_err(HostError::from)
    }

    /// Reads the windows and the chain into the registry. Called by the start-up check.
    ///
    /// # Errors
    ///
    /// A part that is not wired, or a store or chain that cannot be read.
    pub async fn load(&self) -> std::result::Result<(), HostError> {
        let store = self.store()?;
        let view = self.view().await?;
        let mut entries = BTreeMap::new();
        for key in store.list("registry/").await? {
            let Some(bytes) = store.get(&key).await? else {
                continue;
            };
            let Ok(window) = serde_json::from_slice::<Window>(&bytes) else {
                continue;
            };
            if window.tenant != self.tenant || !valid_id(&window.session) {
                continue;
            }
            let over = view
                .session(&window.session)
                .and_then(|s| match s.state.as_str() {
                    "ended" => Some(Over::Ended),
                    "expired" => Some(Over::Expired),
                    "revoked" => Some(Over::Revoked),
                    _ => None,
                });
            entries.insert(
                window.session.clone(),
                Entry {
                    window,
                    over,
                    kill_pending: false,
                },
            );
        }
        let mut g = self.state();
        g.entries = entries;
        g.loaded = true;
        Ok(())
    }

    /// Writes `event`; a failure queues it for the sweep. Returns whether it is on the chain.
    async fn emit(&self, event: BreakGlassEvent) -> bool {
        let written = match self.log() {
            Ok(log) => log.append(event.clone()).await.is_ok(),
            Err(_) => false,
        };
        if !written {
            self.state().outbox.push(event);
        }
        written
    }

    async fn deliver_kill(&self, session: &str) -> bool {
        let Some(bridge) = self.parts.bridge.as_ref() else {
            return false;
        };
        if bridge.kill(&self.tenant, session).await.is_err() {
            return false;
        }
        if let Some(entry) = self.state().entries.get_mut(session) {
            entry.kill_pending = false;
        }
        true
    }

    /// See [`BreakGlassPort::register`].
    pub async fn register(&self, window: Window) -> std::result::Result<(), HostError> {
        if window.tenant != self.tenant {
            return Err(HostError::Denied);
        }
        if !valid_id(&window.session) {
            return Err(HostError::Invalid("the session id is not valid"));
        }
        if window.requester.is_empty() || window.requester.len() > 256 {
            return Err(HostError::Invalid("the requester is out of bounds"));
        }
        if window.starts_at >= window.expires_at {
            return Err(HostError::Invalid("the window is not a valid range"));
        }
        self.require_loaded()?;
        let store = self.store()?;
        let key = window_key(&window.session);
        let bytes = serde_json::to_vec(&window).map_err(|_| HostError::Store)?;
        match store.put_new(&key, bytes).await {
            Ok(()) => {}
            Err(HostError::Crate(BreakGlassError::AppendOnly)) => {
                // The same window again is fine; another window under the same id is not.
                let existing = store
                    .get(&key)
                    .await?
                    .and_then(|b| serde_json::from_slice::<Window>(&b).ok());
                if existing.as_ref() != Some(&window) {
                    return Err(HostError::Invalid(
                        "the session is already registered with another window",
                    ));
                }
            }
            Err(e) => return Err(e),
        }
        self.state()
            .entries
            .entry(window.session.clone())
            .or_insert(Entry {
                window,
                over: None,
                kill_pending: false,
            });
        Ok(())
    }

    /// See [`BreakGlassPort::authorise`].
    pub async fn authorise(&self, session: &str, now: i64) -> std::result::Result<(), HostError> {
        self.require_loaded()?;
        if !valid_id(session) {
            return Err(HostError::NotFound);
        }
        let known = self.state().entries.contains_key(session);
        if !known {
            self.refresh_one(session).await?;
        }
        let g = self.state();
        let entry = g.entries.get(session).ok_or(HostError::NotFound)?;
        match entry.over {
            Some(Over::Revoked) => return Err(HostError::Crate(BreakGlassError::Revoked)),
            Some(Over::Expired) => return Err(HostError::Crate(BreakGlassError::Expired)),
            Some(Over::Ended) => {
                return Err(HostError::Crate(BreakGlassError::WrongState(
                    "the session is over",
                )));
            }
            None => {}
        }
        if now < entry.window.starts_at {
            Err(HostError::Crate(BreakGlassError::NotStarted))
        } else if now >= entry.window.expires_at {
            Err(HostError::Crate(BreakGlassError::Expired))
        } else {
            Ok(())
        }
    }

    /// A session registered by another node: read its window (and whether the chain shows it
    /// over) once, then keep it.
    async fn refresh_one(&self, session: &str) -> std::result::Result<(), HostError> {
        let Some(bytes) = self.store()?.get(&window_key(session)).await? else {
            return Ok(());
        };
        let Ok(window) = serde_json::from_slice::<Window>(&bytes) else {
            return Ok(());
        };
        if window.tenant != self.tenant || window.session != session {
            return Ok(());
        }
        let over = self
            .view()
            .await?
            .session(session)
            .and_then(|s| match s.state.as_str() {
                "ended" => Some(Over::Ended),
                "expired" => Some(Over::Expired),
                "revoked" => Some(Over::Revoked),
                _ => None,
            });
        self.state()
            .entries
            .entry(session.to_owned())
            .or_insert(Entry {
                window,
                over,
                kill_pending: false,
            });
        Ok(())
    }

    /// See [`BreakGlassPort::record_event`].
    pub async fn record_event(&self, event: BreakGlassEvent) -> std::result::Result<(), HostError> {
        if event.tenant != self.tenant {
            return Err(HostError::Denied);
        }
        let over = match event.kind.as_str() {
            ENDED => Some(Over::Ended),
            EXPIRED => Some(Over::Expired),
            REVOKED => Some(Over::Revoked),
            _ => None,
        };
        if let Some(over) = over {
            // Close first: if the chain is down the session must still stop acting.
            if let Some(entry) = self.state().entries.get_mut(&event.session) {
                entry.over.get_or_insert(over);
            }
        }
        self.log()?.append(event).await
    }

    /// See [`BreakGlassPort::list`].
    pub async fn list(
        &self,
        caller: &Caller,
        page: Page,
    ) -> std::result::Result<Listing, HostError> {
        self.require(caller, &[ADMIN_DUTY, AUDITOR_DUTY]).await?;
        if page.limit == 0 {
            return Err(HostError::Invalid("the page size must be at least 1"));
        }
        if page.after.as_deref().is_some_and(|c| !valid_id(c)) {
            return Err(HostError::Invalid("the cursor is not valid"));
        }
        let limit = page.limit.min(self.settings.page_max);
        let view = self.view().await?;
        let all: Vec<&SessionSummary> = view
            .sessions()
            .iter()
            .rev()
            .filter(|s| s.tenant == self.tenant)
            .collect();
        let start = match &page.after {
            None => 0,
            Some(after) => all
                .iter()
                .position(|s| &s.session == after)
                .map(|i| i + 1)
                .ok_or(HostError::Invalid("the cursor is not valid"))?,
        };
        let rows: Vec<SessionSummary> = all
            .iter()
            .skip(start)
            .take(limit)
            .map(|s| (*s).clone())
            .collect();
        let more = start + rows.len() < all.len();
        let next = if more {
            rows.last().map(|s| s.session.clone())
        } else {
            None
        };
        Ok(Listing { rows, next })
    }

    /// See [`BreakGlassPort::summary`].
    pub async fn summary(
        &self,
        caller: &Caller,
        session: &str,
    ) -> std::result::Result<SessionSummary, HostError> {
        self.require(caller, &[ADMIN_DUTY, AUDITOR_DUTY]).await?;
        let view = self.view().await?;
        view.session(session)
            .filter(|s| s.tenant == self.tenant)
            .cloned()
            .ok_or(HostError::NotFound)
    }

    /// See [`BreakGlassPort::archive_recording`].
    pub async fn archive_recording(
        &self,
        session: &str,
        bytes: Vec<u8>,
    ) -> std::result::Result<String, HostError> {
        self.require_loaded()?;
        if !valid_id(session) {
            return Err(HostError::NotFound);
        }
        if !self.state().entries.contains_key(session) {
            return Err(HostError::NotFound);
        }
        if bytes.is_empty() || bytes.len() as u64 > self.settings.max_recording_bytes {
            return Err(HostError::Invalid(
                "the recording is empty or over the size limit",
            ));
        }
        let replay = replay_bytes(&bytes);
        if !replay.verified() {
            return Err(HostError::Crate(BreakGlassError::RecordingTampered {
                frame: replay.frames().len() as u64 + 1,
            }));
        }
        let digest = hex(&replay.digest());
        let commands = replay
            .frames()
            .iter()
            .filter(|f| f.kind == FrameKind::Input)
            .count() as u64;
        let manifest = ArchiveManifest {
            tenant: self.tenant.clone(),
            session: session.to_owned(),
            created_at: self.now(),
            digest: digest.clone(),
            frames: replay.frames().len() as u64,
            truncated: replay.truncated(),
            bytes: bytes.len() as u64,
        };
        let sealed = self.sealer()?.seal(&self.seal_context(session), &bytes)?;
        let store = self.store()?;
        // Once, never overwritten: a second recording under the same id is refused.
        store.put_new(&rec_key(session), sealed).await?;
        let manifest_bytes = serde_json::to_vec(&manifest).map_err(|_| HostError::Store)?;
        store
            .put_new(&manifest_key(session), manifest_bytes)
            .await?;
        let digest_bytes: [u8; 32] = replay.digest();
        let event = BreakGlassEvent::new(
            RECORDING,
            session,
            &self.tenant,
            "system",
            "recording archived",
            BreakGlassError::AppendOnly.code(),
            self.now(),
        )
        .with_recording(digest_bytes, commands);
        // A digest that is not on the chain yet is retried by the sweep; reads need the chain.
        self.emit(event).await;
        Ok(digest)
    }

    fn seal_context(&self, session: &str) -> String {
        format!("break_glass:{}:{session}", self.tenant)
    }

    /// See [`BreakGlassPort::recording`].
    pub async fn recording(
        &self,
        caller: &Caller,
        session: &str,
    ) -> std::result::Result<RecordingView, HostError> {
        self.require(caller, &[ADMIN_DUTY]).await?;
        if !valid_id(session) {
            return Err(HostError::NotFound);
        }
        let view = self.view().await?;
        let summary = view
            .session(session)
            .filter(|s| s.tenant == self.tenant)
            .cloned()
            .ok_or(HostError::NotFound)?;
        let chain_digest = summary
            .recording_digest
            .clone()
            .ok_or(HostError::Crate(BreakGlassError::RecordingNotFound))?;
        let sealed = self
            .store()?
            .get(&rec_key(session))
            .await?
            .ok_or(HostError::Crate(BreakGlassError::RecordingNotFound))?;
        let plain = self.sealer()?.open(&self.seal_context(session), &sealed)?;
        let replay = replay_bytes(&plain);
        if !replay.verified() {
            return Err(HostError::Crate(BreakGlassError::RecordingTampered {
                frame: replay.frames().len() as u64 + 1,
            }));
        }
        // The chain is the customer's: the object must hash to what the chain recorded.
        if hex(&replay.digest()) != chain_digest {
            return Err(HostError::Crate(BreakGlassError::RecordingTampered {
                frame: 0,
            }));
        }
        // The read is audited before the content leaves; no event, no content.
        let read = BreakGlassEvent::new(
            READ_KIND,
            session,
            &self.tenant,
            &caller.user,
            "recording read",
            BreakGlassError::AppendOnly.code(),
            self.now(),
        );
        self.log()?
            .append(read)
            .await
            .map_err(|_| HostError::Crate(BreakGlassError::AuditUnavailable))?;
        Ok(RecordingView {
            summary,
            digest: chain_digest,
            truncated: replay.truncated(),
            frames: replay.frames().to_vec(),
        })
    }

    /// Recordings are append-only: nobody deletes one through this module. A vendor-side
    /// caller is told so explicitly.
    ///
    /// # Errors
    ///
    /// Always: `VendorDeleteRefused` for a vendor principal, `AppendOnly` otherwise.
    pub fn erase(
        &self,
        principal: &Principal,
        _session: &str,
    ) -> std::result::Result<(), HostError> {
        Err(HostError::Crate(
            if principal.kind == PrincipalKind::Vendor {
                BreakGlassError::VendorDeleteRefused
            } else {
                BreakGlassError::AppendOnly
            },
        ))
    }

    /// See [`BreakGlassPort::revoke`].
    pub async fn revoke(
        &self,
        caller: &Caller,
        session: &str,
    ) -> std::result::Result<RevokeOutcome, HostError> {
        self.require(caller, &[ADMIN_DUTY]).await?;
        self.require_loaded()?;
        if !valid_id(session) {
            return Err(HostError::NotFound);
        }
        let now = self.now();
        // Close first. From here `authorise` refuses, whatever else fails.
        let fresh = {
            let mut g = self.state();
            let entry = g.entries.get_mut(session).ok_or(HostError::NotFound)?;
            match entry.over {
                Some(Over::Revoked) => false,
                Some(_) => {
                    return Err(HostError::Crate(BreakGlassError::WrongState(
                        "the session is already over; revocation is final",
                    )));
                }
                None => {
                    entry.over = Some(Over::Revoked);
                    entry.kill_pending = true;
                    true
                }
            }
        };
        let audited = if fresh {
            let event = BreakGlassEvent::new(
                REVOKED,
                session,
                &self.tenant,
                &caller.user,
                "revoked by the customer",
                BreakGlassError::Revoked.code(),
                now,
            );
            self.emit(event).await
        } else {
            self.state().outbox.is_empty()
        };
        let delivered = self.deliver_kill(session).await;
        Ok(RevokeOutcome { delivered, audited })
    }

    /// One sweep at the host clock's time.
    pub async fn sweep(&self) -> SweepReport {
        self.sweep_at(self.now()).await
    }

    /// One sweep: flush queued events, end the sessions past their window, retry kills, and
    /// apply retention. Never fails: what cannot be done now stays queued.
    pub async fn sweep_at(&self, now: i64) -> SweepReport {
        let mut report = SweepReport::default();
        if !self.state().loaded {
            return report;
        }
        // Events first, so a revoke that could not be written lands before anything else.
        let queued = std::mem::take(&mut self.state().outbox);
        for event in queued {
            let written = match self.log() {
                Ok(log) => log.append(event.clone()).await.is_ok(),
                Err(_) => false,
            };
            if written {
                report.flushed += 1;
            } else {
                self.state().outbox.push(event);
            }
        }
        // Expiry: even when the operator side never reports an end.
        let due: Vec<String> = self
            .state()
            .entries
            .iter()
            .filter(|(_, e)| e.over.is_none() && now >= e.window.expires_at)
            .map(|(id, _)| id.clone())
            .collect();
        for session in due {
            if let Some(entry) = self.state().entries.get_mut(&session) {
                entry.over = Some(Over::Expired);
                entry.kill_pending = true;
            }
            let event = BreakGlassEvent::new(
                EXPIRED,
                &session,
                &self.tenant,
                "system",
                "window closed",
                BreakGlassError::Expired.code(),
                now,
            );
            self.emit(event).await;
            report.expired += 1;
        }
        let pending: Vec<String> = self
            .state()
            .entries
            .iter()
            .filter(|(_, e)| e.kill_pending)
            .map(|(id, _)| id.clone())
            .collect();
        for session in pending {
            if self.deliver_kill(&session).await {
                report.kills += 1;
            }
        }
        report.retired = self.retire_expired(now).await;
        report
    }

    /// Removes recordings past the retention, at most hourly, never one of a live session.
    async fn retire_expired(&self, now: i64) -> usize {
        {
            let mut g = self.state();
            if now - g.last_retention < 3600 {
                return 0;
            }
            g.last_retention = now;
        }
        let (Some(retire), Ok(store)) = (self.parts.retire.as_ref(), self.store()) else {
            return 0;
        };
        let days = self
            .parts
            .retention
            .as_ref()
            .and_then(|r| r.recording_days(&self.tenant))
            .unwrap_or(self.settings.retention_days)
            .clamp(1, 3650);
        let keep_secs = i64::from(days) * 86_400;
        let Ok(keys) = store.list("recordings/").await else {
            return 0;
        };
        let mut removed = 0;
        for key in keys.iter().filter(|k| k.ends_with(".manifest")) {
            let Ok(Some(bytes)) = store.get(key).await else {
                continue;
            };
            let Ok(manifest) = serde_json::from_slice::<ArchiveManifest>(&bytes) else {
                continue;
            };
            if manifest.tenant != self.tenant
                || !valid_id(&manifest.session)
                || now - manifest.created_at < keep_secs
            {
                continue;
            }
            let live = self
                .state()
                .entries
                .get(&manifest.session)
                .is_some_and(|e| e.over.is_none());
            if live {
                continue;
            }
            let rec = retire.remove(&rec_key(&manifest.session)).await;
            let man = retire.remove(&manifest_key(&manifest.session)).await;
            if rec.is_ok() && man.is_ok() {
                removed += 1;
            }
        }
        removed
    }
}

macro_rules! boxed {
    ($self:ident, $call:expr) => {
        Box::pin($call)
    };
}

impl BreakGlassPort for BreakGlassHost {
    fn register<'a>(&'a self, window: Window) -> BoxFuture<'a, std::result::Result<(), HostError>> {
        boxed!(self, BreakGlassHost::register(self, window))
    }
    fn authorise<'a>(
        &'a self,
        session: &'a str,
        now: i64,
    ) -> BoxFuture<'a, std::result::Result<(), HostError>> {
        boxed!(self, BreakGlassHost::authorise(self, session, now))
    }
    fn record_event<'a>(
        &'a self,
        event: BreakGlassEvent,
    ) -> BoxFuture<'a, std::result::Result<(), HostError>> {
        boxed!(self, BreakGlassHost::record_event(self, event))
    }
    fn archive_recording<'a>(
        &'a self,
        session: &'a str,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, std::result::Result<String, HostError>> {
        boxed!(
            self,
            BreakGlassHost::archive_recording(self, session, bytes)
        )
    }
    fn list<'a>(
        &'a self,
        caller: &'a Caller,
        page: Page,
    ) -> BoxFuture<'a, std::result::Result<Listing, HostError>> {
        boxed!(self, BreakGlassHost::list(self, caller, page))
    }
    fn summary<'a>(
        &'a self,
        caller: &'a Caller,
        session: &'a str,
    ) -> BoxFuture<'a, std::result::Result<SessionSummary, HostError>> {
        boxed!(self, BreakGlassHost::summary(self, caller, session))
    }
    fn recording<'a>(
        &'a self,
        caller: &'a Caller,
        session: &'a str,
    ) -> BoxFuture<'a, std::result::Result<RecordingView, HostError>> {
        boxed!(self, BreakGlassHost::recording(self, caller, session))
    }
    fn revoke<'a>(
        &'a self,
        caller: &'a Caller,
        session: &'a str,
    ) -> BoxFuture<'a, std::result::Result<RevokeOutcome, HostError>> {
        boxed!(self, BreakGlassHost::revoke(self, caller, session))
    }
}
