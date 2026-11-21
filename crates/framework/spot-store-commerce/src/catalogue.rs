//! The catalogue: listings, their price plans and the review gate.
//!
//! A [`Listing`] is one app in the store: its publisher, its priced versions and its
//! regional availability. Versions are [`Release`]s from `spot-registry` (so the store
//! reuses the registry's identity, digest and signature evidence) gated by a human
//! [`ReviewStatus`]: **only an approved version of a signed, active release is listed**.
//! Browsing delegates to the registry's ranked search over an `IndexStore`.
//!
//! Prices are [`Price`]s in **integer minor units** of one [`Currency`] (ADR-12: no
//! floats); converting to a `spot-money` [`Money`] is explicit and exact.

use std::collections::BTreeMap;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use spot_money::{Currency, CurrencyTable, Money, parse_decimal};
use spot_registry::{AppName, IndexStore, Release, SearchPage, SearchQuery, Version};

use crate::error::{Result, StoreError, codes};
use crate::publisher::check_identifier;

/// How often a subscription plan bills.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Interval {
    /// Once a month.
    Monthly,
    /// Once a year.
    Annual,
}

impl Interval {
    /// The length of one billing period in seconds (a nominal month of 30 days, a year of
    /// 365; the provider's calendar is the source of truth for a real charge).
    #[must_use]
    pub fn seconds(self) -> i64 {
        match self {
            Self::Monthly => 30 * 86_400,
            Self::Annual => 365 * 86_400,
        }
    }
}

/// What a listing costs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricePlan {
    /// Free to install; no checkout, no licence.
    Free,
    /// A one-time purchase that grants a perpetual app licence.
    OneTime,
    /// A recurring subscription; the licence carries an expiry and a grace period.
    Subscription {
        /// How often it bills.
        interval: Interval,
    },
}

impl PricePlan {
    /// Whether this plan is paid (a checkout is required).
    #[must_use]
    pub fn is_paid(self) -> bool {
        !matches!(self, Self::Free)
    }

    /// The spot-license kind an app licence for this plan mints.
    #[must_use]
    pub fn licence_kind(self) -> spot_license::Kind {
        match self {
            Self::Free | Self::OneTime => spot_license::Kind::Perpetual,
            Self::Subscription { .. } => spot_license::Kind::Subscription,
        }
    }

    /// The billing interval, when the plan is a subscription.
    #[must_use]
    pub fn interval(self) -> Option<Interval> {
        match self {
            Self::Subscription { interval } => Some(interval),
            _ => None,
        }
    }
}

/// A price in integer minor units of one currency.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Price {
    minor_units: i64,
    currency: Currency,
}

impl Price {
    /// A price of `minor_units` (cents, pence, ...) in `currency`.
    ///
    /// # Errors
    /// [`codes::PRICE_INVALID`] when the amount is negative.
    pub fn new(minor_units: i64, currency: Currency) -> Result<Self> {
        if minor_units < 0 {
            return Err(StoreError::new(
                codes::PRICE_INVALID,
                "a price is a non-negative number of minor units",
            ));
        }
        Ok(Self {
            minor_units,
            currency,
        })
    }

    /// The amount in minor units.
    #[must_use]
    pub fn minor_units(&self) -> i64 {
        self.minor_units
    }

    /// The currency.
    #[must_use]
    pub fn currency(&self) -> Currency {
        self.currency
    }

    /// Whether the price is zero.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.minor_units == 0
    }

    /// The exact `spot-money` value, using `table` for the currency's minor-unit exponent.
    ///
    /// # Errors
    /// [`codes::MONEY_ERROR`] when the currency is not in the table, and
    /// [`codes::PRICE_INVALID`] if the conversion cannot be represented.
    pub fn money(&self, table: &CurrencyTable) -> Result<Money> {
        let exponent = table.exponent(&self.currency)?;
        let text = minor_text(self.minor_units, exponent);
        let value = parse_decimal(&text)?;
        Ok(Money::new(value, self.currency))
    }

    /// Reads an exact `spot-money` amount back into minor units.
    ///
    /// # Errors
    /// [`codes::MONEY_ERROR`] when the currency is not in the table, and
    /// [`codes::PRICE_INVALID`] when the value is not an exact whole number of minor
    /// units (it would need rounding, which this crate never does silently).
    pub fn from_money(money: &Money, table: &CurrencyTable) -> Result<Self> {
        let currency = *money.currency();
        let exponent = table.exponent(&currency)?;
        let value = money.value();
        let scale = value.scale();
        let mantissa = value.mantissa();
        let minor = if scale <= exponent {
            let factor = pow10_i128(exponent - scale)?;
            mantissa.checked_mul(factor).ok_or_else(price_overflow)?
        } else {
            let factor = pow10_i128(scale - exponent)?;
            if mantissa % factor != 0 {
                return Err(StoreError::new(
                    codes::PRICE_INVALID,
                    "the amount is not a whole number of minor units",
                ));
            }
            mantissa / factor
        };
        let minor = i64::try_from(minor).map_err(|_| price_overflow())?;
        Price::new(minor, currency)
    }
}

/// `10^exponent` as an `i128`, refusing exponents past a decimal's 28 digits.
fn pow10_i128(exponent: u32) -> Result<i128> {
    if exponent > 28 {
        return Err(price_overflow());
    }
    let mut value = 1_i128;
    for _ in 0..exponent {
        value = value.checked_mul(10).ok_or_else(price_overflow)?;
    }
    Ok(value)
}

/// `minor` as exact decimal text with `exponent` fractional digits.
fn minor_text(minor: i64, exponent: u32) -> String {
    let negative = minor < 0;
    let magnitude = i128::from(minor).unsigned_abs();
    let factor = 10_u128.pow(exponent);
    let whole = magnitude / factor;
    let fraction = magnitude % factor;
    let mut text = String::new();
    if negative {
        text.push('-');
    }
    text.push_str(&whole.to_string());
    if exponent > 0 {
        text.push('.');
        let digits = fraction.to_string();
        for _ in 0..(exponent as usize - digits.len()) {
            text.push('0');
        }
        text.push_str(&digits);
    }
    text
}

fn price_overflow() -> StoreError {
    StoreError::new(codes::PRICE_INVALID, "the price is out of range")
}

/// Where an app version is in the review pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewStatus {
    /// Submitted; the review has not concluded.
    Submitted,
    /// The review produced findings a person must read.
    NeedsHumanReview,
    /// The review found a blocker; the version cannot be listed.
    Rejected,
    /// A human approved the version; it may be listed (if signed).
    Approved,
}

/// One version of a listing: the registry release plus its review state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListingVersion {
    /// The semantic version.
    pub version: Version,
    /// The registry release the version is addressed by.
    pub release: Release,
    /// The review state.
    pub review: ReviewStatus,
    /// The review report (JSON), when the review ran.
    pub review_report: Option<String>,
    /// When the version was submitted (UTC seconds).
    pub submitted_at: i64,
    /// When the version was approved, if it was (UTC seconds).
    pub approved_at: Option<i64>,
}

impl ListingVersion {
    /// Whether the version may be listed: approved by a human **and** a signed, active
    /// registry release.
    ///
    /// # Errors
    /// The refusal that stops listing, for a caller that wants to explain it.
    pub fn listable(&self) -> Result<()> {
        if self.review != ReviewStatus::Approved {
            return Err(StoreError::new(
                codes::LISTING_UNREVIEWED,
                "the version has not been approved by a human reviewer",
            ));
        }
        if !self.release.active() {
            return Err(StoreError::new(
                codes::LISTING_UNSIGNED,
                "the release is not available for new installs",
            ));
        }
        if self.release.signature_ref.signature.app_id != self.release.app.as_str()
            || self.release.signature_ref.signature.publisher_id != self.release.publisher
        {
            return Err(StoreError::new(
                codes::LISTING_UNSIGNED,
                "the release signature does not name this app and publisher",
            ));
        }
        Ok(())
    }
}

/// Where a listing is in its own lifecycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ListingState {
    /// Created, but no approved version yet.
    Draft,
    /// Published: at least one approved version is listable.
    Published,
    /// Delisted by its publisher; no new installs.
    Unlisted,
}

/// One app in the store: publisher, priced versions, regions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing {
    /// The stable listing id.
    pub id: String,
    /// The app name `spot-registry` uses.
    pub app: AppName,
    /// The owning publisher id.
    pub publisher: String,
    /// The catalogue title.
    pub title: String,
    /// The catalogue description.
    pub description: String,
    /// The catalogue category.
    pub category: String,
    /// Every submitted version, newest last.
    pub versions: Vec<ListingVersion>,
    /// What the app costs.
    pub price_plan: PricePlan,
    /// The price, for a paid plan.
    pub price: Option<Price>,
    /// Free trial length in days (0 for none).
    pub trial_days: u32,
    /// The regions the listing is sold in (ISO-like codes); empty means everywhere.
    pub regions: Vec<String>,
    /// The listing lifecycle state.
    pub state: ListingState,
    /// When the listing was last changed (UTC seconds).
    pub updated_at: i64,
}

impl Listing {
    /// The approved, listable versions, newest first.
    #[must_use]
    pub fn listable_versions(&self) -> Vec<&ListingVersion> {
        let mut versions: Vec<&ListingVersion> = self
            .versions
            .iter()
            .filter(|version| version.listable().is_ok())
            .collect();
        versions.sort_by(|a, b| b.version.cmp(&a.version));
        versions
    }

    /// The newest listable version, when there is one.
    #[must_use]
    pub fn latest_listable(&self) -> Option<&ListingVersion> {
        self.listable_versions().into_iter().next()
    }

    /// The version with the given semantic version, approved or not.
    #[must_use]
    pub fn version(&self, version: &Version) -> Option<&ListingVersion> {
        self.versions.iter().find(|row| &row.version == version)
    }

    /// Whether the listing is on sale at `now` and in `region`.
    #[must_use]
    pub fn sells_in(&self, region: Option<&str>) -> bool {
        if self.state != ListingState::Published {
            return false;
        }
        match region {
            None => true,
            Some(region) => {
                self.regions.is_empty() || self.regions.iter().any(|code| code == region)
            }
        }
    }
}

/// The store's catalogue of listings.
#[derive(Debug, Default)]
pub struct Catalogue {
    listings: Mutex<BTreeMap<String, Listing>>,
}

impl Catalogue {
    /// An empty catalogue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Listing>> {
        self.listings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Creates a draft listing for an app, free by default.
    ///
    /// # Errors
    /// [`codes::INVALID_INPUT`] for a bad listing id or app name, or a duplicate listing
    /// id or app.
    #[allow(clippy::too_many_arguments)]
    pub fn create(
        &self,
        id: &str,
        app: &str,
        publisher: &str,
        title: &str,
        description: &str,
        category: &str,
        now: i64,
    ) -> Result<Listing> {
        check_identifier(id, "listing id")?;
        check_identifier(publisher, "publisher id")?;
        let app = AppName::parse(app)?;
        let listing = Listing {
            id: id.to_owned(),
            app,
            publisher: publisher.to_owned(),
            title: title.to_owned(),
            description: description.to_owned(),
            category: category.to_owned(),
            versions: Vec::new(),
            price_plan: PricePlan::Free,
            price: None,
            trial_days: 0,
            regions: Vec::new(),
            state: ListingState::Draft,
            updated_at: now,
        };
        let mut listings = self.lock();
        if listings.contains_key(id) {
            return Err(StoreError::new(
                codes::INVALID_INPUT,
                "the listing id is already used",
            ));
        }
        if listings
            .values()
            .any(|existing| existing.app == listing.app)
        {
            return Err(StoreError::new(
                codes::INVALID_INPUT,
                "the app already has a listing",
            ));
        }
        listings.insert(id.to_owned(), listing.clone());
        Ok(listing)
    }

    /// The listing with the given id.
    ///
    /// # Errors
    /// [`codes::LISTING_UNKNOWN`].
    pub fn get(&self, id: &str) -> Result<Listing> {
        self.lock()
            .get(id)
            .cloned()
            .ok_or_else(|| StoreError::missing(codes::LISTING_UNKNOWN, "the listing"))
    }

    /// The listing for an app.
    ///
    /// # Errors
    /// [`codes::LISTING_UNKNOWN`].
    pub fn by_app(&self, app: &AppName) -> Result<Listing> {
        self.lock()
            .values()
            .find(|listing| &listing.app == app)
            .cloned()
            .ok_or_else(|| StoreError::missing(codes::LISTING_UNKNOWN, "the listing"))
    }

    fn update(&self, id: &str, apply: impl FnOnce(&mut Listing)) -> Result<Listing> {
        let mut listings = self.lock();
        let listing = listings
            .get_mut(id)
            .ok_or_else(|| StoreError::missing(codes::LISTING_UNKNOWN, "the listing"))?;
        apply(listing);
        Ok(listing.clone())
    }

    /// Sets a listing's price plan, price, trial and regions.
    ///
    /// # Errors
    /// [`codes::LISTING_UNKNOWN`], [`codes::PRICE_INVALID`] when a paid plan has no
    /// positive price, or [`codes::INVALID_INPUT`] when a free plan carries a price.
    pub fn set_pricing(
        &self,
        id: &str,
        plan: PricePlan,
        price: Option<Price>,
        trial_days: u32,
        regions: Vec<String>,
        now: i64,
    ) -> Result<Listing> {
        if plan.is_paid() {
            match price {
                Some(price) if !price.is_zero() => {}
                _ => {
                    return Err(StoreError::new(
                        codes::PRICE_INVALID,
                        "a paid plan needs a positive price",
                    ));
                }
            }
        } else if price.is_some() {
            return Err(StoreError::new(
                codes::INVALID_INPUT,
                "a free plan carries no price",
            ));
        }
        if plan.interval().is_none() && trial_days > 0 {
            return Err(StoreError::new(
                codes::INVALID_INPUT,
                "only a subscription can carry a free trial",
            ));
        }
        self.update(id, |listing| {
            listing.price_plan = plan;
            listing.price = price;
            listing.trial_days = trial_days;
            listing.regions = regions;
            listing.updated_at = now;
        })
    }

    /// Records a submitted version, in [`ReviewStatus::Submitted`].
    ///
    /// # Errors
    /// [`codes::LISTING_UNKNOWN`], [`codes::INVALID_INPUT`] when the release's app does
    /// not match the listing, or [`codes::VERSION_UNKNOWN`] when the version already
    /// exists.
    pub fn add_version(&self, id: &str, release: Release, now: i64) -> Result<ListingVersion> {
        let version = ListingVersion {
            version: release.version.clone(),
            release,
            review: ReviewStatus::Submitted,
            review_report: None,
            submitted_at: now,
            approved_at: None,
        };
        let listing = self.update(id, |_| {})?;
        if listing.app != version.release.app {
            return Err(StoreError::new(
                codes::INVALID_INPUT,
                "the release's app does not match the listing",
            ));
        }
        if listing.version(&version.version).is_some() {
            return Err(StoreError::new(
                codes::VERSION_UNKNOWN,
                "the version is already submitted",
            ));
        }
        self.update(id, |listing| {
            listing.versions.push(version.clone());
            listing.updated_at = now;
        })?;
        Ok(version)
    }

    /// Records the outcome of a review.
    ///
    /// # Errors
    /// [`codes::LISTING_UNKNOWN`] or [`codes::VERSION_UNKNOWN`].
    pub fn set_review(
        &self,
        id: &str,
        version: &Version,
        review: ReviewStatus,
        report: Option<String>,
        now: i64,
    ) -> Result<ListingVersion> {
        let mut result = None;
        self.update(id, |listing| {
            if let Some(row) = listing
                .versions
                .iter_mut()
                .find(|row| &row.version == version)
            {
                row.review = review;
                if report.is_some() {
                    row.review_report = report.clone();
                }
                if review == ReviewStatus::Approved {
                    row.approved_at = Some(now);
                }
                result = Some(row.clone());
            }
            listing.updated_at = now;
        })?;
        result.ok_or_else(|| StoreError::missing(codes::VERSION_UNKNOWN, "the app version"))
    }

    /// A human approves a version; it becomes listable if the release is signed and active.
    ///
    /// # Errors
    /// [`codes::LISTING_UNKNOWN`], [`codes::VERSION_UNKNOWN`], or the listing gate's own
    /// refusal.
    pub fn approve_version(
        &self,
        id: &str,
        version: &Version,
        report: Option<String>,
        now: i64,
    ) -> Result<ListingVersion> {
        // Decide before writing: a rejected version is never approved, and an approval that
        // the listing gate would refuse leaves the stored review untouched.
        let current = self
            .get(id)?
            .version(version)
            .cloned()
            .ok_or_else(|| StoreError::missing(codes::VERSION_UNKNOWN, "the app version"))?;
        if current.review == ReviewStatus::Rejected {
            return Err(StoreError::new(
                codes::LISTING_UNREVIEWED,
                "the version was rejected by review and cannot be approved",
            ));
        }
        let mut candidate = current;
        candidate.review = ReviewStatus::Approved;
        candidate.listable()?;
        let approved = self.set_review(id, version, ReviewStatus::Approved, report, now)?;
        self.update(id, |listing| {
            listing.state = ListingState::Published;
        })?;
        Ok(approved)
    }

    /// Delists a listing; no new installs are offered.
    ///
    /// # Errors
    /// [`codes::LISTING_UNKNOWN`].
    pub fn unlist(&self, id: &str, now: i64) -> Result<Listing> {
        self.update(id, |listing| {
            listing.state = ListingState::Unlisted;
            listing.updated_at = now;
        })
    }

    /// Browsing delegates to the registry's ranked search over `index`, keeping only
    /// releases the catalogue has approved and published.
    ///
    /// # Errors
    /// [`codes::REGISTRY_ERROR`] when the index refuses the query.
    pub fn browse(&self, index: &dyn IndexStore, query: &SearchQuery) -> Result<SearchPage> {
        let mut page = index.search(query)?;
        page.releases.retain(|release| {
            self.lock().values().any(|listing| {
                listing.state == ListingState::Published
                    && listing
                        .version(&release.version)
                        .is_some_and(|version| version.listable().is_ok())
            })
        });
        Ok(page)
    }

    /// Every listing, in id order.
    #[must_use]
    pub fn all(&self) -> Vec<Listing> {
        self.lock().values().cloned().collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{Catalogue, Interval, Price, PricePlan, ReviewStatus};
    use crate::error::codes;
    use crate::test_support::{release, table};
    use spot_money::Currency;

    #[test]
    fn prices_round_trip_through_money() {
        let table = table();
        let price = Price::new(1250, Currency::new("USD").unwrap()).unwrap();
        let money = price.money(&table).unwrap();
        assert_eq!(money.value().to_string(), "12.50");
        assert_eq!(Price::from_money(&money, &table).unwrap(), price);
    }

    #[test]
    fn a_price_needing_rounding_is_refused() {
        let table = table();
        let money = spot_money::Money::new(
            spot_money::parse_decimal("12.505").unwrap(),
            Currency::new("USD").unwrap(),
        );
        assert_eq!(
            Price::from_money(&money, &table).unwrap_err().code(),
            codes::PRICE_INVALID
        );
    }

    #[test]
    fn an_unreviewed_version_is_not_listed() {
        let catalogue = Catalogue::new();
        catalogue
            .create("list-1", "my_app", "acme", "My App", "", "tools", 100)
            .unwrap();
        catalogue
            .add_version("list-1", release("my_app", "acme", "1.0.0"), 150)
            .unwrap();
        let listing = catalogue.get("list-1").unwrap();
        assert!(listing.listable_versions().is_empty());
        assert_eq!(
            listing.versions[0].listable().unwrap_err().code(),
            codes::LISTING_UNREVIEWED
        );
    }

    #[test]
    fn approval_publishes_and_a_subscription_needs_a_price() {
        let catalogue = Catalogue::new();
        catalogue
            .create("list-1", "my_app", "acme", "My App", "", "tools", 100)
            .unwrap();
        assert_eq!(
            catalogue
                .set_pricing(
                    "list-1",
                    PricePlan::Subscription {
                        interval: Interval::Monthly
                    },
                    None,
                    0,
                    vec![],
                    110,
                )
                .unwrap_err()
                .code(),
            codes::PRICE_INVALID
        );
        catalogue
            .set_pricing(
                "list-1",
                PricePlan::Subscription {
                    interval: Interval::Monthly,
                },
                Some(Price::new(990, Currency::new("USD").unwrap()).unwrap()),
                14,
                vec!["US".to_owned()],
                120,
            )
            .unwrap();
        catalogue
            .add_version("list-1", release("my_app", "acme", "1.0.0"), 150)
            .unwrap();
        catalogue
            .approve_version("list-1", &"1.0.0".parse().unwrap(), None, 200)
            .unwrap();
        let listing = catalogue.get("list-1").unwrap();
        assert_eq!(listing.state, super::ListingState::Published);
        assert_eq!(listing.listable_versions().len(), 1);
        assert!(listing.sells_in(Some("US")));
        assert!(!listing.sells_in(Some("DE")));
        assert_eq!(listing.versions[0].review, ReviewStatus::Approved);
    }

    #[test]
    fn a_version_of_another_app_is_refused() {
        let catalogue = Catalogue::new();
        catalogue
            .create("list-1", "my_app", "acme", "My App", "", "tools", 100)
            .unwrap();
        assert_eq!(
            catalogue
                .add_version("list-1", release("other_app", "acme", "1.0.0"), 150)
                .unwrap_err()
                .code(),
            codes::INVALID_INPUT
        );
    }
}
