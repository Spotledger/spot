//! The section registry, the [`Section`] trait every customization kind implements, and the
//! site reader/writer traits capture and apply run against.
//!
//! # How a later WP registers a section
//!
//! A new kind (workflows, reports, print templates, BPM definitions) is one module that
//! implements [`Section`] and one call to [`SectionRegistry::register`]. Nothing else in this
//! crate changes: the registry packs every section through the same code path, the digest
//! covers whatever files the section produces, and a reader that does not know the kind
//! carries it opaquely (see [`crate::Bundle::open`]). A section that arrives later should:
//!
//! 1. pick a stable `kind` string (the directory name under `sections/`), lower-case;
//! 2. capture its state as one file per item, with a canonical name and a deterministic
//!    serialization, so capture-then-apply-then-capture is byte-identical;
//! 3. return one [`Problem`] per problem from [`Section::check`], never panic on bad input;
//! 4. give every item a stable [`Change::key`] so plan, diff and apply agree.

use std::collections::BTreeMap;

use crate::error::CustomBundleError;

/// A section's files, keyed by path relative to the section's directory.
pub type Files = BTreeMap<String, Vec<u8>>;

/// One problem found while checking a section, or one warning listed in an apply plan.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Problem {
    /// The stable `custom_bundle.*` (or pass-through `meta.*`/`view.*`/`expr.*`) code.
    pub code: String,
    /// The item the problem is about: a file, a DocType, a field, an app.
    pub item: String,
    /// One plain-language sentence.
    pub message: String,
}

/// What a change does to an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeAction {
    /// The item is absent on the target and the bundle adds it.
    Add,
    /// The item exists on both sides and differs.
    Update,
    /// The item exists on the target and the bundle does not carry it.
    Remove,
}

/// How risky a change is, for the human approving the plan.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    /// Additive only; nothing existing can be lost.
    Low,
    /// Replaces an existing value; review recommended.
    Medium,
    /// Removes something or changes behaviour; explicit approval required.
    High,
}

/// One typed change: add, update or remove one item of one section.
///
/// `key` is the item's stable identity within its section; [`Section::apply`] uses it to find
/// the item's new value in the bundle's files. `hash` is a deterministic hash of the change,
/// so two plans that do the same thing hash the same.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Change {
    /// The section kind, for example `meta` or `views`.
    pub section: String,
    /// The item's stable key within the section.
    pub key: String,
    /// What the change does.
    pub action: ChangeAction,
    /// The risk of the change.
    pub risk: Risk,
    /// Whether the change has a clear inverse the target can take back to its prior state.
    pub reversible: bool,
    /// The change's deterministic hash.
    pub hash: String,
}

impl Change {
    /// Builds a change and computes its hash from `desired` (the item's bytes in the bundle;
    /// empty for a removal).
    pub(crate) fn new(
        section: &str,
        key: &str,
        action: ChangeAction,
        risk: Risk,
        reversible: bool,
        desired: &[u8],
    ) -> Self {
        let hash = change_hash(section, key, action, desired);
        Self {
            section: section.to_owned(),
            key: key.to_owned(),
            action,
            risk,
            reversible,
            hash,
        }
    }
}

/// The action's stable name, for the hash input.
fn action_name(action: ChangeAction) -> &'static str {
    match action {
        ChangeAction::Add => "add",
        ChangeAction::Update => "update",
        ChangeAction::Remove => "remove",
    }
}

/// The deterministic hash of one change: SHA-256 over the domain prefix, the section, the
/// action, the key and the item's new bytes.
pub(crate) fn change_hash(
    section: &str,
    key: &str,
    action: ChangeAction,
    desired: &[u8],
) -> String {
    let mut input = Vec::new();
    input.extend_from_slice(crate::digest::DIGEST_PREFIX);
    input.extend_from_slice(b"change\0");
    input.extend_from_slice(section.as_bytes());
    input.push(0);
    input.extend_from_slice(action_name(action).as_bytes());
    input.push(0);
    input.extend_from_slice(key.as_bytes());
    input.push(0);
    input.extend_from_slice(desired);
    crate::digest::sha256_hex(&input)
}

/// Compares the items of a section on two sides: `current` (from the target) to `desired`
/// (from the bundle). Every item of `desired` is an add or an update; every item only in
/// `current` is a removal. The result is sorted by key, so the plan is deterministic.
pub(crate) fn plan_between(
    section: &str,
    current: &BTreeMap<String, Vec<u8>>,
    desired: &BTreeMap<String, Vec<u8>>,
    risk: impl Fn(ChangeAction) -> Risk + Copy,
) -> Vec<Change> {
    let mut changes = Vec::new();
    for (key, new) in desired {
        match current.get(key) {
            None => changes.push(Change::new(
                section,
                key,
                ChangeAction::Add,
                risk(ChangeAction::Add),
                true,
                new,
            )),
            Some(old) if old != new => changes.push(Change::new(
                section,
                key,
                ChangeAction::Update,
                risk(ChangeAction::Update),
                true,
                new,
            )),
            Some(_) => {}
        }
    }
    for key in current.keys() {
        if !desired.contains_key(key) {
            changes.push(Change::new(
                section,
                key,
                ChangeAction::Remove,
                risk(ChangeAction::Remove),
                false,
                &[],
            ));
        }
    }
    changes.sort_by(|a, b| a.key.cmp(&b.key));
    changes
}

/// One customization kind: its files, how it is captured, checked, planned and applied.
///
/// Object-safe: every method takes `&self`, there are no associated constants, so the
/// registry holds `Box<dyn Section>`.
pub trait Section: Send + Sync {
    /// The section's stable kind string (its directory name under `sections/`), lower-case.
    fn kind(&self) -> &'static str;

    /// The schema version of the files this section writes.
    fn schema_version(&self) -> u32;

    /// Validates the section's files without a site. Reports every problem at once.
    fn check(&self, files: &Files) -> Vec<Problem>;

    /// Captures this section's state from a site.
    fn capture(&self, site: &dyn SiteReader) -> Files;

    /// Decomposes the section's files into its items: a stable key to the item's canonical
    /// bytes. Two captures with the same items have the same map, whatever the file order.
    fn items(&self, files: &Files) -> BTreeMap<String, Vec<u8>>;

    /// The risk of one action for this section.
    fn risk(&self, action: ChangeAction) -> Risk;

    /// Plans the application of this section's files to a target site: the target's items
    /// against the bundle's.
    fn plan_apply(&self, files: &Files, site: &dyn SiteReader) -> Vec<Change> {
        let current = self.items(&self.capture(site));
        let desired = self.items(files);
        plan_between(self.kind(), &current, &desired, |action| self.risk(action))
    }

    /// Applies one change to a site, reading the item's new value from the bundle's files.
    fn apply(
        &self,
        change: &Change,
        files: &Files,
        site: &mut dyn SiteWriter,
    ) -> Result<(), CustomBundleError>;
}

/// One script as the bundle carries it: the TypeScript source and the hash spot-ts records
/// for it. The transpiled JavaScript and bytecode are rebuilt on apply and are never stored.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ScriptRecord {
    /// The script's name within the site.
    pub name: String,
    /// The app that owns the script.
    pub app: String,
    /// The hook point the script is bound to.
    pub hook_point: String,
    /// The TypeScript source.
    pub source: String,
    /// The SHA-256 of `source`, as 64 lowercase hex digits (spot-ts's `source_hash`).
    pub hash: String,
}

/// One rule as the bundle carries it: a name and an expression string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuleRecord {
    /// The rule's name.
    pub name: String,
    /// The expression text.
    pub expression: String,
}

/// One DocType's fixtures: the JSON documents a site added.
#[derive(Clone, Debug, PartialEq)]
pub struct FixtureSet {
    /// The DocType the documents belong to.
    pub doctype: String,
    /// The documents.
    pub documents: Vec<serde_json::Value>,
}

/// Reads a site's customization state for capture and planning.
pub trait SiteReader {
    /// The installed apps and their versions, in any order.
    fn installed_apps(&self) -> Vec<(String, semver::Version)>;

    /// The host/platform version.
    fn host_version(&self) -> semver::Version;

    /// The app definitions the site has installed.
    fn definitions(&self) -> Vec<spot_meta::Definition>;

    /// The site's custom fields and property overrides.
    fn customisations(&self) -> spot_meta::Customisations;

    /// Every view layer on the site (never a `standard` layer).
    fn view_layers(&self) -> Vec<spot_views::ViewLayer>;

    /// The app's base view for a DocType, used to resolve and validate layers.
    fn base_view(&self, doctype: &str) -> Option<spot_views::ViewDef>;

    /// Every script on the site.
    fn scripts(&self) -> Vec<ScriptRecord>;

    /// Every rule on the site.
    fn rules(&self) -> Vec<RuleRecord>;

    /// Every fixture set on the site.
    fn fixtures(&self) -> Vec<FixtureSet>;

    /// Whether the target marks an item as system-managed (capture skips it; apply refuses).
    /// The item string is the same key [`Change::key`] carries.
    fn is_system_managed(&self, kind: &str, item: &str) -> bool;

    /// Whether the target marks an item as secret (capture skips it; apply refuses).
    fn is_secret(&self, kind: &str, item: &str) -> bool;
}

/// Writes a site's customization state for apply.
///
/// Every method is called inside a [`begin`](SiteWriter::begin)/[`commit`](SiteWriter::commit)
/// or [`rollback`](SiteWriter::rollback) window: an implementation buffers the writes and only
/// publishes them on commit, so a failure part-way leaves the site unchanged.
pub trait SiteWriter: SiteReader {
    /// Begins a transaction.
    fn begin(&mut self) -> Result<(), CustomBundleError>;
    /// Commits the transaction.
    fn commit(&mut self) -> Result<(), CustomBundleError>;
    /// Rolls back the transaction, restoring the state at `begin`.
    fn rollback(&mut self) -> Result<(), CustomBundleError>;

    /// Adds or replaces a custom field, keeping its overrides.
    fn upsert_custom_field(
        &mut self,
        doctype: &str,
        spec: spot_meta::FieldSpec,
        insert_after: Option<&str>,
    ) -> Result<(), CustomBundleError>;
    /// Removes a custom field and the overrides that name it.
    fn remove_custom_field(&mut self, doctype: &str, name: &str) -> Result<(), CustomBundleError>;
    /// Sets one property of a field (`Some`) or of the DocType (`None`).
    fn set_override(
        &mut self,
        doctype: &str,
        field: Option<&str>,
        property: spot_meta::Property,
    ) -> Result<(), CustomBundleError>;
    /// Clears one property of a field (`Some`) or of the DocType (`None`).
    fn clear_override(
        &mut self,
        doctype: &str,
        field: Option<&str>,
        key: &str,
    ) -> Result<(), CustomBundleError>;

    /// Adds or replaces a view layer.
    fn upsert_view_layer(&mut self, layer: spot_views::ViewLayer) -> Result<(), CustomBundleError>;
    /// Removes a view layer by DocType and kind.
    fn remove_view_layer(
        &mut self,
        doctype: &str,
        kind: &spot_views::LayerKind,
    ) -> Result<(), CustomBundleError>;

    /// Adds or replaces a script.
    fn upsert_script(&mut self, record: ScriptRecord) -> Result<(), CustomBundleError>;
    /// Removes a script.
    fn remove_script(&mut self, name: &str) -> Result<(), CustomBundleError>;

    /// Adds or replaces a rule.
    fn upsert_rule(&mut self, name: &str, expression: &str) -> Result<(), CustomBundleError>;
    /// Removes a rule.
    fn remove_rule(&mut self, name: &str) -> Result<(), CustomBundleError>;

    /// Replaces a DocType's fixtures.
    fn set_fixtures(
        &mut self,
        doctype: &str,
        documents: Vec<serde_json::Value>,
    ) -> Result<(), CustomBundleError>;
    /// Removes a DocType's fixtures.
    fn remove_fixtures(&mut self, doctype: &str) -> Result<(), CustomBundleError>;
}

/// The registry of every known section kind, in the order sections are packed.
pub struct SectionRegistry {
    sections: Vec<Box<dyn Section>>,
}

impl SectionRegistry {
    /// A registry with the sections this crate ships: `meta`, `views`, `scripts`, `rules`,
    /// `fixtures`.
    pub fn new() -> Self {
        let mut registry = Self {
            sections: Vec::new(),
        };
        registry.register(Box::new(crate::section::MetaSection));
        registry.register(Box::new(crate::section::ViewsSection));
        registry.register(Box::new(crate::section::ScriptsSection));
        registry.register(Box::new(crate::section::RulesSection));
        registry.register(Box::new(crate::section::FixturesSection));
        registry
    }

    /// Registers a section. A later WP's section is one module plus this call.
    ///
    /// A kind already registered is replaced, so a crate can override a built-in section.
    pub fn register(&mut self, section: Box<dyn Section>) {
        let kind = section.kind().to_owned();
        self.sections.retain(|existing| existing.kind() != kind);
        self.sections.push(section);
        self.sections.sort_by_key(|section| section.kind());
    }

    /// The section for a kind, if known.
    pub fn get(&self, kind: &str) -> Option<&dyn Section> {
        self.sections
            .iter()
            .find(|section| section.kind() == kind)
            .map(|section| section.as_ref())
    }

    /// The kinds the registry knows, in pack order (sorted).
    pub fn kinds(&self) -> Vec<&'static str> {
        self.sections.iter().map(|section| section.kind()).collect()
    }

    /// The registry's sections, in pack order.
    pub fn iter(&self) -> impl Iterator<Item = &dyn Section> {
        self.sections.iter().map(|section| section.as_ref())
    }
}

impl Default for SectionRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_kinds_are_sorted() {
        let registry = SectionRegistry::new();
        assert_eq!(
            registry.kinds(),
            vec!["fixtures", "meta", "rules", "scripts", "views"]
        );
    }

    #[test]
    fn plan_between_adds_updates_and_removes() {
        let mut current = BTreeMap::new();
        current.insert("kept".to_owned(), b"same".to_vec());
        current.insert("changed".to_owned(), b"old".to_vec());
        current.insert("gone".to_owned(), b"x".to_vec());
        let mut desired = BTreeMap::new();
        desired.insert("kept".to_owned(), b"same".to_vec());
        desired.insert("changed".to_owned(), b"new".to_vec());
        desired.insert("added".to_owned(), b"y".to_vec());
        let changes = plan_between("meta", &current, &desired, |_| Risk::Low);
        let seen: Vec<(&str, ChangeAction)> = changes
            .iter()
            .map(|change| (change.key.as_str(), change.action))
            .collect();
        assert_eq!(
            seen,
            vec![
                ("added", ChangeAction::Add),
                ("changed", ChangeAction::Update),
                ("gone", ChangeAction::Remove),
            ]
        );
    }

    #[test]
    fn change_hash_is_stable() {
        let one = Change::new(
            "meta",
            "sales_invoice.custom_x",
            ChangeAction::Add,
            Risk::Low,
            true,
            b"x",
        );
        let two = Change::new(
            "meta",
            "sales_invoice.custom_x",
            ChangeAction::Add,
            Risk::Low,
            true,
            b"x",
        );
        assert_eq!(one.hash, two.hash);
        let three = Change::new(
            "meta",
            "sales_invoice.custom_x",
            ChangeAction::Add,
            Risk::Low,
            true,
            b"y",
        );
        assert_ne!(one.hash, three.hash);
    }
}
