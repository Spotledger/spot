//! Shared builders for the planner tests: manifests from TOML, digests, hosts and installed apps.

#![allow(dead_code)]

use semver::{Version, VersionReq};
use spot_bundle::Digest;
use spot_installer::{Candidate, GrantedCapabilities, Host, InstalledApp, Verified};
use spot_manifest::AppManifest;

/// The WIT world the test host serves and the test candidates target.
pub const WORLD: &str = "spot:app@0.3.0";

/// A digest whose 32 bytes are all `byte`.
pub fn digest(byte: u8) -> Digest {
    format!("{byte:02x}")
        .repeat(32)
        .parse()
        .expect("64 hex characters")
}

/// Parses a version, panicking on a typo in a test.
pub fn ver(text: &str) -> Version {
    text.parse().expect("valid version")
}

/// Parses a version requirement, panicking on a typo in a test.
pub fn req(text: &str) -> VersionReq {
    text.parse().expect("valid requirement")
}

/// The host served at `version` and the test world.
pub fn host(version: &str) -> Host {
    Host::new(ver(version), WORLD)
}

/// A manifest spec that renders to `spot-app.toml` and parses.
#[derive(Clone, Default)]
pub struct Spec {
    /// App name.
    pub name: String,
    /// Human title.
    pub title: String,
    /// Version.
    pub version: String,
    /// `requires_host`, when set.
    pub host_req: Option<String>,
    /// Dependencies as `(app, range)`.
    pub depends: Vec<(String, String)>,
    /// Patches as `(id, intent, reversible)`, ids ascending.
    pub patches: Vec<(String, String, bool)>,
    /// Fixtures as `(doctype, file)`.
    pub fixtures: Vec<(String, String)>,
    /// Network endpoints.
    pub network: Vec<(String, u16)>,
    /// Whether e-mail is declared.
    pub email: bool,
    /// File access, when set.
    pub files: Option<String>,
    /// DocTypes readable.
    pub read: Vec<String>,
    /// DocTypes writable.
    pub write: Vec<String>,
}

impl Spec {
    /// A spec for `name` at `version`.
    pub fn new(name: &str, version: &str) -> Self {
        Self {
            name: name.to_owned(),
            version: version.to_owned(),
            ..Self::default()
        }
    }

    /// Sets the title.
    pub fn title(mut self, title: &str) -> Self {
        self.title = title.to_owned();
        self
    }

    /// Sets `requires_host`.
    pub fn host_req(mut self, range: &str) -> Self {
        self.host_req = Some(range.to_owned());
        self
    }

    /// Adds a dependency.
    pub fn dep(mut self, app: &str, range: &str) -> Self {
        self.depends.push((app.to_owned(), range.to_owned()));
        self
    }

    /// Adds a patch.
    pub fn patch(mut self, id: &str, intent: &str, reversible: bool) -> Self {
        self.patches
            .push((id.to_owned(), intent.to_owned(), reversible));
        self
    }

    /// Adds a fixture.
    pub fn fixture(mut self, doctype: &str, file: &str) -> Self {
        self.fixtures.push((doctype.to_owned(), file.to_owned()));
        self
    }

    /// Declares a network endpoint.
    pub fn network(mut self, host: &str, port: u16) -> Self {
        self.network.push((host.to_owned(), port));
        self
    }

    /// Declares e-mail.
    pub fn email(mut self) -> Self {
        self.email = true;
        self
    }

    /// Declares file access.
    pub fn files(mut self, access: &str) -> Self {
        self.files = Some(access.to_owned());
        self
    }

    /// Declares read access to a DocType.
    pub fn read(mut self, doctype: &str) -> Self {
        self.read.push(doctype.to_owned());
        self
    }

    /// Declares write access to a DocType.
    pub fn write(mut self, doctype: &str) -> Self {
        self.write.push(doctype.to_owned());
        self
    }

    /// Renders the spec as a `spot-app.toml`.
    pub fn toml(&self) -> String {
        let title = if self.title.is_empty() {
            self.name.clone()
        } else {
            self.title.clone()
        };
        let mut out = format!(
            "[app]\nname = \"{}\"\nversion = \"{}\"\ntitle = \"{}\"\n",
            self.name, self.version, title
        );
        if let Some(range) = &self.host_req {
            out.push_str(&format!("requires_host = \"{range}\"\n"));
        }
        if !self.depends.is_empty() {
            let items: Vec<String> = self
                .depends
                .iter()
                .map(|(app, range)| format!("{{ app = \"{app}\", version = \"{range}\" }}"))
                .collect();
            out.push_str(&format!("depends = [{}]\n", items.join(", ")));
        }
        for (id, intent, reversible) in &self.patches {
            out.push_str(&format!(
                "\n[[patches]]\nid = \"{id}\"\nfile = \"patches/{id}.toml\"\nintent = \"{intent}\"\nreversible = {reversible}\n"
            ));
        }
        for (doctype, file) in &self.fixtures {
            out.push_str(&format!(
                "\n[[fixtures]]\ndoctype = \"{doctype}\"\nfile = \"{file}\"\n"
            ));
        }
        let empty_caps = self.network.is_empty()
            && !self.email
            && self.files.is_none()
            && self.read.is_empty()
            && self.write.is_empty();
        if !empty_caps {
            out.push_str("\n[capabilities]\n");
            if !self.network.is_empty() {
                let items: Vec<String> = self
                    .network
                    .iter()
                    .map(|(host, port)| format!("\"{host}:{port}\""))
                    .collect();
                out.push_str(&format!("network = [{}]\n", items.join(", ")));
            }
            if self.email {
                out.push_str("email = true\n");
            }
            if let Some(access) = &self.files {
                out.push_str(&format!("files = \"{access}\"\n"));
            }
            if !self.read.is_empty() || !self.write.is_empty() {
                let read: Vec<String> = self.read.iter().map(|d| format!("\"{d}\"")).collect();
                let write: Vec<String> = self.write.iter().map(|d| format!("\"{d}\"")).collect();
                out.push_str(&format!(
                    "doctypes = {{ read = [{}], write = [{}] }}\n",
                    read.join(", "),
                    write.join(", ")
                ));
            }
        }
        out
    }

    /// The parsed manifest.
    pub fn manifest(&self) -> AppManifest {
        spot_manifest::parse(&self.toml(), "test.toml").expect("valid test manifest")
    }

    /// A verified candidate in the test world.
    pub fn candidate(&self, byte: u8) -> Candidate {
        Candidate::new(
            self.manifest(),
            digest(byte),
            Verified::trusted("acme", 7),
            WORLD,
        )
    }

    /// A candidate targeting another WIT world.
    pub fn candidate_world(&self, byte: u8, world: &str) -> Candidate {
        Candidate::new(
            self.manifest(),
            digest(byte),
            Verified::trusted("acme", 7),
            world,
        )
    }

    /// A candidate whose signature check by the caller failed.
    pub fn candidate_unverified(&self, byte: u8) -> Candidate {
        Candidate::new(self.manifest(), digest(byte), Verified::refused(), WORLD)
    }
}

/// An installed app matching a spec: version, digest, applied patches and dependencies.
pub fn installed_from(spec: &Spec, byte: u8) -> InstalledApp {
    InstalledApp {
        name: spec.name.clone(),
        version: ver(&spec.version),
        digest: digest(byte),
        approved: GrantedCapabilities::default(),
        applied_patches: spec.patches.iter().map(|(id, _, _)| id.clone()).collect(),
        order: 0,
        depends: spec.manifest().app.depends.clone(),
    }
}

/// An installed app with just a name, version and digest.
pub fn installed(name: &str, version: &str, byte: u8) -> InstalledApp {
    InstalledApp::new(name, ver(version), digest(byte))
}

/// Capabilities granting exactly what a spec declares, so no delta remains.
pub fn fully_approved(spec: &Spec) -> GrantedCapabilities {
    let caps = spec.manifest().capabilities;
    GrantedCapabilities {
        network: caps.network,
        email: caps.email,
        files: caps.files,
        doctypes: caps.doctypes,
    }
}

// ---------------------------------------------------------------------------------------------
// Executor harness (WP-STR-04.T2): the fakes, an in-process coordinator and a manual clock.
// ---------------------------------------------------------------------------------------------

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use spot_coordinator::{
    BoxFuture, Coordinator, InMemoryPresence, InProcessCoordinator, Lease, NodeId, RateLimit,
};
use spot_core::{ManualClock, SiteId};
use spot_installer::Executor;
use spot_installer::testing::{
    FakeComponents, FakeFixtures, FakePatches, FakeRegistry, FakeSnapshots, Schedule,
};

/// Everything an executor test needs, wired together.
pub struct Harness {
    /// The registry fake.
    pub registry: Arc<FakeRegistry>,
    /// The patch runner fake.
    pub patches: Arc<FakePatches>,
    /// The component host fake.
    pub components: Arc<FakeComponents>,
    /// The fixture loader fake.
    pub fixtures: Arc<FakeFixtures>,
    /// The snapshotter fake.
    pub snapshots: Arc<FakeSnapshots>,
    /// The failure schedule every fake shares.
    pub schedule: Schedule,
    /// The manual clock the executor and coordinator read.
    pub clock: Arc<ManualClock>,
    /// The in-process coordinator holding the per-site leases.
    pub coordinator: Arc<InProcessCoordinator>,
    /// The executor under test, over the in-process coordinator.
    pub executor: Executor,
}

impl Harness {
    /// Builds a harness with fresh fakes, an in-process coordinator and a clock at the epoch.
    pub async fn new() -> Self {
        let schedule = Schedule::new();
        let registry = Arc::new(FakeRegistry::new(schedule.clone()));
        let patches = Arc::new(FakePatches::new(schedule.clone()));
        let components = Arc::new(FakeComponents::new(schedule.clone()));
        let fixtures = Arc::new(FakeFixtures::new(schedule.clone()));
        let snapshots = Arc::new(FakeSnapshots::new(schedule.clone()));
        let clock = Arc::new(ManualClock::new(time::OffsetDateTime::UNIX_EPOCH));
        let coordinator = InProcessCoordinator::start(
            Arc::new(InMemoryPresence::new()),
            NodeId::new("installer-test").expect("a constant node id is valid"),
            clock.clone(),
        )
        .await
        .expect("the in-process coordinator starts alone");
        let executor = executor_over(
            &registry,
            &patches,
            &components,
            &fixtures,
            &snapshots,
            coordinator.clone(),
            clock.clone(),
        );
        Self {
            registry,
            patches,
            components,
            fixtures,
            snapshots,
            schedule,
            clock,
            coordinator,
            executor,
        }
    }

    /// The same fakes, but over a different coordinator (for the fence cases).
    pub fn executor_with(&self, coordinator: Arc<dyn Coordinator>) -> Executor {
        executor_over(
            &self.registry,
            &self.patches,
            &self.components,
            &self.fixtures,
            &self.snapshots,
            coordinator,
            self.clock.clone(),
        )
    }

    /// A site id for a test.
    pub fn site(name: &str) -> SiteId {
        SiteId::new(name).expect("a test site id is valid")
    }
}

/// Builds an executor over the fakes and a coordinator.
fn executor_over(
    registry: &Arc<FakeRegistry>,
    patches: &Arc<FakePatches>,
    components: &Arc<FakeComponents>,
    fixtures: &Arc<FakeFixtures>,
    snapshots: &Arc<FakeSnapshots>,
    coordinator: Arc<dyn Coordinator>,
    clock: Arc<ManualClock>,
) -> Executor {
    Executor::new(
        registry.clone(),
        patches.clone(),
        components.clone(),
        fixtures.clone(),
        snapshots.clone(),
        coordinator,
        clock,
    )
}

/// A coordinator that delegates to another and deliberately breaks the fence after a number of
/// `check_fence` calls, simulating a lease lost at a chosen journal write.
pub struct FenceBreaker {
    inner: Arc<dyn Coordinator>,
    allowed: usize,
    checks: AtomicUsize,
}

impl FenceBreaker {
    /// Delegates to `inner` but fails `check_fence` from the `allowed`-th call on.
    pub fn new(inner: Arc<dyn Coordinator>, allowed: usize) -> Self {
        Self {
            inner,
            allowed,
            checks: AtomicUsize::new(0),
        }
    }
}

impl Coordinator for FenceBreaker {
    fn acquire<'a>(
        &'a self,
        name: &'a str,
        holder: &'a str,
        ttl: Duration,
    ) -> BoxFuture<'a, spot_coordinator::Result<Lease>> {
        self.inner.acquire(name, holder, ttl)
    }

    fn renew<'a>(
        &'a self,
        lease: &'a Lease,
        ttl: Duration,
    ) -> BoxFuture<'a, spot_coordinator::Result<Lease>> {
        self.inner.renew(lease, ttl)
    }

    fn release<'a>(&'a self, lease: &'a Lease) -> BoxFuture<'a, spot_coordinator::Result<()>> {
        self.inner.release(lease)
    }

    fn current<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, spot_coordinator::Result<Option<Lease>>> {
        self.inner.current(name)
    }

    fn check_fence<'a>(
        &'a self,
        name: &'a str,
        token: spot_coordinator::FencingToken,
    ) -> BoxFuture<'a, spot_coordinator::Result<()>> {
        let seen = self.checks.fetch_add(1, Ordering::SeqCst);
        if seen >= self.allowed {
            let message = format!("fence {name} is deliberately broken");
            return Box::pin(async move {
                Err(spot_coordinator::Error::new(
                    spot_coordinator::error::LEASE_LOST,
                    message,
                ))
            });
        }
        self.inner.check_fence(name, token)
    }

    fn next<'a>(&'a self, name: &'a str, by: u64) -> BoxFuture<'a, spot_coordinator::Result<u64>> {
        self.inner.next(name, by)
    }

    fn peek<'a>(&'a self, name: &'a str) -> BoxFuture<'a, spot_coordinator::Result<u64>> {
        self.inner.peek(name)
    }

    fn set_at_least<'a>(
        &'a self,
        name: &'a str,
        value: u64,
    ) -> BoxFuture<'a, spot_coordinator::Result<u64>> {
        self.inner.set_at_least(name, value)
    }

    fn take<'a>(
        &'a self,
        key: &'a str,
        limit: RateLimit,
        cost: u64,
    ) -> BoxFuture<'a, spot_coordinator::Result<spot_coordinator::Admit>> {
        self.inner.take(key, limit, cost)
    }

    fn waker(&self) -> Arc<tokio::sync::Notify> {
        self.inner.waker()
    }

    fn release_now(&self, lease: &Lease) -> bool {
        self.inner.release_now(lease)
    }
}
