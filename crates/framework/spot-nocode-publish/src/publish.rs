//! Publishing a signed app to the store registry, with the review hand-off.
//!
//! [`publish`] takes a [`SignedSource`] and a [`Registry`]. It always runs the app store review
//! first and returns its report; a report with a blocker stops the publication. It then applies
//! the semver rules (no republishing a version, every new version higher) before handing the
//! bytes, signature and certificate chain to `spot-registry`'s publish API.
//!
//! # Store versus private
//!
//! [`Audience::Store`] is the public catalogue. [`Audience::Private`] is the publisher's own
//! organisation only; this crate enforces that the certificate's publisher **is** that
//! organisation and records the audience on the receipt. The registry itself stores releases
//! as it always does; a private release is filtered by the caller's organisation catalogue, so
//! publishing privately never widens who may read it.
//!
//! # Versions
//!
//! Versions are `semver`. Build metadata does not create a new version, a version already
//! published cannot be replaced, and a version must exceed every published one. The registry
//! repeats these checks atomically; this crate refuses earlier with a plain message.

use std::future::Future;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};

use semver::Version;
use spot_app_review::{NullProbe, Report, ReviewConfig, Severity, Verdict};
use spot_bundle_signing::{BundleSignature, CertificateChain};
use spot_registry::{AppName, BlobStore, IndexStore, Registry as Store, Release, Visibility};

use crate::error::Error;
use crate::sign::SignedSource;

/// Where a release is listed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Audience {
    /// The public store catalogue.
    Store,
    /// The publisher's own organisation only.
    Private {
        /// The organisation id; it must equal the certificate's publisher id.
        organisation: String,
    },
}

impl Audience {
    /// Whether the audience is the public store.
    #[must_use]
    pub fn is_store(&self) -> bool {
        matches!(self, Self::Store)
    }
}

/// A bundle resolved from the registry, with the evidence the fetch verified.
#[derive(Clone, Debug)]
pub struct FetchedBundle {
    /// The validated `.spotapp` bytes.
    pub bytes: Vec<u8>,
    /// The authenticated release facts.
    pub release: Release,
    /// The signature verifier's evidence.
    pub verified: spot_bundle_signing::Verified,
}

impl FetchedBundle {
    /// The fetch evidence in the installer's vocabulary, ready for a plan candidate.
    ///
    /// The mapping is deliberately explicit: the installer decides trust from these fields, and
    /// a fetch that verified is the only thing that may set `ok`.
    #[must_use]
    pub fn installer_verified(&self) -> spot_installer::Verified {
        spot_installer::Verified {
            ok: true,
            publisher_id: self.verified.publisher_id.clone(),
            key_id: self.verified.key_id.as_str().to_owned(),
            signed_at: self.verified.signed_at,
            revocation_generation: self.verified.revocation_generation,
        }
    }
}

/// The publishing and resolving seam.
///
/// [`StoreRegistry`] implements it over `spot-registry`; tests may implement it over an
/// in-memory store. All methods take explicit time: the crate never reads a clock.
pub trait Registry {
    /// Every published semantic version of `app`, at `now`.
    ///
    /// # Errors
    /// The registry's refusal, when the app name is invalid or the listing is unavailable.
    fn versions(&self, app: &str, now: i64) -> Result<Vec<Version>, Error>;

    /// Publishes validated bytes with its publisher evidence.
    ///
    /// # Errors
    /// The registry's refusal.
    fn publish(
        &self,
        bytes: Vec<u8>,
        signature: BundleSignature,
        chain: CertificateChain,
        now: i64,
    ) -> Result<Release, Error>;

    /// Resolves and verifies `app` at `version`, at `now`.
    ///
    /// # Errors
    /// The registry's refusal (missing, yanked, withdrawn or failed verification).
    fn fetch(&self, app: &str, version: &Version, now: i64) -> Result<FetchedBundle, Error>;
}

/// `spot-registry`'s registry, behind the object-safe [`Registry`] seam.
///
/// The store's operations are asynchronous; this adapter drives them on the calling thread.
/// The in-memory stores complete without a reactor; a filesystem or network store keeps its
/// blocking work on its own executor (see `spot-registry`'s module docs).
pub struct StoreRegistry<I: IndexStore, B: BlobStore> {
    inner: Arc<Store<I, B>>,
}

impl<I: IndexStore, B: BlobStore> StoreRegistry<I, B> {
    /// Wraps a configured registry.
    #[must_use]
    pub fn new(inner: Arc<Store<I, B>>) -> Self {
        Self { inner }
    }

    /// The registry behind the adapter.
    #[must_use]
    pub fn inner(&self) -> &Arc<Store<I, B>> {
        &self.inner
    }
}

impl<I: IndexStore, B: BlobStore> Registry for StoreRegistry<I, B> {
    fn versions(&self, app: &str, now: i64) -> Result<Vec<Version>, Error> {
        let app = AppName::parse(app).map_err(Error::from)?;
        let releases = self
            .inner
            .releases(&app, &Visibility::default(), None, now)
            .map_err(Error::from)?;
        Ok(releases
            .into_iter()
            .map(|release| release.version)
            .collect())
    }

    fn publish(
        &self,
        bytes: Vec<u8>,
        signature: BundleSignature,
        chain: CertificateChain,
        now: i64,
    ) -> Result<Release, Error> {
        block_on(self.inner.publish(bytes, signature, chain, now)).map_err(Error::from)
    }

    fn fetch(&self, app: &str, version: &Version, now: i64) -> Result<FetchedBundle, Error> {
        let app = AppName::parse(app).map_err(Error::from)?;
        let fetched = block_on(self.inner.fetch(&app, version, now)).map_err(Error::from)?;
        Ok(FetchedBundle {
            bytes: fetched.bytes,
            release: fetched.release,
            verified: fetched.verified,
        })
    }
}

/// Drives a store future to completion on this thread. No runtime is required.
fn block_on<F: Future>(future: F) -> F::Output {
    struct ThreadWake(std::thread::Thread);
    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.unpark();
        }
    }
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = std::pin::pin!(future);
    loop {
        match future.as_mut().poll(&mut context) {
            Poll::Ready(output) => return output,
            Poll::Pending => std::thread::park(),
        }
    }
}

/// What a publish run reports back to the publisher.
#[derive(Clone, Debug)]
pub struct PublishReceipt {
    /// The release the registry stored.
    pub release: Release,
    /// The review report the store lists beside the app.
    pub report: Report,
    /// Where the release is listed.
    pub audience: Audience,
}

/// The knobs of one publish.
#[derive(Clone, Debug)]
pub struct PublishOptions {
    /// The store or private audience.
    pub audience: Audience,
    /// The review configuration.
    pub review: ReviewConfig,
    /// The review's timestamp, as the caller states it (never the clock).
    pub generated_at: String,
    /// The registry's time (UTC seconds).
    pub now: i64,
}

impl PublishOptions {
    /// Store publishing at `now`, with the default review configuration.
    #[must_use]
    pub fn store(now: i64, generated_at: impl Into<String>) -> Self {
        Self {
            audience: Audience::Store,
            review: ReviewConfig::default(),
            generated_at: generated_at.into(),
            now,
        }
    }

    /// Private publishing for `organisation` at `now`.
    #[must_use]
    pub fn private(
        organisation: impl Into<String>,
        now: i64,
        generated_at: impl Into<String>,
    ) -> Self {
        Self {
            audience: Audience::Private {
                organisation: organisation.into(),
            },
            review: ReviewConfig::default(),
            generated_at: generated_at.into(),
            now,
        }
    }
}

/// Runs the app store review and returns its report, refusing a blocker.
///
/// This is the hand-off the store records beside a listing: the review never approves an app,
/// and a [`Verdict::Rejected`] report stops a publication before it reaches the registry.
///
/// # Errors
/// [`Error::ReviewRejected`] listing every blocker finding.
pub fn review_app(
    bytes: &[u8],
    config: &ReviewConfig,
    generated_at: &str,
) -> Result<Report, Error> {
    let report = spot_app_review::review_at(bytes, config, &NullProbe, generated_at);
    if report.verdict != Verdict::Rejected {
        return Ok(report);
    }
    let blockers = report
        .findings
        .iter()
        .filter(|finding| finding.severity == Severity::Blocker)
        .map(|finding| format!("{}: {}", finding.rule, finding.message))
        .collect::<Vec<_>>()
        .join("; ");
    Err(Error::ReviewRejected { blockers })
}

/// Runs the review, checks the version rules and publishes `signed`.
///
/// # Errors
/// [`Error::ReviewRejected`] when the review finds a blocker, [`Error::PrivateOtherOrganisation`]
/// when a private release names another owner, [`Error::VersionExists`] or
/// [`Error::VersionNotIncreasing`] for a bad version, and [`Error::Registry`] when the store
/// refuses the release.
pub fn publish(
    signed: &SignedSource,
    registry: &dyn Registry,
    options: &PublishOptions,
) -> Result<PublishReceipt, Error> {
    let report = review_app(signed.bytes(), &options.review, &options.generated_at)?;

    if let Audience::Private { organisation } = &options.audience
        && organisation != signed.publisher()
    {
        return Err(Error::PrivateOtherOrganisation {
            publisher: signed.publisher().to_owned(),
            organisation: organisation.clone(),
        });
    }

    let existing = registry.versions(signed.app(), options.now)?;
    check_version_bump(signed.app(), &existing, signed.version())?;

    let release = registry.publish(
        signed.bytes().to_vec(),
        signed.signature().clone(),
        signed.chain().clone(),
        options.now,
    )?;
    Ok(PublishReceipt {
        release,
        report,
        audience: options.audience.clone(),
    })
}

/// The semver rules: `new` must not exist and must exceed every published version.
///
/// # Errors
/// [`Error::VersionExists`] for the same semantic version (build metadata does not help) and
/// [`Error::VersionNotIncreasing`] when the new version is not higher than the latest.
pub fn check_version_bump(app: &str, existing: &[Version], new: &Version) -> Result<(), Error> {
    if let Some(same) = existing
        .iter()
        .find(|version| version.cmp_precedence(new).is_eq())
    {
        return Err(Error::VersionExists {
            app: app.to_owned(),
            version: same.to_string(),
        });
    }
    if let Some(latest) = existing.iter().max_by(|a, b| a.cmp_precedence(b))
        && !new.cmp_precedence(latest).is_gt()
    {
        return Err(Error::VersionNotIncreasing {
            app: app.to_owned(),
            version: new.to_string(),
            latest: latest.to_string(),
        });
    }
    Ok(())
}
