//! `lookup_filter`: the values a restricted field may take, for the lookup operation.

use spot_permissions::{Allowed, PermissionEngine, Principal, ids::parse_record_id};
use spot_storage_contract::{Expr, RecordId, Value};

use crate::PermissionFilter;
use crate::read::{ReadTarget, id_column};

/// What a lookup of one field's candidates may offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LookupFilter {
    /// Anything: no restriction reaches the field.
    Unrestricted,
    /// Only the records of the target DocType this predicate holds for (on the target's
    /// `id`); `Or([])` when nothing is allowed.
    Records(Expr),
    /// Only these values of a select field.
    Values(Vec<String>),
}

impl PermissionFilter {
    /// The values of the restricted `field` of `doctype` the principal may pick.
    ///
    /// For a link field the filter is over the *target* DocType's records, written against
    /// `target.alias`; for a select field it is the list of values.
    #[must_use]
    pub fn lookup_filter(
        engine: &PermissionEngine,
        principal: &Principal,
        doctype: &str,
        field: &str,
        target: &ReadTarget,
    ) -> LookupFilter {
        let Allowed::Only(list) = engine.allowed_values(principal, doctype, field) else {
            return LookupFilter::Unrestricted;
        };
        let is_select = engine
            .snapshot()
            .facts(doctype)
            .and_then(|f| f.links.iter().find(|l| l.name == field).cloned())
            .is_some_and(|l| l.select_values.is_some());
        if is_select {
            return LookupFilter::Values(list);
        }
        let mut ids: Vec<RecordId> = list
            .iter()
            .filter_map(|t| parse_record_id(t).ok())
            .collect();
        ids.sort();
        ids.dedup();
        LookupFilter::Records(Expr::In(
            Box::new(id_column(target)),
            ids.into_iter().map(Value::Id).collect(),
        ))
    }
}
