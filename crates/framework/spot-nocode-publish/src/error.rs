//! The typed refusals of packaging, publishing and installing a no-code app.
//!
//! Every variant carries a stable `nocode_publish.*` code ([`Error::code`]) and one
//! plain-language sentence naming the item, never a secret or a signature. The review
//! refusal keeps the blocker sentences so the publisher can fix them.

use crate::codes;

/// Why a no-code app could not be packaged, signed, published or installed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The draft itself is not publishable.
    #[error("the app draft is not publishable: {message}")]
    Draft {
        /// The plain sentence naming the problem.
        message: String,
    },

    /// An item the draft names is not on the site.
    #[error("`{item}` is not available on the site this app was designed on")]
    MissingItem {
        /// The item reference, such as `script:asset_rules`.
        item: String,
    },

    /// The site carries fixture rows for a DocType the draft does not declare.
    #[error("the site carries fixture rows for `{doctype}`, which the draft does not declare")]
    DataNotDeclared {
        /// The undeclared DocType.
        doctype: String,
    },

    /// The generated manifest does not pass `spot-manifest`.
    #[error("the generated manifest is not valid: {messages}")]
    Manifest {
        /// Every `manifest.*` problem, one sentence each.
        messages: String,
    },

    /// The packed bytes do not pass the validator the installer uses.
    #[error("the packed app bundle is not valid: {messages}")]
    Bundle {
        /// Every `bundle.*` problem, one sentence each.
        messages: String,
    },

    /// The manifest asks for more than the host policy allows.
    #[error("the app asks for more than the host allows: {messages}")]
    OverPrivileged {
        /// Every `bundle.over_privileged` problem, one sentence each.
        messages: String,
    },

    /// The captured customisation bundle is malformed.
    #[error("the captured site customisations are not valid: {message}")]
    CustomBundle {
        /// One plain sentence from `spot-custom-bundle`.
        message: String,
    },

    /// The review pipeline found a blocker.
    #[error("the app store review rejected the bundle: {blockers}")]
    ReviewRejected {
        /// The blocker findings, one sentence each.
        blockers: String,
    },

    /// The same semantic version is already published.
    #[error("`{app}` {version} is already published; a new version must be higher")]
    VersionExists {
        /// The app name.
        app: String,
        /// The version already published.
        version: String,
    },

    /// The new version does not exceed every published version.
    #[error("`{app}` {version} does not exceed the published {latest}; versions must increase")]
    VersionNotIncreasing {
        /// The app name.
        app: String,
        /// The new version.
        version: String,
        /// The highest published version.
        latest: String,
    },

    /// A private release names an organisation that does not own the app.
    #[error("`{publisher}` may not publish privately for `{organisation}`")]
    PrivateOtherOrganisation {
        /// The publisher the certificate names.
        publisher: String,
        /// The organisation the caller asked for.
        organisation: String,
    },

    /// The signature is missing, does not verify, or names another identity.
    #[error("the publisher signature is not usable: {message}")]
    Signature {
        /// One plain sentence from the signer or the verifier.
        message: String,
    },

    /// The store registry refused the release.
    #[error("the store registry refused the release: {message}")]
    Registry {
        /// The registry's sentence and stable `registry.*` code.
        message: String,
    },

    /// The installer, executor or customisation apply refused the install.
    #[error("the install did not complete: {message}")]
    Install {
        /// One plain sentence naming the step that refused.
        message: String,
    },
}

impl Error {
    /// The stable `nocode_publish.*` code for this refusal.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Draft { .. } => codes::BAD_DRAFT,
            Self::MissingItem { .. } => codes::MISSING_ITEM,
            Self::DataNotDeclared { .. } => codes::DATA_NOT_DECLARED,
            Self::Manifest { .. } => codes::BAD_MANIFEST,
            Self::Bundle { .. } => codes::BAD_BUNDLE,
            Self::OverPrivileged { .. } => codes::OVER_PRIVILEGED,
            Self::CustomBundle { .. } => codes::BAD_CUSTOM_BUNDLE,
            Self::ReviewRejected { .. } => codes::REVIEW_REJECTED,
            Self::VersionExists { .. } => codes::VERSION_EXISTS,
            Self::VersionNotIncreasing { .. } => codes::VERSION_NOT_INCREASING,
            Self::PrivateOtherOrganisation { .. } => codes::PRIVATE_OTHER_ORGANISATION,
            Self::Signature { .. } => codes::SIGNATURE,
            Self::Registry { .. } => codes::REGISTRY,
            Self::Install { .. } => codes::INSTALL,
        }
    }

    /// A draft refusal with one plain sentence.
    pub(crate) fn draft(message: impl Into<String>) -> Self {
        Self::Draft {
            message: message.into(),
        }
    }

    /// A bundle refusal from the validator's error list.
    pub(crate) fn bundle(errors: Vec<spot_bundle::BundleError>) -> Self {
        Self::Bundle {
            messages: join_bundle(errors),
        }
    }

    /// An over-privilege refusal from the policy check.
    pub(crate) fn over_privileged(errors: Vec<spot_bundle::BundleError>) -> Self {
        Self::OverPrivileged {
            messages: join_bundle(errors),
        }
    }
}

/// Joins bundle errors into one sentence list without losing the stable codes.
pub(crate) fn join_bundle(errors: Vec<spot_bundle::BundleError>) -> String {
    errors
        .iter()
        .map(|error| match &error.path {
            Some(path) => format!("{} ({path}): {}", error.code, error.message),
            None => format!("{}: {}", error.code, error.message),
        })
        .collect::<Vec<_>>()
        .join("; ")
}

impl From<spot_bundle_signing::SigningError> for Error {
    fn from(error: spot_bundle_signing::SigningError) -> Self {
        Self::Signature {
            message: format!("{}: {error}", error.code()),
        }
    }
}

impl From<spot_custom_bundle::CustomBundleError> for Error {
    fn from(error: spot_custom_bundle::CustomBundleError) -> Self {
        Self::CustomBundle {
            message: format!("{}: {error}", error.code()),
        }
    }
}

impl From<spot_registry::RegistryError> for Error {
    fn from(error: spot_registry::RegistryError) -> Self {
        Self::Registry {
            message: format!("{}: {error}", error.code),
        }
    }
}
