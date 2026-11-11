//! The pure update decision: given what is installed, what is offered, the policy and `now`,
//! return [`Apply`](Decision::Apply), [`StepVia`](Decision::StepVia), [`Skip`](Decision::Skip) or
//! [`Refuse`](Decision::Refuse).
//!
//! The decision runs no signature or digest check; that is [`verify_update`](crate::verify_update)
//! and [`verify_payload`](crate::verify_payload). It answers only "should this verified update run
//! on this install?". The rules, in order:
//!
//! 1. the product must be the installed product;
//! 2. the channel must be one the policy allows;
//! 3. **an older version than installed is refused** ([`UpdaterErrorCode::Downgrade`]);
//! 4. the same version is skipped, unless its payload digest differs, which is
//!    [`UpdaterErrorCode::DigestChanged`];
//! 5. a manifest dated in the future is refused;
//! 6. an offered schema epoch below the installed database's is refused, and one above it needs
//!    the installed epoch listed in `migrations`;
//! 7. an installed version older than `min_from` steps through a listed `via`, or is refused
//!    when none is listed.
//!
//! A signed `allow_downgrade` is not supported: rollback is the supported way back.

use semver::Version;

use crate::digest::Digest;
use crate::error::UpdaterErrorCode;
use crate::manifest::{Channel, UpdateManifest};

/// The tolerance before a manifest's `released_at` counts as being in the future, so a small
/// clock skew does not refuse a freshly released update.
pub const RELEASE_TOLERANCE_SECS: i64 = 86_400;

/// The update currently installed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Installed {
    /// The installed product.
    pub product: String,
    /// The installed version.
    pub version: Version,
    /// The installed channel.
    pub channel: Channel,
    /// The schema epoch the installed database is at.
    pub schema_epoch: u64,
    /// The installed payload's SHA-256, when it is known; a same-version offer with a different
    /// digest is then refused.
    pub payload_sha256: Option<Digest>,
}

/// The offered update. The manifest is exactly what `decide` needs, so it is the type itself.
pub type Offered = UpdateManifest;

/// What the update policy allows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdatePolicy {
    /// The channels an update may come from.
    pub allowed_channels: Vec<Channel>,
    /// The post-commit probation window, in seconds: a crash of the new version inside it rolls
    /// back on the next [`tick`](crate::UpdateEngine::tick), one outside it is kept.
    pub probation_secs: u64,
    /// How many version directories to keep after a commit; the active and last-good versions
    /// are always kept, whatever this says.
    pub retention: usize,
    /// How long, in seconds, the new version has to become ready after it is started.
    pub health_timeout_secs: i64,
    /// The numerator of the free-space factor: unpacking needs the payload size times
    /// `free_space_num / free_space_den` free before it starts.
    pub free_space_num: u64,
    /// The denominator of the free-space factor.
    pub free_space_den: u64,
}

impl Default for UpdatePolicy {
    fn default() -> Self {
        Self {
            allowed_channels: vec![Channel::Stable, Channel::Beta],
            probation_secs: 300,
            retention: 3,
            health_timeout_secs: 60,
            free_space_num: 2,
            free_space_den: 1,
        }
    }
}

/// What to do with an offered update.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Apply the update directly.
    Apply,
    /// Step through the named version first.
    StepVia(Version),
    /// Do nothing, for the given reason.
    Skip(String),
    /// Refuse, with the code that names why.
    Refuse(UpdaterErrorCode),
}

/// Decides what to do with `offered` on `installed` under `policy` at `now`. See the module docs
/// for the order.
#[must_use]
pub fn decide(
    installed: &Installed,
    offered: &Offered,
    policy: &UpdatePolicy,
    now: i64,
) -> Decision {
    if installed.product != offered.product {
        return Decision::Refuse(UpdaterErrorCode::ProductMismatch);
    }
    if !policy.allowed_channels.contains(&offered.channel) {
        return Decision::Refuse(UpdaterErrorCode::ChannelNotAllowed);
    }
    if offered.version < installed.version {
        return Decision::Refuse(UpdaterErrorCode::Downgrade);
    }
    if offered.version == installed.version {
        return match &installed.payload_sha256 {
            Some(installed_digest) if *installed_digest != offered.payload.sha256 => {
                Decision::Refuse(UpdaterErrorCode::DigestChanged)
            }
            _ => Decision::Skip("this version is already installed".to_owned()),
        };
    }
    if offered.released_at > now.saturating_add(RELEASE_TOLERANCE_SECS) {
        return Decision::Refuse(UpdaterErrorCode::NotReleased);
    }
    if offered.schema_epoch < installed.schema_epoch {
        return Decision::Refuse(UpdaterErrorCode::SchemaEpoch);
    }
    if offered.schema_epoch > installed.schema_epoch
        && !offered.migrations.contains(&installed.schema_epoch)
    {
        return Decision::Refuse(UpdaterErrorCode::SchemaEpoch);
    }
    if installed.version < offered.min_from {
        return match offered
            .via
            .iter()
            .find(|via| **via > installed.version && **via <= offered.version)
        {
            Some(step) => Decision::StepVia(step.clone()),
            None => Decision::Refuse(UpdaterErrorCode::MinFrom),
        };
    }
    Decision::Apply
}
