//! The designer's app draft: what the business user built, and what a package must collect.
//!
//! A no-code app is **declarative**: it has no WASM component of its own. The draft names the
//! DocTypes, views, scripts, processes, reports and fixtures that make it up; [`package`]
//! turns that list into the one `.spotapp` an installer accepts.
//! [`package`]: crate::package

use std::collections::BTreeSet;

use semver::{Version, VersionReq};
use spot_nocode_model::Draft;

use crate::error::Error;

/// A reference to one item of the app, as the designer selected it.
///
/// `DocType`, `View`, `Script`, `Rule` and `Fixture` name content that already lives on the
/// site (a definition, a view layer, a script, a rule or a fixture set) and is captured
/// through `spot-custom-bundle`. `Process` and `Report` carry their declarative definition,
/// because the site model does not own those sections yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ItemRef {
    /// A DocType installed on the site, or designed in one of the drafts.
    DocType {
        /// The DocType's storage name.
        name: String,
    },
    /// A custom field on a DocType.
    CustomField {
        /// The DocType.
        doctype: String,
        /// The field's storage name.
        field: String,
    },
    /// A site view layer for a DocType.
    View {
        /// The DocType whose view the layer changes.
        doctype: String,
    },
    /// A TypeScript script.
    Script {
        /// The script's name.
        name: String,
    },
    /// A named expression rule.
    Rule {
        /// The rule's name.
        name: String,
    },
    /// A process definition, carried as declarative text.
    Process {
        /// The process's name.
        name: String,
        /// Its definition (`toml` or `json`), carried byte for byte.
        source: String,
    },
    /// A report definition, carried as declarative text.
    Report {
        /// The report's name.
        name: String,
        /// Its definition (`toml` or `json`), carried byte for byte.
        source: String,
    },
    /// The fixture rows of one DocType.
    Fixture {
        /// The DocType whose rows travel with the app.
        doctype: String,
    },
}

impl ItemRef {
    /// The stable kind word, for messages and the declarative payload.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DocType { .. } => "doctype",
            Self::CustomField { .. } => "custom_field",
            Self::View { .. } => "view",
            Self::Script { .. } => "script",
            Self::Rule { .. } => "rule",
            Self::Process { .. } => "process",
            Self::Report { .. } => "report",
            Self::Fixture { .. } => "fixture",
        }
    }

    /// The stable key, unique within its kind.
    #[must_use]
    pub fn key(&self) -> String {
        match self {
            Self::DocType { name } => name.clone(),
            Self::CustomField { doctype, field } => format!("{doctype}.{field}"),
            Self::View { doctype } => doctype.clone(),
            Self::Script { name } | Self::Rule { name } => name.clone(),
            Self::Process { name, .. } | Self::Report { name, .. } => name.clone(),
            Self::Fixture { doctype } => doctype.clone(),
        }
    }

    /// The item as one sentence piece, for a missing-item refusal.
    fn describe(&self) -> String {
        format!("{}:{}", self.kind(), self.key())
    }
}

/// Another app this app builds on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Dependency {
    /// The app's storage name.
    pub app: String,
    /// The versions of it this app accepts.
    pub version: VersionReq,
}

impl Dependency {
    /// A dependency on `app` at `version`.
    #[must_use]
    pub fn new(app: impl Into<String>, version: VersionReq) -> Self {
        Self {
            app: app.into(),
            version,
        }
    }
}

/// Everything a designer built, before it is packaged.
///
/// The draft is plain data. [`package`](crate::package::package) validates it against the site
/// it was designed on and never mutates it.
#[derive(Clone, Debug, PartialEq)]
pub struct AppDraft {
    /// The app's storage name (`snake_case`, as `spot-manifest` accepts).
    pub name: String,
    /// Its semantic version.
    pub version: Version,
    /// The publisher that owns the app; the signing certificate must name the same id.
    pub publisher: String,
    /// The human title the admin sees.
    pub title: String,
    /// A licence reference only (an SPDX id or a store licence id); no licence text is carried.
    pub licence_ref: Option<String>,
    /// The tenant the app was designed on; drafts are validated against it.
    pub site: String,
    /// When the publisher created the package, as the caller states it (never the clock).
    pub created_at: String,
    /// The platform versions the app runs on.
    pub requires_host: Option<VersionReq>,
    /// The apps it builds on.
    pub dependencies: Vec<Dependency>,
    /// The selected items.
    pub items: Vec<ItemRef>,
    /// Newly designed DocTypes (their fields, child tables and form layouts).
    pub drafts: Vec<Draft>,
}

impl AppDraft {
    /// An empty draft for `name` at `version`, published by `publisher`.
    #[must_use]
    pub fn new(name: impl Into<String>, version: Version, publisher: impl Into<String>) -> Self {
        let name = name.into();
        Self {
            title: name.clone(),
            name,
            version,
            publisher: publisher.into(),
            licence_ref: None,
            site: String::new(),
            created_at: String::new(),
            requires_host: None,
            dependencies: Vec::new(),
            items: Vec::new(),
            drafts: Vec::new(),
        }
    }

    /// Sets the human title.
    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Sets the licence reference.
    #[must_use]
    pub fn licence(mut self, reference: impl Into<String>) -> Self {
        self.licence_ref = Some(reference.into());
        self
    }

    /// Sets the tenant the app was designed on.
    #[must_use]
    pub fn site(mut self, site: impl Into<String>) -> Self {
        self.site = site.into();
        self
    }

    /// Sets the creation time, as the caller states it.
    #[must_use]
    pub fn created_at(mut self, at: impl Into<String>) -> Self {
        self.created_at = at.into();
        self
    }

    /// Sets the host requirement.
    #[must_use]
    pub fn requires_host(mut self, range: VersionReq) -> Self {
        self.requires_host = Some(range);
        self
    }

    /// Adds a dependency.
    #[must_use]
    pub fn depends_on(mut self, dependency: Dependency) -> Self {
        self.dependencies.push(dependency);
        self
    }

    /// Adds one item reference.
    #[must_use]
    pub fn item(mut self, item: ItemRef) -> Self {
        self.items.push(item);
        self
    }

    /// Adds a newly designed DocType.
    #[must_use]
    pub fn draft(mut self, draft: Draft) -> Self {
        self.drafts.push(draft);
        self
    }

    /// The DocTypes the app declares: site references and designed drafts, sorted.
    #[must_use]
    pub fn doctypes(&self) -> Vec<String> {
        let mut names = BTreeSet::new();
        for item in &self.items {
            if let ItemRef::DocType { name } = item {
                names.insert(name.clone());
            }
        }
        for draft in &self.drafts {
            names.insert(draft.doctype().to_owned());
            for child in &draft.children {
                names.insert(child.doctype_def.name.clone());
            }
        }
        names.into_iter().collect()
    }

    /// The fixture DocTypes the app declares.
    #[must_use]
    pub fn fixture_doctypes(&self) -> BTreeSet<String> {
        self.items
            .iter()
            .filter_map(|item| match item {
                ItemRef::Fixture { doctype } => Some(doctype.clone()),
                _ => None,
            })
            .collect()
    }

    /// Refuses a draft that cannot become a package, before anything is captured.
    ///
    /// # Errors
    /// [`Error::Draft`] naming the first problem: an invalid app name, an empty publisher,
    /// site or creation time, an empty licence reference, or a duplicate item.
    pub fn validate(&self) -> Result<(), Error> {
        spot_registry::AppName::parse(&self.name).map_err(|error| {
            Error::draft(format!("`{}` is not a valid app name: {error}", self.name))
        })?;
        if self.publisher.trim().is_empty() {
            return Err(Error::draft("the publisher id is empty"));
        }
        if self.site.trim().is_empty() {
            return Err(Error::draft(
                "the site the app was designed on is not named",
            ));
        }
        if self.created_at.trim().is_empty() {
            return Err(Error::draft("the creation time is not stated"));
        }
        if self
            .licence_ref
            .as_deref()
            .is_some_and(|reference| reference.trim().is_empty())
        {
            return Err(Error::draft("the licence reference is empty"));
        }
        if self.title.trim().is_empty() {
            return Err(Error::draft("the app has no human title"));
        }
        let mut seen = BTreeSet::new();
        for item in &self.items {
            if !seen.insert((item.kind(), item.key())) {
                return Err(Error::draft(format!(
                    "the draft lists `{}` twice",
                    item.describe()
                )));
            }
        }
        let mut designed = BTreeSet::new();
        for draft in &self.drafts {
            if !designed.insert(draft.doctype().to_owned()) {
                return Err(Error::draft(format!(
                    "the draft designs the DocType `{}` twice",
                    draft.doctype()
                )));
            }
        }
        Ok(())
    }
}
