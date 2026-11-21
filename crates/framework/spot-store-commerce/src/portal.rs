//! The publisher portal: the service functions the portal screens call.
//!
//! Submitting a version hands the bundle bytes to a [`ReviewService`] — in production
//! [`SpotReviewService`] over `spot-app-review` — and records the report and its status
//! against the version. The portal never approves an app: a human calls
//! [`Portal::approve_version`]. Setting pricing, viewing review status and viewing sales
//! and entitlements are all here.

use spot_registry::Release;

use crate::catalogue::{Catalogue, Listing, ListingVersion, Price, PricePlan, ReviewStatus};
use crate::entitlement::{Entitlement, EntitlementStore};
use crate::error::{Result, StoreError, codes};
use crate::ledger::{Ledger, Order};
use crate::publisher::PublisherRegistry;

/// The review pipeline's verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewVerdict {
    /// A blocker was found; the version cannot be accepted as it stands.
    Rejected,
    /// A person must read the findings before the version is accepted.
    NeedsHumanReview,
    /// No finding above Info; still for a person to accept.
    LooksClean,
}

/// The outcome of running the review pipeline over a bundle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewOutcome {
    /// The verdict.
    pub verdict: ReviewVerdict,
    /// How many findings the report carried.
    pub findings: u32,
    /// The review report, as JSON.
    pub report: String,
}

/// The review pipeline a submitted version is handed to.
pub trait ReviewService: Send + Sync {
    /// Reviews a bundle's bytes and returns the outcome. `generated_at` is the caller's
    /// timestamp string, so two runs over the same bytes are byte-identical.
    fn review(&self, bytes: &[u8], generated_at: &str) -> ReviewOutcome;
}

/// The production review service: `spot-app-review`'s pipeline.
///
/// The review produces evidence, never an approval; the portal records the outcome and a
/// human decides.
#[derive(Debug, Default)]
pub struct SpotReviewService;

impl ReviewService for SpotReviewService {
    fn review(&self, bytes: &[u8], generated_at: &str) -> ReviewOutcome {
        let report = spot_app_review::review_at(
            bytes,
            &spot_app_review::ReviewConfig::default(),
            &spot_app_review::NullProbe,
            generated_at,
        );
        let verdict = match report.verdict {
            spot_app_review::Verdict::Rejected => ReviewVerdict::Rejected,
            spot_app_review::Verdict::NeedsHumanReview => ReviewVerdict::NeedsHumanReview,
            spot_app_review::Verdict::LooksClean => ReviewVerdict::LooksClean,
        };
        ReviewOutcome {
            verdict,
            findings: report.counts.total(),
            report: report.to_json(),
        }
    }
}

/// A review service for tests: a fixed verdict and no bundle parsing.
#[derive(Clone, Debug)]
pub struct FakeReviewService {
    /// The verdict every review returns.
    pub verdict: ReviewVerdict,
    /// How many findings every report claims.
    pub findings: u32,
}

impl FakeReviewService {
    /// A service that returns `verdict`.
    #[must_use]
    pub fn new(verdict: ReviewVerdict) -> Self {
        Self {
            verdict,
            findings: 0,
        }
    }
}

impl ReviewService for FakeReviewService {
    fn review(&self, _bytes: &[u8], generated_at: &str) -> ReviewOutcome {
        ReviewOutcome {
            verdict: self.verdict,
            findings: self.findings,
            report: format!("{{\"verdict\":\"fake\",\"generated_at\":\"{generated_at}\"}}"),
        }
    }
}

/// A version submission: the publisher, the listing and the release plus its bytes.
#[derive(Clone, Debug)]
pub struct Submission {
    /// The submitting publisher id.
    pub publisher: String,
    /// The listing the version belongs to.
    pub listing_id: String,
    /// The registry release (already published; the portal records it against the listing).
    pub release: Release,
    /// The bundle bytes to review.
    pub bytes: Vec<u8>,
}

/// The publisher portal, over the store's publisher, catalogue and review components.
pub struct Portal<'a> {
    publishers: &'a PublisherRegistry,
    catalogue: &'a Catalogue,
    review: &'a dyn ReviewService,
}

impl<'a> Portal<'a> {
    /// A portal over the given components.
    #[must_use]
    pub fn new(
        publishers: &'a PublisherRegistry,
        catalogue: &'a Catalogue,
        review: &'a dyn ReviewService,
    ) -> Self {
        Self {
            publishers,
            catalogue,
            review,
        }
    }

    /// Submits an app version: checks the publisher may publish, records the release, runs
    /// the review and stores its status.
    ///
    /// The status is [`ReviewStatus::Rejected`] for a rejected review and
    /// [`ReviewStatus::NeedsHumanReview`] otherwise — **even for a clean review**, because
    /// the review never approves an app; a person calls [`Portal::approve_version`].
    ///
    /// # Errors
    /// [`codes::PUBLISHER_UNKNOWN`], [`codes::PUBLISHER_NOT_VERIFIED`],
    /// [`codes::PUBLISHER_SUSPENDED`], [`codes::LISTING_UNKNOWN`], [`codes::INVALID_INPUT`]
    /// when the publisher does not own the listing or the release's app does not match, or
    /// [`codes::VERSION_UNKNOWN`] when the version already exists.
    pub fn submit_version(&self, submission: &Submission, now: i64) -> Result<ListingVersion> {
        self.publishers.require_active(&submission.publisher)?;
        let listing = self.catalogue.get(&submission.listing_id)?;
        if listing.publisher != submission.publisher {
            return Err(StoreError::new(
                codes::INVALID_INPUT,
                "the publisher does not own the listing",
            ));
        }
        self.catalogue
            .add_version(&submission.listing_id, submission.release.clone(), now)?;
        let outcome = self.review.review(&submission.bytes, &now.to_string());
        let status = match outcome.verdict {
            ReviewVerdict::Rejected => ReviewStatus::Rejected,
            ReviewVerdict::NeedsHumanReview | ReviewVerdict::LooksClean => {
                ReviewStatus::NeedsHumanReview
            }
        };
        self.catalogue.set_review(
            &submission.listing_id,
            &submission.release.version,
            status,
            Some(outcome.report),
            now,
        )
    }

    /// The review status and report of a version.
    ///
    /// # Errors
    /// [`codes::LISTING_UNKNOWN`] or [`codes::VERSION_UNKNOWN`].
    pub fn review_status(
        &self,
        listing_id: &str,
        version: &spot_registry::Version,
    ) -> Result<(ReviewStatus, Option<String>)> {
        let listing = self.catalogue.get(listing_id)?;
        let row = listing
            .version(version)
            .ok_or_else(|| StoreError::missing(codes::VERSION_UNKNOWN, "the app version"))?;
        Ok((row.review, row.review_report.clone()))
    }

    /// A store reviewer approves a version. The version becomes listable only if its release
    /// is signed and active.
    ///
    /// `reviewer` is the store staff identity doing the approval; the caller must have
    /// authorised that identity for the reviewer role (that check is the caller's, because this
    /// crate has no staff directory). It must not be empty and must not be the listing's own
    /// publisher: a publisher cannot approve their own app past the store's review.
    ///
    /// # Errors
    /// [`codes::INVALID_INPUT`] for an empty reviewer or the listing's own publisher,
    /// [`codes::LISTING_UNKNOWN`], [`codes::VERSION_UNKNOWN`], or the listing gate's own
    /// refusal ([`codes::LISTING_UNREVIEWED`], [`codes::LISTING_UNSIGNED`]).
    pub fn approve_version(
        &self,
        reviewer: &str,
        listing_id: &str,
        version: &spot_registry::Version,
        now: i64,
    ) -> Result<ListingVersion> {
        let listing = self.catalogue.get(listing_id)?;
        if reviewer.trim().is_empty() || listing.publisher == reviewer {
            return Err(StoreError::new(
                codes::INVALID_INPUT,
                "a version is approved by a store reviewer, never by its own publisher",
            ));
        }
        let report = listing
            .version(version)
            .and_then(|row| row.review_report.clone());
        self.catalogue
            .approve_version(listing_id, version, report, now)
    }

    /// Sets a listing's pricing. The publisher must own the listing.
    ///
    /// # Errors
    /// [`codes::PUBLISHER_UNKNOWN`], [`codes::PUBLISHER_NOT_VERIFIED`],
    /// [`codes::PUBLISHER_SUSPENDED`], [`codes::LISTING_UNKNOWN`], [`codes::INVALID_INPUT`],
    /// or [`codes::PRICE_INVALID`].
    #[allow(clippy::too_many_arguments)]
    pub fn set_pricing(
        &self,
        publisher: &str,
        listing_id: &str,
        plan: PricePlan,
        price: Option<Price>,
        trial_days: u32,
        regions: Vec<String>,
        now: i64,
    ) -> Result<Listing> {
        self.publishers.require_active(publisher)?;
        let listing = self.catalogue.get(listing_id)?;
        if listing.publisher != publisher {
            return Err(StoreError::new(
                codes::INVALID_INPUT,
                "the publisher does not own the listing",
            ));
        }
        self.catalogue
            .set_pricing(listing_id, plan, price, trial_days, regions, now)
    }

    /// The sales of a publisher's apps, in recording order.
    ///
    /// # Errors
    /// [`codes::PUBLISHER_UNKNOWN`].
    pub fn sales(&self, publisher: &str, ledger: &Ledger) -> Result<Vec<Order>> {
        self.publishers.get(publisher)?;
        Ok(ledger.orders_for_publisher(publisher))
    }

    /// The entitlements sold for a publisher's apps, in app order.
    ///
    /// # Errors
    /// [`codes::PUBLISHER_UNKNOWN`].
    pub fn entitlements(
        &self,
        publisher: &str,
        entitlements: &EntitlementStore,
    ) -> Result<Vec<Entitlement>> {
        self.publishers.get(publisher)?;
        let apps: Vec<String> = self
            .catalogue
            .all()
            .into_iter()
            .filter(|listing| listing.publisher == publisher)
            .map(|listing| listing.app.as_str().to_owned())
            .collect();
        Ok(entitlements
            .all()
            .into_iter()
            .filter(|entitlement| apps.contains(&entitlement.app))
            .collect())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{
        FakeReviewService, Portal, ReviewService, ReviewVerdict, SpotReviewService, Submission,
    };
    use crate::catalogue::{Catalogue, ListingState, ReviewStatus};
    use crate::error::codes;
    use crate::licence::StoreSigner;
    use crate::publisher::PublisherRegistry;
    use crate::test_support::{TestSigner, release};

    fn publishers() -> PublisherRegistry {
        let registry = PublisherRegistry::new();
        registry
            .onboard("acme", "Acme Ltd", "payout-1", 100)
            .unwrap();
        registry.verify("acme", 150).unwrap();
        registry
            .register_signing_key("acme", &TestSigner::new(7).public_key_bytes(), 160)
            .unwrap();
        registry
    }

    #[test]
    fn a_submitted_version_is_not_listed_until_a_human_approves() {
        let publishers = publishers();
        let catalogue = Catalogue::new();
        catalogue
            .create("list-1", "my_app", "acme", "My App", "", "tools", 100)
            .unwrap();
        let review = FakeReviewService::new(ReviewVerdict::LooksClean);
        let portal = Portal::new(&publishers, &catalogue, &review);
        let submission = Submission {
            publisher: "acme".to_owned(),
            listing_id: "list-1".to_owned(),
            release: release("my_app", "acme", "1.0.0"),
            bytes: b"not a zip".to_vec(),
        };
        let submitted = portal.submit_version(&submission, 200).unwrap();
        assert_eq!(submitted.review, ReviewStatus::NeedsHumanReview);
        assert!(
            catalogue
                .get("list-1")
                .unwrap()
                .listable_versions()
                .is_empty()
        );

        portal
            .approve_version("reviewer-1", "list-1", &"1.0.0".parse().unwrap(), 300)
            .unwrap();
        let listing = catalogue.get("list-1").unwrap();
        assert_eq!(listing.state, ListingState::Published);
        assert_eq!(listing.listable_versions().len(), 1);
    }

    #[test]
    fn a_rejected_review_is_recorded_and_a_review_never_approves() {
        let publishers = publishers();
        let catalogue = Catalogue::new();
        catalogue
            .create("list-1", "my_app", "acme", "My App", "", "tools", 100)
            .unwrap();
        let review = FakeReviewService::new(ReviewVerdict::Rejected);
        let portal = Portal::new(&publishers, &catalogue, &review);
        let submission = Submission {
            publisher: "acme".to_owned(),
            listing_id: "list-1".to_owned(),
            release: release("my_app", "acme", "1.0.0"),
            bytes: b"not a zip".to_vec(),
        };
        portal.submit_version(&submission, 200).unwrap();
        let (status, report) = portal
            .review_status("list-1", &"1.0.0".parse().unwrap())
            .unwrap();
        assert_eq!(status, ReviewStatus::Rejected);
        assert!(report.is_some());
        let version: spot_registry::Version = "1.0.0".parse().unwrap();
        assert_eq!(
            portal
                .approve_version("reviewer-1", "list-1", &version, 250)
                .unwrap_err()
                .code(),
            codes::LISTING_UNREVIEWED
        );
        assert_eq!(
            portal
                .approve_version("acme", "list-1", &version, 250)
                .unwrap_err()
                .code(),
            codes::INVALID_INPUT
        );
        assert_eq!(
            portal.review_status("list-1", &version).unwrap().0,
            ReviewStatus::Rejected
        );
        assert!(
            catalogue
                .get("list-1")
                .unwrap()
                .listable_versions()
                .is_empty()
        );
    }

    #[test]
    fn an_unverified_publisher_cannot_submit() {
        let publishers = PublisherRegistry::new();
        publishers
            .onboard("acme", "Acme Ltd", "payout-1", 100)
            .unwrap();
        let catalogue = Catalogue::new();
        catalogue
            .create("list-1", "my_app", "acme", "My App", "", "tools", 100)
            .unwrap();
        let review = FakeReviewService::new(ReviewVerdict::LooksClean);
        let portal = Portal::new(&publishers, &catalogue, &review);
        let submission = Submission {
            publisher: "acme".to_owned(),
            listing_id: "list-1".to_owned(),
            release: release("my_app", "acme", "1.0.0"),
            bytes: b"not a zip".to_vec(),
        };
        assert_eq!(
            portal.submit_version(&submission, 200).unwrap_err().code(),
            codes::PUBLISHER_NOT_VERIFIED
        );
    }

    #[test]
    fn the_spot_review_service_rejects_non_bundles() {
        let outcome = SpotReviewService.review(b"not a zip", "100");
        assert_eq!(outcome.verdict, ReviewVerdict::Rejected);
        assert!(outcome.report.contains("bundle.bad_archive"));
    }
}
