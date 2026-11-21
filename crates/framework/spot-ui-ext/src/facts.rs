//! What the validator may refer to: a contribution, its facts and the site.

use std::collections::BTreeSet;

use spot_manifest::AppManifest;
use spot_meta::EffectiveMeta;

use crate::limits;

/// One method an app's manifest declares, with the argument names its call accepts.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct MethodFacts {
    /// The DocType the method belongs to.
    pub doctype: String,
    /// The method's name.
    pub name: String,
    /// The argument names a `call` may pass; empty when the manifest declares no signature.
    pub args: Vec<String>,
}

impl MethodFacts {
    /// A method with no declared argument names.
    #[must_use]
    pub fn new(doctype: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            doctype: doctype.into(),
            name: name.into(),
            args: Vec::new(),
        }
    }
}

/// Whether a query returns rows or one aggregate number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryKind {
    /// Rows, one per document, with named columns.
    Rows,
    /// One aggregated number.
    Aggregate,
}

impl QueryKind {
    /// The kind's name, as findings and the admin explain spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rows => "rows",
            Self::Aggregate => "aggregate",
        }
    }

    /// Parses `rows` or `aggregate`; anything else is `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "rows" => Some(Self::Rows),
            "aggregate" => Some(Self::Aggregate),
            _ => None,
        }
    }
}

/// One server query an app declares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryFacts {
    /// The query id, `app.name`.
    pub id: String,
    /// Whether it returns rows or one number.
    pub kind: QueryKind,
    /// The DocType the query reads.
    pub doctype: String,
    /// The column names a rows query returns; empty when the caller does not know them.
    pub columns: Vec<String>,
    /// The permission a caller needs to read it; when absent, `<doctype>:read` is used if it
    pub permission: Option<String>,
}

impl QueryFacts {
    /// A query with no columns and no explicit read permission.
    #[must_use]
    pub fn new(id: impl Into<String>, kind: QueryKind, doctype: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            kind,
            doctype: doctype.into(),
            columns: Vec::new(),
            permission: None,
        }
    }

    /// The query with its column names.
    #[must_use]
    pub fn with_columns<I, S>(mut self, columns: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.columns = columns.into_iter().map(Into::into).collect();
        self
    }
}

/// One DocType the validator may check field references against.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct DoctypeFacts {
    /// The DocType's storage name.
    pub name: String,
    /// Its field names.
    pub fields: Vec<String>,
}

/// Everything the validator may look up for one app.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppFacts {
    /// The declared methods.
    pub methods: Vec<MethodFacts>,
    /// The declared queries.
    pub queries: Vec<QueryFacts>,
    /// The permission references that exist.
    pub permissions: BTreeSet<String>,
    /// The registered destinations.
    pub destinations: BTreeSet<String>,
    /// The routes the app's manifest declares, as absolute paths.
    pub routes: Vec<String>,
    /// The DocTypes and their fields.
    pub doctypes: Vec<DoctypeFacts>,
    /// The icon tokens the caller accepts; `None` means the contract's closed set.
    pub icons: Option<BTreeSet<String>>,
}

impl AppFacts {
    /// Empty facts: the contract's icon set, and no methods, queries, permissions or DocTypes.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one declared method.
    #[must_use]
    pub fn with_method(mut self, method: MethodFacts) -> Self {
        self.methods.push(method);
        self
    }

    /// Adds one declared query.
    #[must_use]
    pub fn with_query(mut self, query: QueryFacts) -> Self {
        self.queries.push(query);
        self
    }

    /// Adds one existing permission reference.
    #[must_use]
    pub fn with_permission(mut self, permission: impl Into<String>) -> Self {
        self.permissions.insert(permission.into());
        self
    }

    /// Adds one registered destination.
    #[must_use]
    pub fn with_destination(mut self, destination: impl Into<String>) -> Self {
        self.destinations.insert(destination.into());
        self
    }

    /// Adds one route the app's manifest declares.
    #[must_use]
    pub fn with_route(mut self, route: impl Into<String>) -> Self {
        self.routes.push(route.into());
        self
    }

    /// Adds one DocType and its fields.
    #[must_use]
    pub fn with_doctype(mut self, doctype: DoctypeFacts) -> Self {
        self.doctypes.push(doctype);
        self
    }

    /// Replaces the accepted icon tokens with the caller's own set.
    #[must_use]
    pub fn with_icons<I, S>(mut self, icons: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.icons = Some(icons.into_iter().map(Into::into).collect());
        self
    }

    /// Adds the methods and routes an app's manifest declares.
    #[must_use]
    pub fn with_manifest(mut self, manifest: &AppManifest) -> Self {
        for method in &manifest.methods {
            self.methods.push(MethodFacts::new(
                method.doctype.clone(),
                method.name.to_string(),
            ));
        }
        for route in &manifest.routes {
            self.routes.push(route.path.as_str().to_owned());
        }
        self
    }

    /// Adds every DocType of `meta` with its field names.
    #[must_use]
    pub fn with_meta(mut self, meta: &EffectiveMeta) -> Self {
        for definition in meta.definitions() {
            let fields = definition
                .fields
                .iter()
                .map(|field| field.name.clone())
                .collect();
            if let Some(existing) = self
                .doctypes
                .iter_mut()
                .find(|doctype| doctype.name == definition.name)
            {
                existing.fields = fields;
            } else {
                self.doctypes.push(DoctypeFacts {
                    name: definition.name.clone(),
                    fields,
                });
            }
        }
        self
    }

    /// One declared method.
    #[must_use]
    pub fn method(&self, doctype: &str, name: &str) -> Option<&MethodFacts> {
        self.methods
            .iter()
            .find(|method| method.doctype == doctype && method.name == name)
    }

    /// One declared query.
    #[must_use]
    pub fn query(&self, id: &str) -> Option<&QueryFacts> {
        self.queries.iter().find(|query| query.id == id)
    }

    /// One known DocType.
    #[must_use]
    pub fn doctype(&self, name: &str) -> Option<&DoctypeFacts> {
        self.doctypes.iter().find(|doctype| doctype.name == name)
    }

    /// Whether the app may use `token` as an icon.
    #[must_use]
    pub fn icon_known(&self, token: &str) -> bool {
        match &self.icons {
            Some(icons) => icons.contains(token),
            None => limits::is_icon_token(token),
        }
    }

    /// Whether one of the app's declared routes covers `path`.
    #[must_use]
    pub fn route_covers(&self, path: &str) -> bool {
        self.routes
            .iter()
            .any(|route| path == route || path.starts_with(&format!("{route}/")))
    }

    /// The permission a caller needs to read `query`, if the facts state one.
    #[must_use]
    pub fn query_permission(&self, query: &str) -> Option<String> {
        let info = self.query(query)?;
        if let Some(permission) = &info.permission {
            return Some(permission.clone());
        }
        let read = format!("{}:read", info.doctype);
        self.permissions.contains(&read).then_some(read)
    }
}

/// One app's contribution: its identity, the apps it depends on, the files it declares and the
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppContribution {
    /// The app's name.
    pub app: String,
    /// The installed version of the app.
    pub version: String,
    /// The apps this app declares a dependency on (ADR-17).
    pub depends: Vec<String>,
    /// The app's documents as `(file, text)`, in any order.
    pub documents: Vec<(String, String)>,
    /// What the documents may refer to.
    pub facts: AppFacts,
}

impl AppContribution {
    /// A contribution with no documents and no facts.
    #[must_use]
    pub fn new(app: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            app: app.into(),
            version: version.into(),
            depends: Vec::new(),
            documents: Vec::new(),
            facts: AppFacts::default(),
        }
    }

    /// Declares a dependency on `app`.
    #[must_use]
    pub fn with_dependency(mut self, app: impl Into<String>) -> Self {
        self.depends.push(app.into());
        self
    }

    /// Adds one document.
    #[must_use]
    pub fn with_document(mut self, file: impl Into<String>, text: impl Into<String>) -> Self {
        self.documents.push((file.into(), text.into()));
        self
    }

    /// Sets the facts and returns the contribution.
    #[must_use]
    pub fn with_facts(mut self, facts: AppFacts) -> Self {
        self.facts = facts;
        self
    }
}

/// One app installed on the site, with the version the site runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InstalledApp {
    /// The app's name.
    pub app: String,
    /// Its installed version.
    pub version: String,
}

impl InstalledApp {
    /// An installed app.
    #[must_use]
    pub fn new(app: impl Into<String>, version: impl Into<String>) -> Self {
        Self {
            app: app.into(),
            version: version.into(),
        }
    }
}

/// What the site knows beyond the contributions: the installed apps and the shell's contract
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SiteContext {
    /// The installed apps and their versions.
    pub installed: Vec<InstalledApp>,
    /// The contract version the site's shell implements.
    pub shell_contract: String,
}

impl SiteContext {
    /// A site with the given installed apps and shell contract version.
    #[must_use]
    pub fn new(installed: Vec<InstalledApp>, shell_contract: impl Into<String>) -> Self {
        Self {
            installed,
            shell_contract: shell_contract.into(),
        }
    }

    /// The installed version of `app`, when the site lists it.
    #[must_use]
    pub fn version_of(&self, app: &str) -> Option<&str> {
        self.installed
            .iter()
            .find(|installed| installed.app == app)
            .map(|installed| installed.version.as_str())
    }
}

/// The permission set a resolved bundle is filtered for.
pub trait PermissionSet {
    /// Whether the set holds `permission`, a `<doctype>:<right>` reference.
    fn has(&self, permission: &str) -> bool;
}

impl<F> PermissionSet for F
where
    F: Fn(&str) -> bool,
{
    fn has(&self, permission: &str) -> bool {
        self(permission)
    }
}

/// A permission set held as a sorted list of references.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionList {
    permissions: BTreeSet<String>,
}

impl PermissionList {
    /// An empty set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one permission reference.
    pub fn insert(&mut self, permission: impl Into<String>) {
        self.permissions.insert(permission.into());
    }
}

impl FromIterator<String> for PermissionList {
    fn from_iter<T: IntoIterator<Item = String>>(iter: T) -> Self {
        Self {
            permissions: iter.into_iter().collect(),
        }
    }
}

impl PermissionSet for PermissionList {
    fn has(&self, permission: &str) -> bool {
        self.permissions.contains(permission)
    }
}

/// A set that holds every permission.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AllowAll;

impl PermissionSet for AllowAll {
    fn has(&self, _permission: &str) -> bool {
        true
    }
}
