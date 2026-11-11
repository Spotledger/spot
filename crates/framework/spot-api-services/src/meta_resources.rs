//! `MetaResources` — implements `MetaService` from `spot-meta`'s effective metadata.

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_http::api::registry::Code;
use spot_http::api::service::{ApiError, BoxFuture, MetaService, RequestCtx};
use spot_meta::EffectiveMeta;

/// The metadata one request is served with.
#[derive(Clone)]
pub struct CurrentMeta {
    /// The effective metadata.
    pub meta: Arc<EffectiveMeta>,
    /// The version of the snapshot it belongs to, when the host versions its snapshots. It
    /// replaces the metadata's own version on the wire (and so in the `ETag`), because a
    /// snapshot rebuilt from folded definitions carries a version that does not rise with the
    /// host's customisations.
    pub version: Option<u64>,
}

impl From<Arc<EffectiveMeta>> for CurrentMeta {
    fn from(meta: Arc<EffectiveMeta>) -> Self {
        Self {
            meta,
            version: None,
        }
    }
}

/// Where [`MetaResources`] reads the effective metadata from, once per request.
///
/// A host that swaps metadata while it runs (runtime customisation) hands in a source that
/// answers with the snapshot it serves *now*, so the next request after a swap sees the new
/// DocTypes with no rebuild of the router. A source may refuse (a node that diverged from the
/// metadata its peers announced must not guess): the request then fails with the returned
/// error.
pub trait MetaSource: Send + Sync + 'static {
    /// The metadata to serve this request with.
    ///
    /// # Errors
    ///
    /// The error to answer the request with when the node must not serve metadata.
    fn current(&self) -> Result<CurrentMeta, ApiError>;
}

/// A source that never changes: the metadata fixed at construction.
struct Fixed(Arc<EffectiveMeta>);

impl MetaSource for Fixed {
    fn current(&self) -> Result<CurrentMeta, ApiError> {
        Ok(Arc::clone(&self.0).into())
    }
}

impl<F> MetaSource for F
where
    F: Fn() -> Result<CurrentMeta, ApiError> + Send + Sync + 'static,
{
    fn current(&self) -> Result<CurrentMeta, ApiError> {
        self()
    }
}

/// Implements `MetaService` using `spot-meta`'s `EffectiveMeta`.
///
/// Produces the native API's `DocTypeMeta` shape with `ETag` set to the metadata version. The
/// metadata is read from its [`MetaSource`] on every request.
#[derive(Clone)]
pub struct MetaResources {
    source: Arc<dyn MetaSource>,
}

impl MetaResources {
    /// Create a new `MetaResources` over fixed effective metadata (tests, and hosts whose
    /// metadata never changes while they run).
    pub fn new(meta: Arc<EffectiveMeta>) -> Self {
        Self {
            source: Arc::new(Fixed(meta)),
        }
    }

    /// Create a `MetaResources` that reads `source` for every request.
    pub fn live(source: Arc<dyn MetaSource>) -> Self {
        Self { source }
    }
}

impl MetaService for MetaResources {
    fn doctype_meta<'a>(
        &'a self,
        _ctx: &'a RequestCtx,
        doctype: &'a str,
    ) -> BoxFuture<'a, Result<spot_http::api::meta::DocTypeMeta, ApiError>> {
        Box::pin(async move {
            let current = self.source.current()?;
            let mut wire = doctype_meta(&current.meta, doctype)?;
            if let Some(version) = current.version {
                wire.version = version;
            }
            Ok(wire)
        })
    }
}

/// The wire metadata of one DocType, with its presentation formats from the effective
/// definition.
pub(crate) fn doctype_meta(
    meta: &EffectiveMeta,
    doctype: &str,
) -> Result<spot_http::api::meta::DocTypeMeta, ApiError> {
    let def = meta.doctype(doctype).ok_or_else(|| {
        ApiError::new(
            Code::MetaUnknownDoctype,
            format!("The DocType `{doctype}` does not exist."),
        )
    })?;

    let mut wire = def_meta(&def)?;
    // Presentation formats are on the effective definition, not the app view.
    if let Some(definition) = meta.definition(doctype) {
        for field in &mut wire.fields {
            if let Some(spec) = definition
                .fields
                .iter()
                .find(|s| s.name == field.name.as_str())
            {
                field.format = spec.format.clone();
                for (key, value) in [
                    ("syntax", &spec.syntax),
                    ("symbology", &spec.symbology),
                    ("currency_field", &spec.currency_field),
                ] {
                    if let Some(value) = value {
                        field
                            .format_params
                            .insert(key.into(), serde_json::Value::String(value.clone()));
                    }
                }
                if let Some(max) = spec.max {
                    field
                        .format_params
                        .insert("max".into(), serde_json::Value::from(max));
                }
            }
        }
    }
    Ok(wire)
}

/// The parent's wire metadata plus every child table's, keyed by storage name: what the codec
/// needs to decode and encode a document with its child rows.
pub(crate) fn metas_with_children(
    meta: &EffectiveMeta,
    doctype: &str,
) -> Result<BTreeMap<String, spot_http::api::meta::DocTypeMeta>, ApiError> {
    let parent = doctype_meta(meta, doctype)?;
    let mut map = BTreeMap::new();
    map.insert(parent.doctype.as_str().to_owned(), parent.clone());
    for field in &parent.fields {
        if field.field_type == spot_http::api::meta::FieldType::Table
            && let Some(child) = &field.child_doctype
            && !map.contains_key(child.as_str())
        {
            let child_meta = doctype_meta(meta, child.as_str())?;
            map.insert(child.as_str().to_owned(), child_meta);
        }
    }
    Ok(map)
}

/// Convert `DocTypeDef` to `DocTypeMeta` for the HTTP layer.
pub(crate) fn def_meta(
    def: &spot_app_api::meta::DocTypeDef,
) -> Result<spot_http::api::meta::DocTypeMeta, ApiError> {
    let fields = def
        .fields
        .iter()
        .map(meta_field)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(spot_http::api::meta::DocTypeMeta {
        doctype: def.name.clone(),
        label: def.label.clone(),
        version: def.version,
        is_posted: def.is_posted,
        fields,
    })
}

fn meta_field(
    f: &spot_app_api::meta::FieldDef,
) -> Result<spot_http::api::meta::MetaField, ApiError> {
    use spot_http::api::lookup::LookupSpec;
    use spot_http::api::meta::{FieldHints, FieldType, MetaField};

    let field_type = match &f.kind {
        spot_app_api::meta::FieldKind::Bool => FieldType::Bool,
        spot_app_api::meta::FieldKind::Int { .. } => FieldType::Int,
        spot_app_api::meta::FieldKind::Decimal { scale, .. } => {
            FieldType::Decimal { scale: *scale }
        }
        spot_app_api::meta::FieldKind::Text { .. } => FieldType::Text,
        spot_app_api::meta::FieldKind::LongText { .. } => FieldType::Text,
        spot_app_api::meta::FieldKind::Select { .. } => FieldType::Text,
        spot_app_api::meta::FieldKind::Date => FieldType::Date,
        spot_app_api::meta::FieldKind::Time => FieldType::Time,
        spot_app_api::meta::FieldKind::DateTime => FieldType::DateTime,
        spot_app_api::meta::FieldKind::Link { .. } => FieldType::Link,
        spot_app_api::meta::FieldKind::DynamicLink { .. } => FieldType::Link,
        spot_app_api::meta::FieldKind::Json { .. } => FieldType::Json,
        spot_app_api::meta::FieldKind::Bytes => FieldType::Bytes,
        spot_app_api::meta::FieldKind::Attach { .. } => FieldType::Attach,
        spot_app_api::meta::FieldKind::Secret => FieldType::Bytes,
        spot_app_api::meta::FieldKind::Table { .. } => FieldType::Table,
        spot_app_api::meta::FieldKind::TableMultiselect { .. } => FieldType::Table,
        _ => {
            return Err(ApiError::new(
                Code::HttpInternal,
                "Unsupported metadata field kind",
            ));
        }
    };

    let lookup = f.lookup.as_ref().map(|l| LookupSpec {
        seam: l.seam.clone(),
        version: l.version as u64,
        params: l.params.clone(),
    });

    let hints = FieldHints {
        description: f.hints.description.clone(),
        hidden: f.hints.hidden,
        in_list_view: f.hints.in_list_view,
        in_standard_filter: f.hints.in_standard_filter,
        no_copy: f.hints.no_copy,
        permlevel: f.hints.permlevel,
        control: f
            .hints
            .control
            .and_then(|c| spot_http::api::meta::ControlHint::parse(c.as_str())),
    };

    let has_hints = !hints.is_default();

    let child_doctype = match &f.kind {
        spot_app_api::meta::FieldKind::Table { doctype }
        | spot_app_api::meta::FieldKind::TableMultiselect { doctype, .. } => Some(doctype.clone()),
        _ => None,
    };

    let link_field = match &f.kind {
        spot_app_api::meta::FieldKind::TableMultiselect { link_field, .. } => {
            Some(link_field.clone())
        }
        _ => None,
    };

    let mut field = MetaField {
        name: f.name.clone(),
        label: f.label.clone(),
        field_type,
        required: f.required,
        read_only: f.read_only,
        scale: match &f.kind {
            spot_app_api::meta::FieldKind::Decimal { scale, .. } => Some(*scale),
            _ => None,
        },
        precision: match &f.kind {
            spot_app_api::meta::FieldKind::Decimal { precision, .. } => Some(*precision),
            _ => None,
        },
        link_to: match &f.kind {
            spot_app_api::meta::FieldKind::Link { doctype } => Some(doctype.clone()),
            spot_app_api::meta::FieldKind::DynamicLink { .. } => None,
            _ => None,
        },
        link_type_field: match &f.kind {
            spot_app_api::meta::FieldKind::DynamicLink { type_field } => Some(type_field.clone()),
            _ => None,
        },
        child_doctype,
        link_field,
        select: match &f.kind {
            spot_app_api::meta::FieldKind::Select { values } => Some(values.clone()),
            _ => None,
        },
        max_chars: match &f.kind {
            spot_app_api::meta::FieldKind::Text { max_chars, .. } => *max_chars,
            _ => None,
        },
        default: None,
        format: None,
        format_params: BTreeMap::new(),
        lookup,
        hints,
        has_hints,
        multiple: f.multiple,
    };
    field.default = f
        .default
        .as_ref()
        .map(|v| spot_http::api::codec::encode_scalar(v, &field))
        .transpose()?;
    Ok(field)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex;

    use spot_meta::{Definition, Registry};

    use super::*;

    fn meta(label: &str) -> Arc<EffectiveMeta> {
        let text = format!(
            "name = \"party\"\nlabel = \"{label}\"\n[[fields]]\nname = \"party_name\"\ntype = \"text\"\nmax_chars = 40\n"
        );
        let definition = Definition::parse("party.toml", &text).unwrap();
        Registry::new(vec![definition]).unwrap().effective()
    }

    fn ctx() -> RequestCtx {
        RequestCtx {
            site: spot_core::SiteId::new("test").unwrap(),
            credential: None,
            idempotency_key: None,
            language: None,
            request_id: spot_http::request::RequestId::generate(),
            peer: None,
        }
    }

    #[tokio::test]
    async fn a_live_source_is_read_for_every_request() {
        let cell = Arc::new(Mutex::new(CurrentMeta {
            meta: meta("Party"),
            version: Some(3),
        }));
        let source = {
            let cell = cell.clone();
            move || Ok(cell.lock().unwrap().clone())
        };
        let service = MetaResources::live(Arc::new(source));
        let first = service.doctype_meta(&ctx(), "party").await.unwrap();
        assert_eq!((first.label.as_str(), first.version), ("Party", 3));

        *cell.lock().unwrap() = CurrentMeta {
            meta: meta("Counterparty"),
            version: Some(4),
        };
        let second = service.doctype_meta(&ctx(), "party").await.unwrap();
        assert_eq!(
            (second.label.as_str(), second.version),
            ("Counterparty", 4),
            "the next request sees the swapped metadata and its snapshot version"
        );
    }

    #[tokio::test]
    async fn a_source_may_refuse_to_serve() {
        let service = MetaResources::live(Arc::new(|| {
            Err(ApiError::from_code(Code::ServerUnavailable))
        }));
        let refused = service.doctype_meta(&ctx(), "party").await.unwrap_err();
        assert_eq!(refused.code.as_str(), Code::ServerUnavailable.as_str());
    }

    #[tokio::test]
    async fn fixed_metadata_keeps_its_own_version() {
        let fixed = meta("Party");
        let version = fixed.version();
        let service = MetaResources::new(fixed);
        let wire = service.doctype_meta(&ctx(), "party").await.unwrap();
        assert_eq!(wire.version, version);
    }
}
