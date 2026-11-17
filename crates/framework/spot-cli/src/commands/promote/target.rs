//! The target site as the promotion service sees it: an in-memory image of the site's
//! customisations that writes through to the site's customisation records.
//!
//! * The image is the installed apps' definitions plus the **wanted** custom fields and property
//!   records (`applied` and `pending`: a pending record is a change the host finishes at its next
//!   start, so planning and verifying see it).
//! * A write is buffered inside the `begin`/`commit` window. `commit` publishes the buffered
//!   record operations **atomically** to the record store (`pending`, with the next intent
//!   number) and the host's own recovery applies the schema change; `rollback` drops them.
//! * Views, scripts, rules and fixtures have no live store yet: a commit that would change one
//!   is **refused** (`custom_bundle.apply_failed`), so a promotion never reports a write that
//!   did not happen.

use std::sync::Arc;

use spot_custom_bundle::error::codes;
use spot_custom_bundle::registry::{FixtureSet, RuleRecord, ScriptRecord, SiteReader, SiteWriter};
use spot_custom_bundle::{CustomBundleError, FakeSite};
use spot_customise::{
    FieldRecord, PropertyRecord, RecordKey, RecordOp, RecordState, RecordStore, Records,
    StorageRecordStore,
};
use spot_environments::EnvRef;
use spot_meta::{Customisations, Definition, FieldSpec, Property};
use spot_postgres::PgBackend;
use spot_promotion::TargetSite;
use spot_storage_contract::Catalog;

use super::super::bundle::{AppliedMeta, ImageBase, build_image};

/// What a site's image is built from: the installed apps and their definitions.
#[derive(Clone)]
pub struct Parts {
    /// `(app, version)` of every installed app.
    pub apps: Vec<(String, String)>,
    /// The host version.
    pub host: String,
    /// The installed definitions.
    pub definitions: Vec<Definition>,
}

impl Parts {
    /// The parts of `site` from the host configuration and the app catalogue.
    ///
    /// # Errors
    /// [`spot_cli::Error`] when the configuration or an app cannot be read.
    pub fn of_site(root: &std::path::Path, site: &str) -> spot_cli::Result<Parts> {
        let loaded = spot_cli::config::load(root, false)?;
        let site_config = loaded
            .sites
            .iter()
            .find(|s| s.site == site)
            .ok_or_else(|| spot_cli::Error(format!("there is no site `{site}`")))?
            .clone();
        let host = env!("CARGO_PKG_VERSION").to_owned();
        let catalogue = spot_cli::apps::catalogue(root, &loaded)?;
        let names: Vec<String> = if site_config.apps.is_empty() {
            catalogue.iter().map(|a| a.name.clone()).collect()
        } else {
            site_config.apps.clone()
        };
        let apps = names
            .into_iter()
            .map(|name| {
                let version = catalogue
                    .iter()
                    .find(|a| a.name == name)
                    .map_or_else(|| host.clone(), |a| a.version.clone());
                (name, version)
            })
            .collect();
        let definitions = spot_cli::apps::installed_definitions(root, &loaded, &site_config)?;
        Ok(Parts {
            apps,
            host,
            definitions,
        })
    }
}

/// How `commit` publishes the buffered operations.
pub enum Publish {
    /// Synchronously, from a thread outside any runtime, through the runtime `handle`.
    Now(tokio::runtime::Handle),
    /// Keep them for the caller to publish ([`FlushingSite::take_committed`]); for code that is
    /// already inside the runtime.
    Deferred,
}

/// The fingerprints of the sections that have no live store; a change to one is refused.
struct Unstored {
    layers: String,
    scripts: String,
    rules: String,
    fixtures: String,
}

impl Unstored {
    fn of(site: &FakeSite) -> Unstored {
        Unstored {
            layers: format!("{:?}", site.view_layers()),
            scripts: format!("{:?}", site.scripts()),
            rules: format!("{:?}", site.rules()),
            fixtures: format!("{:?}", site.fixtures()),
        }
    }

    fn same_as(&self, site: &FakeSite) -> bool {
        let now = Unstored::of(site);
        self.layers == now.layers
            && self.scripts == now.scripts
            && self.rules == now.rules
            && self.fixtures == now.fixtures
    }
}

/// The in-memory site that writes through to the record store.
pub struct FlushingSite {
    image: FakeSite,
    baseline: Unstored,
    store: Arc<dyn RecordStore>,
    records: Records,
    actor: String,
    publish: Publish,
    buffered: Vec<RecordOp>,
    committed: Vec<RecordOp>,
}

fn failed(message: &str) -> CustomBundleError {
    CustomBundleError::new(codes::APPLY_FAILED, message)
}

fn unstored(what: &str) -> CustomBundleError {
    CustomBundleError::new(
        codes::APPLY_FAILED,
        format!("this site has no live store for {what}; the change was not written"),
    )
}

/// The text form of an override's value: the inverse of `Property::parse` for the value part.
fn property_text(property: &Property) -> String {
    match property {
        Property::Label(v)
        | Property::TitleField(v)
        | Property::FetchFrom(Some(v))
        | Property::DependsOn(Some(v))
        | Property::MandatoryDependsOn(Some(v))
        | Property::ReadOnlyDependsOn(Some(v))
        | Property::Default(Some(v))
        | Property::Description(Some(v))
        | Property::ProtectionGroup(Some(v))
        | Property::Control(Some(v)) => v.clone(),
        Property::Default(None)
        | Property::Description(None)
        | Property::DependsOn(None)
        | Property::MandatoryDependsOn(None)
        | Property::ReadOnlyDependsOn(None)
        | Property::FetchFrom(None)
        | Property::ProtectionGroup(None)
        | Property::Control(None) => String::new(),
        Property::Required(v)
        | Property::ReadOnly(v)
        | Property::AllowOnPosted(v)
        | Property::Hidden(v)
        | Property::InListView(v)
        | Property::InStandardFilter(v)
        | Property::NoCopy(v) => v.to_string(),
        Property::MaxChars(v) => v.to_string(),
        Property::Permlevel(v) => v.to_string(),
        Property::Options(values) | Property::Classification(values) => values.join("\n"),
        Property::Lookup(spec) => spec
            .as_ref()
            .map(|spec| toml::to_string(spec).unwrap_or_default())
            .unwrap_or_default(),
        Property::Masking(spec) => spec
            .as_ref()
            .map(|spec| toml::to_string(spec).unwrap_or_default())
            .unwrap_or_default(),
    }
}

fn next_intent(records: &Records) -> u64 {
    records
        .fields
        .iter()
        .map(|r| r.intent)
        .chain(records.properties.iter().map(|r| r.intent))
        .chain(records.doctypes.iter().map(|r| r.intent))
        .max()
        .map_or(1, |n| n.saturating_add(1))
}

impl FlushingSite {
    /// The operations committed so far, taken out (for [`Publish::Deferred`]).
    pub fn take_committed(&mut self) -> Vec<RecordOp> {
        std::mem::take(&mut self.committed)
    }

    /// The image, read only.
    #[must_use]
    pub fn image(&self) -> &FakeSite {
        &self.image
    }

    fn intent(&self) -> u64 {
        next_intent(&self.records)
    }

    fn property_record(
        &self,
        doctype: &str,
        field: Option<&str>,
        key: &str,
        value: String,
        state: RecordState,
    ) -> PropertyRecord {
        PropertyRecord {
            doctype: doctype.to_owned(),
            field: field.map(str::to_owned),
            property: key.to_owned(),
            value,
            state,
            reason: None,
            intent: self.intent(),
            actor: self.actor.clone(),
            prior: self
                .records
                .property(doctype, field, key)
                .map(|r| r.value.clone()),
        }
    }
}

impl SiteReader for FlushingSite {
    fn installed_apps(&self) -> Vec<(String, semver::Version)> {
        self.image.installed_apps()
    }
    fn host_version(&self) -> semver::Version {
        self.image.host_version()
    }
    fn definitions(&self) -> Vec<Definition> {
        self.image.definitions()
    }
    fn customisations(&self) -> Customisations {
        SiteReader::customisations(&self.image)
    }
    fn view_layers(&self) -> Vec<spot_views::ViewLayer> {
        self.image.view_layers()
    }
    fn base_view(&self, doctype: &str) -> Option<spot_views::ViewDef> {
        self.image.base_view(doctype)
    }
    fn scripts(&self) -> Vec<ScriptRecord> {
        self.image.scripts()
    }
    fn rules(&self) -> Vec<RuleRecord> {
        self.image.rules()
    }
    fn fixtures(&self) -> Vec<FixtureSet> {
        self.image.fixtures()
    }
    fn is_system_managed(&self, kind: &str, item: &str) -> bool {
        self.image.is_system_managed(kind, item)
    }
    fn is_secret(&self, kind: &str, item: &str) -> bool {
        self.image.is_secret(kind, item)
    }
}

impl SiteWriter for FlushingSite {
    fn begin(&mut self) -> Result<(), CustomBundleError> {
        self.buffered.clear();
        self.image.begin()
    }

    fn commit(&mut self) -> Result<(), CustomBundleError> {
        if !self.baseline.same_as(&self.image) {
            self.buffered.clear();
            self.image.rollback()?;
            return Err(unstored("views, scripts, rules or fixtures"));
        }
        let ops = std::mem::take(&mut self.buffered);
        if !ops.is_empty() {
            match &self.publish {
                Publish::Now(handle) => {
                    if handle.block_on(self.store.commit(ops.clone())).is_err() {
                        self.image.rollback()?;
                        return Err(failed("the customisation records could not be written"));
                    }
                    self.records = self.records.with_ops(&ops);
                }
                Publish::Deferred => {
                    self.records = self.records.with_ops(&ops);
                    self.committed.extend(ops);
                }
            }
        }
        self.image.commit()
    }

    fn rollback(&mut self) -> Result<(), CustomBundleError> {
        self.buffered.clear();
        self.image.rollback()
    }

    fn upsert_custom_field(
        &mut self,
        doctype: &str,
        spec: FieldSpec,
        insert_after: Option<&str>,
    ) -> Result<(), CustomBundleError> {
        let text = toml::to_string(&spec)
            .map_err(|_| failed("a custom field cannot be written as a record"))?;
        let record = FieldRecord {
            doctype: doctype.to_owned(),
            name: spec.name.clone(),
            spec: text,
            insert_after: insert_after.map(str::to_owned),
            state: RecordState::Pending,
            reason: None,
            intent: self.intent(),
            actor: self.actor.clone(),
        };
        self.image
            .upsert_custom_field(doctype, spec, insert_after)?;
        self.buffered.push(RecordOp::PutField(record));
        Ok(())
    }

    fn remove_custom_field(&mut self, doctype: &str, name: &str) -> Result<(), CustomBundleError> {
        self.image.remove_custom_field(doctype, name)?;
        if let Some(existing) = self.records.field(doctype, name) {
            let mut record = existing.clone();
            record.state = RecordState::Removing;
            record.intent = self.intent();
            record.actor = self.actor.clone();
            self.buffered.push(RecordOp::PutField(record));
        }
        Ok(())
    }

    fn set_override(
        &mut self,
        doctype: &str,
        field: Option<&str>,
        property: Property,
    ) -> Result<(), CustomBundleError> {
        let record = self.property_record(
            doctype,
            field,
            property.key(),
            property_text(&property),
            RecordState::Pending,
        );
        self.image.set_override(doctype, field, property)?;
        self.buffered.push(RecordOp::PutProperty(record));
        Ok(())
    }

    fn clear_override(
        &mut self,
        doctype: &str,
        field: Option<&str>,
        key: &str,
    ) -> Result<(), CustomBundleError> {
        self.image.clear_override(doctype, field, key)?;
        if let Some(existing) = self.records.property(doctype, field, key) {
            let mut record = existing.clone();
            record.state = RecordState::Removing;
            record.intent = self.intent();
            record.actor = self.actor.clone();
            self.buffered.push(RecordOp::PutProperty(record));
        } else {
            self.buffered.push(RecordOp::Remove(RecordKey::Property(
                doctype.to_owned(),
                field.map(str::to_owned),
                key.to_owned(),
            )));
        }
        Ok(())
    }

    fn upsert_view_layer(&mut self, _: spot_views::ViewLayer) -> Result<(), CustomBundleError> {
        Err(unstored("views"))
    }

    fn remove_view_layer(
        &mut self,
        _: &str,
        _: &spot_views::LayerKind,
    ) -> Result<(), CustomBundleError> {
        Err(unstored("views"))
    }

    fn upsert_script(&mut self, _: ScriptRecord) -> Result<(), CustomBundleError> {
        Err(unstored("scripts"))
    }

    fn remove_script(&mut self, _: &str) -> Result<(), CustomBundleError> {
        Err(unstored("scripts"))
    }

    fn upsert_rule(&mut self, _: &str, _: &str) -> Result<(), CustomBundleError> {
        Err(unstored("rules"))
    }

    fn remove_rule(&mut self, _: &str) -> Result<(), CustomBundleError> {
        Err(unstored("rules"))
    }

    fn set_fixtures(
        &mut self,
        _: &str,
        _: Vec<serde_json::Value>,
    ) -> Result<(), CustomBundleError> {
        Err(unstored("fixtures"))
    }

    fn remove_fixtures(&mut self, _: &str) -> Result<(), CustomBundleError> {
        Err(unstored("fixtures"))
    }
}

/// A target site: its address and the writing image.
pub struct LiveTarget {
    env: EnvRef,
    site: FlushingSite,
}

impl LiveTarget {
    /// The image, read only (what an import evaluation plans against).
    #[must_use]
    pub fn image(&self) -> &FakeSite {
        self.site.image()
    }

    /// The writing image (for [`FlushingSite::take_committed`]).
    pub fn site_mut(&mut self) -> &mut FlushingSite {
        &mut self.site
    }
}

impl TargetSite for LiveTarget {
    fn environment(&self) -> EnvRef {
        self.env.clone()
    }

    fn as_reader(&self) -> &dyn SiteReader {
        &self.site
    }

    fn as_writer(&mut self) -> &mut dyn SiteWriter {
        &mut self.site
    }
}

/// Reads the site's wanted customisation records and builds its image.
///
/// # Errors
/// A message (never a value) when the records or a stored spec cannot be read.
pub async fn load_target(
    parts: &Parts,
    backend: &Arc<PgBackend>,
    catalog: &Arc<Catalog>,
    env: EnvRef,
    actor: &str,
    publish: Publish,
) -> Result<LiveTarget, String> {
    let store: Arc<dyn RecordStore> = Arc::new(StorageRecordStore::new(
        Arc::new((**backend).clone()),
        Arc::clone(catalog),
    ));
    let records = store
        .load()
        .await
        .map_err(|_| "the customisation records could not be read".to_owned())?;
    let wanted = |state: RecordState| matches!(state, RecordState::Applied | RecordState::Pending);
    let meta = AppliedMeta {
        fields: records
            .fields
            .iter()
            .filter(|r| wanted(r.state))
            .map(|r| (r.doctype.clone(), r.spec.clone(), r.insert_after.clone()))
            .collect(),
        properties: records
            .properties
            .iter()
            .filter(|r| wanted(r.state))
            .map(|r| {
                (
                    r.doctype.clone(),
                    r.field.clone(),
                    r.property.clone(),
                    r.value.clone(),
                )
            })
            .collect(),
    };
    let base = ImageBase {
        apps: parts.apps.clone(),
        host: parts.host.clone(),
        definitions: parts.definitions.clone(),
    };
    let image = build_image(&base, &meta)?;
    Ok(LiveTarget {
        env,
        site: FlushingSite {
            baseline: Unstored::of(&image),
            image,
            store,
            records,
            actor: actor.to_owned(),
            publish,
            buffered: Vec::new(),
            committed: Vec::new(),
        },
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use spot_customise::MemoryRecordStore;
    use spot_environments::Role;
    use spot_meta::FieldType;

    fn site(publish: Publish, store: Arc<dyn RecordStore>) -> FlushingSite {
        let image = FakeSite::new();
        FlushingSite {
            baseline: Unstored::of(&image),
            image,
            store,
            records: Records::default(),
            actor: "user:alice".into(),
            publish,
            buffered: Vec::new(),
            committed: Vec::new(),
        }
    }

    #[test]
    fn a_commit_publishes_pending_records_and_a_rollback_drops_them() {
        let store = Arc::new(MemoryRecordStore::new());
        let mut writer = site(Publish::Deferred, store);
        writer.begin().unwrap();
        writer
            .upsert_custom_field("note", FieldSpec::new("memo", FieldType::Text), None)
            .unwrap();
        writer.rollback().unwrap();
        assert!(writer.take_committed().is_empty());
        writer.begin().unwrap();
        writer
            .upsert_custom_field("note", FieldSpec::new("memo", FieldType::Text), None)
            .unwrap();
        writer.commit().unwrap();
        let ops = writer.take_committed();
        assert_eq!(ops.len(), 1);
        assert!(matches!(
            &ops[0],
            RecordOp::PutField(r) if r.state == RecordState::Pending && r.name == "memo"
        ));
    }

    #[test]
    fn a_section_without_a_live_store_is_refused() {
        let store = Arc::new(MemoryRecordStore::new());
        let mut writer = site(Publish::Deferred, store);
        writer.begin().unwrap();
        assert!(writer.upsert_rule("r", "true").is_err());
        assert!(writer.set_fixtures("d", Vec::new()).is_err());
    }

    #[test]
    fn the_address_is_the_one_the_service_compares() {
        let env = super::super::env::env_ref("set", "a", Role::Test);
        let target = LiveTarget {
            env: env.clone(),
            site: site(Publish::Deferred, Arc::new(MemoryRecordStore::new())),
        };
        assert_eq!(target.environment(), env);
    }

    #[test]
    fn property_text_inverts_parse_for_scalars() {
        for (key, value) in [("required", "true"), ("label", "Memo"), ("max_chars", "40")] {
            let property = Property::parse(key, value).unwrap();
            assert_eq!(property_text(&property), value);
        }
    }
}
