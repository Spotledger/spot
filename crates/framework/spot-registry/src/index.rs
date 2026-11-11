use crate::{
    AppName, Caller, Digest, Ownership, Publisher, PublisherStatus, RegistryError, Release,
    Removal, Result, SearchPage, SearchQuery, TransferRecord, Version, Visibility,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Mutex,
};
/// Atomic publish result.
#[derive(Clone, Debug)]
pub struct PublishOutcome {
    /// Stored release, including existing metadata on idempotent success.
    pub release: Release,
    /// Whether this transaction created the row (controls audit emission).
    pub inserted: bool,
}
/// Index seam. Implementations must atomically enforce ownership/status, version and
/// transfer-generation checks in mutation methods, including across registry instances.
/// The raw index is a trusted backend; expose Registry/RegistryClient to untrusted callers.
pub trait IndexStore: Send + Sync {
    /// Checks ownership before version conflicts; does not claim a free name.
    fn preflight(&self, release: &Release) -> Result<()>;
    /// Repeats preflight and atomically claims ownership, records publisher and release.
    /// Registry calls only after durable blob storage. Failure may leave an orphan blob.
    fn commit(&self, release: Release, publisher: Publisher) -> Result<PublishOutcome>;
    /// Current ownership.
    fn ownership(&self, app: &AppName) -> Result<Option<Ownership>>;
    /// Publisher administrative state.
    fn publisher(&self, id: &str) -> Result<Option<Publisher>>;
    /// Sets administrative state; service operator API, not publisher API.
    fn set_publisher_status(&self, id: &str, status: PublisherStatus) -> Result<()>;
    /// Visible releases, newest first. Inactive requests require verified caller identity.
    fn releases(
        &self,
        app: &AppName,
        visibility: &Visibility,
        caller: Option<&Caller>,
    ) -> Result<Vec<Release>>;
    /// Latest available release; pre-releases excluded unless requested.
    fn latest(&self, app: &AppName, include_pre: bool) -> Result<Option<Release>> {
        Ok(self
            .releases(
                app,
                &Visibility {
                    include_pre,
                    include_inactive: false,
                },
                None,
            )?
            .into_iter()
            .next())
    }
    /// Raw release lookup, for digest fetch and repair; may include inactive rows.
    fn by_digest(&self, digest: Digest) -> Result<Option<Release>>;
    /// Raw identity lookup, for verified fetch and authenticated mutations.
    fn release(&self, app: &AppName, version: &Version) -> Result<Option<Release>>;
    /// Ranked catalogue search.
    fn search(&self, query: &SearchQuery) -> Result<SearchPage>;
    /// Atomically checks current ownership and marks a release. Returns None when already
    /// marked; does not overwrite the original reason or create a duplicate audit event.
    fn remove(
        &self,
        app: &AppName,
        version: &Version,
        by: &str,
        removal: Removal,
        withdraw: bool,
    ) -> Result<Option<Release>>;
    /// CAS transfer from exactly the signed ownership generation; retains dual evidence.
    fn transfer(&self, transfer: TransferRecord, recipient: Publisher) -> Result<()>;
    /// Transfer history, in mutation order.
    fn transfers(&self, app: &AppName) -> Result<Vec<TransferRecord>>;
    /// All indexed addresses, including yanked/withdrawn rows.
    fn digests(&self) -> Result<Vec<Digest>>;
    /// Atomically curates catalogue-only fields for all releases of an owned app.
    fn describe(
        &self,
        app: &AppName,
        by: &str,
        description: String,
        category: String,
    ) -> Result<()>;
}
#[derive(Default)]
struct State {
    releases: BTreeMap<AppName, Vec<Release>>,
    owners: BTreeMap<AppName, Ownership>,
    publishers: HashMap<String, Publisher>,
    transfers: Vec<TransferRecord>,
    revision: u64,
}
impl State {
    fn owner(&self, app: &AppName, by: &str) -> Result<()> {
        if self.owners.get(app).is_none_or(|o| o.publisher != by) {
            return Err(RegistryError::new(
                "registry.not_owner",
                "publisher does not own app",
            ));
        }
        self.active(by)
    }
    fn active(&self, by: &str) -> Result<()> {
        if self
            .publishers
            .get(by)
            .is_some_and(|p| p.status != PublisherStatus::Active)
        {
            return Err(RegistryError::new(
                "registry.publisher_inactive",
                "publisher suspended",
            ));
        }
        Ok(())
    }
    fn check(&self, r: &Release) -> Result<()> {
        if let Some(o) = self.owners.get(&r.app)
            && o.publisher != r.publisher
        {
            return Err(RegistryError::new(
                "registry.not_owner",
                "app belongs to another publisher",
            ));
        }
        self.active(&r.publisher)?;
        if let Some(rows) = self.releases.get(&r.app) {
            if let Some(old) = rows
                .iter()
                .find(|old| old.version.cmp_precedence(&r.version).is_eq())
            {
                if old.digest != r.digest {
                    return Err(RegistryError::new(
                        "registry.version_conflict",
                        "version already has another digest",
                    ));
                }
                if old.version != r.version {
                    return Err(RegistryError::new(
                        "registry.version_conflict",
                        "build metadata does not create a new version",
                    ));
                }
                return Ok(());
            }
            if rows
                .iter()
                .any(|old| old.version.cmp_precedence(&r.version).is_ge())
            {
                return Err(RegistryError::new(
                    "registry.version_not_increasing",
                    "version must exceed every published version",
                ));
            }
        }
        Ok(())
    }
}
/// Thread-safe atomic in-memory index. Share via Arc between Registry instances.
#[derive(Default)]
pub struct MemoryIndexStore {
    state: Mutex<State>,
}
impl IndexStore for MemoryIndexStore {
    fn preflight(&self, release: &Release) -> Result<()> {
        self.state
            .lock()
            .map_err(|_| crate::error::poisoned())?
            .check(release)
    }
    fn commit(&self, mut release: Release, publisher: Publisher) -> Result<PublishOutcome> {
        let mut s = self.state.lock().map_err(|_| crate::error::poisoned())?;
        s.check(&release)?;
        if release.publisher != publisher.id || publisher.status != PublisherStatus::Active {
            return Err(RegistryError::new(
                "registry.publisher_inactive",
                "publisher metadata mismatch",
            ));
        }
        if let Some(old) = s
            .releases
            .get(&release.app)
            .and_then(|rows| rows.iter().find(|old| old.version == release.version))
        {
            return Ok(PublishOutcome {
                release: old.clone(),
                inserted: false,
            });
        }
        if s.releases
            .values()
            .flatten()
            .any(|old| old.digest == release.digest)
        {
            return Err(RegistryError::new(
                "registry.version_conflict",
                "digest already indexed",
            ));
        }
        // Catalogue fields belong to the app, and survive new signed releases.
        if let Some(previous) = s.releases.get(&release.app).and_then(|rows| rows.last()) {
            release.description.clone_from(&previous.description);
            release.category.clone_from(&previous.category);
        }
        s.owners
            .entry(release.app.clone())
            .or_insert_with(|| Ownership {
                app: release.app.clone(),
                publisher: release.publisher.clone(),
                generation: 0,
            });
        s.publishers.insert(publisher.id.clone(), publisher);
        s.releases
            .entry(release.app.clone())
            .or_default()
            .push(release.clone());
        s.revision = s.revision.wrapping_add(1);
        Ok(PublishOutcome {
            release,
            inserted: true,
        })
    }
    fn ownership(&self, app: &AppName) -> Result<Option<Ownership>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| crate::error::poisoned())?
            .owners
            .get(app)
            .cloned())
    }
    fn publisher(&self, id: &str) -> Result<Option<Publisher>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| crate::error::poisoned())?
            .publishers
            .get(id)
            .cloned())
    }
    fn set_publisher_status(&self, id: &str, status: PublisherStatus) -> Result<()> {
        let mut s = self.state.lock().map_err(|_| crate::error::poisoned())?;
        s.publishers.get_mut(id).ok_or_else(missing)?.status = status;
        Ok(())
    }
    fn releases(
        &self,
        app: &AppName,
        visibility: &Visibility,
        caller: Option<&Caller>,
    ) -> Result<Vec<Release>> {
        if visibility.include_inactive && caller.is_none() {
            return Err(RegistryError::new(
                "registry.unauthorized",
                "inactive listings require a caller",
            ));
        }
        let s = self.state.lock().map_err(|_| crate::error::poisoned())?;
        let mut rows = s.releases.get(app).cloned().unwrap_or_default();
        rows.retain(|r| crate::search::authorized(r, visibility, caller));
        rows.sort_by(|a, b| b.version.cmp(&a.version));
        Ok(rows)
    }
    fn by_digest(&self, digest: Digest) -> Result<Option<Release>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| crate::error::poisoned())?
            .releases
            .values()
            .flatten()
            .find(|r| r.digest == digest)
            .cloned())
    }
    fn release(&self, app: &AppName, version: &Version) -> Result<Option<Release>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| crate::error::poisoned())?
            .releases
            .get(app)
            .and_then(|rows| rows.iter().find(|r| &r.version == version))
            .cloned())
    }
    fn search(&self, query: &SearchQuery) -> Result<SearchPage> {
        let s = self.state.lock().map_err(|_| crate::error::poisoned())?;
        crate::search::page(
            s.releases.values().flatten().cloned().collect(),
            query,
            s.revision,
        )
    }
    fn remove(
        &self,
        app: &AppName,
        version: &Version,
        by: &str,
        removal: Removal,
        withdraw: bool,
    ) -> Result<Option<Release>> {
        let mut s = self.state.lock().map_err(|_| crate::error::poisoned())?;
        s.owner(app, by)?;
        let release = s
            .releases
            .get_mut(app)
            .and_then(|rows| rows.iter_mut().find(|r| &r.version == version))
            .ok_or_else(missing)?;
        let marker = if withdraw {
            &mut release.withdrawn
        } else {
            &mut release.yanked
        };
        if marker.is_some() {
            return Ok(None);
        }
        *marker = Some(removal);
        let r = release.clone();
        s.revision = s.revision.wrapping_add(1);
        Ok(Some(r))
    }
    fn transfer(&self, transfer: TransferRecord, recipient: Publisher) -> Result<()> {
        let mut s = self.state.lock().map_err(|_| crate::error::poisoned())?;
        let request = &transfer.request;
        s.owner(&request.app, &request.from)?;
        s.active(&request.to)?;
        if recipient.id != request.to || recipient.status != PublisherStatus::Active {
            return Err(RegistryError::new(
                "registry.transfer_invalid",
                "recipient mismatch",
            ));
        }
        let owner = s.owners.get_mut(&request.app).ok_or_else(missing)?;
        if owner.generation != request.generation || request.from == request.to {
            return Err(RegistryError::new(
                "registry.transfer_replay",
                "stale ownership generation or self transfer",
            ));
        }
        owner.generation = owner.generation.checked_add(1).ok_or_else(|| {
            RegistryError::new("registry.transfer_replay", "ownership generation exhausted")
        })?;
        owner.publisher = request.to.clone();
        s.publishers.insert(recipient.id.clone(), recipient);
        s.transfers.push(transfer);
        s.revision = s.revision.wrapping_add(1);
        Ok(())
    }
    fn transfers(&self, app: &AppName) -> Result<Vec<TransferRecord>> {
        Ok(self
            .state
            .lock()
            .map_err(|_| crate::error::poisoned())?
            .transfers
            .iter()
            .filter(|t| &t.request.app == app)
            .cloned()
            .collect())
    }
    fn digests(&self) -> Result<Vec<Digest>> {
        let mut rows: Vec<_> = self
            .state
            .lock()
            .map_err(|_| crate::error::poisoned())?
            .releases
            .values()
            .flatten()
            .map(|r| r.digest)
            .collect();
        rows.sort_by_key(Digest::to_hex);
        rows.dedup();
        Ok(rows)
    }
    fn describe(
        &self,
        app: &AppName,
        by: &str,
        description: String,
        category: String,
    ) -> Result<()> {
        let mut s = self.state.lock().map_err(|_| crate::error::poisoned())?;
        s.owner(app, by)?;
        for r in s.releases.get_mut(app).ok_or_else(missing)? {
            r.description.clone_from(&description);
            r.category.clone_from(&category);
        }
        s.revision = s.revision.wrapping_add(1);
        Ok(())
    }
}
fn missing() -> RegistryError {
    RegistryError::new("registry.not_found", "index record absent")
}
