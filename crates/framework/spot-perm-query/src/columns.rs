//! `restrict_columns`: dropping the columns the principal cannot read and listing the ones
//! to mask after the fetch.

use spot_permissions::{DocFacts, FieldLevel, MaskFormat, PermissionEngine, Principal};
use spot_storage_contract::{Expr, Name, Query};

use crate::PermissionFilter;
use crate::analysis::{ReadGrant, read_grant, shared_read_ids, sorted_assignments};
use crate::check::{field_refs, is_system};

/// A column whose value the API layer masks after the fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaskedColumn {
    /// The select alias of the column.
    pub column: Name,
    /// The DocType and field, `doctype.field`.
    pub field: String,
    /// How to mask it.
    pub rule: MaskFormat,
}

/// The columns to mask after the fetch.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct MaskPlan {
    /// One entry per masked column, in select order.
    pub columns: Vec<MaskedColumn>,
}

/// A query with only the columns the principal may see.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Restricted {
    /// The query, select list reduced. It may have no columns left; then there is nothing the
    /// principal may see, and the caller answers without querying.
    pub query: Query,
    /// What to mask in the fetched rows.
    pub masks: MaskPlan,
}

fn rank(l: FieldLevel) -> u8 {
    match l {
        FieldLevel::Hidden => 0,
        FieldLevel::Masked(_) => 1,
        FieldLevel::Read => 2,
        FieldLevel::ReadReveal => 3,
    }
}

/// The document shapes a row the principal can read may have: owned by them, somebody
/// else's, or shared. A column shows only what the *least* revealing reachable shape shows.
fn shapes(engine: &PermissionEngine, principal: &Principal, doctype: &str) -> Vec<DocFacts> {
    let snapshot = engine.snapshot();
    let mut owned = false;
    let mut other = false;
    for a in sorted_assignments(principal) {
        match read_grant(snapshot, a, doctype) {
            ReadGrant::Nothing => {}
            ReadGrant::OwnerOnly => owned = true,
            ReadGrant::Everything => {
                owned = true;
                other = true;
            }
        }
    }
    let shared = shared_read_ids(snapshot, principal);
    let base = DocFacts {
        doctype: doctype.to_owned(),
        ..DocFacts::default()
    };
    let mut out = Vec::new();
    if owned {
        out.push(DocFacts {
            created_by: Some(principal.user.clone()),
            ..base.clone()
        });
    }
    if other {
        out.push(base.clone());
    }
    if let Some(first) = shared.first() {
        out.push(DocFacts {
            id: Some(first.to_string()),
            ..base
        });
    }
    out
}

/// How much of one column a principal sees.
///
/// A name the snapshot does not know is **not readable by anybody**: the snapshot's facts for
/// a DocType are the whole list of columns it has (the framework's system columns aside, which
/// the callers skip), so a name outside that list is a column no grant was ever written for.
/// It fails closed to [`FieldLevel::Hidden`] here, before any principal is consulted, so an
/// unknown name — `<field>__sealed`, `<field>__bidx` or any column a later task adds — can
/// never be selected, sorted, grouped, filtered on or joined through this crate.
fn level(
    engine: &PermissionEngine,
    principal: &Principal,
    doctype: &str,
    field: &str,
    shapes: &[DocFacts],
) -> FieldLevel {
    let Some(facts) = engine
        .snapshot()
        .facts(doctype)
        .and_then(|f| f.field(field).cloned())
    else {
        return FieldLevel::Hidden;
    };
    if principal.is_system {
        // The platform reads a field the snapshot knows at the plain level. Revealing a masked
        // value is a deliberate, narrow grant (ADR-068, ADR-070), never the default answer to
        // every system question, and a system principal does not get it here.
        return FieldLevel::Read;
    }
    shapes
        .iter()
        .map(|d| engine.field_access(principal, doctype, &facts, d).level)
        .min_by_key(|l| rank(*l))
        .unwrap_or(FieldLevel::Hidden)
}

impl PermissionFilter {
    /// Reduces the select list to the columns the principal may see.
    ///
    /// A column that is a plain field of the DocType stays when readable, stays with a
    /// [`MaskedColumn`] entry when masked, and goes when hidden. A computed column stays only
    /// if every field in it is readable unmasked. A name the snapshot does not know goes for
    /// **every** principal, the system included: it is not readable, so a query that names one
    /// answers with no column (and a select of nothing but unknown names answers with no
    /// column at all, so the caller returns no rows). System columns are always kept. Fields
    /// are judged for the least revealing kind of row the principal can reach (their own,
    /// others', or shared), without a document: grants that depend on a document's state do
    /// not count. Values are not touched here; the API layer masks after the fetch.
    #[must_use]
    pub fn restrict_columns(
        engine: &PermissionEngine,
        principal: &Principal,
        doctype: &str,
        query: &Query,
    ) -> Restricted {
        let mut shape_cache: Vec<(String, Vec<DocFacts>)> = Vec::new();
        let entity_of = |alias: &Name| -> String {
            if alias == &query.from.alias {
                return doctype.to_owned();
            }
            query
                .joins
                .iter()
                .find(|j| &j.source.alias == alias)
                .map_or_else(String::new, |j| j.source.entity.as_str().to_owned())
        };
        let mut kept = Vec::new();
        let mut masks = Vec::new();
        for sel in &query.select {
            let refs = field_refs(&sel.expr);
            let plain = matches!(sel.expr, Expr::Field(_));
            let mut worst = FieldLevel::ReadReveal;
            let mut masked_field = String::new();
            for r in refs.iter().filter(|r| !is_system(&r.field)) {
                let entity = entity_of(&r.alias);
                if !shape_cache.iter().any(|(e, _)| e == &entity) {
                    let s = shapes(engine, principal, &entity);
                    shape_cache.push((entity.clone(), s));
                }
                let s = shape_cache
                    .iter()
                    .find(|(e, _)| e == &entity)
                    .map_or(&[][..], |(_, s)| s.as_slice());
                let l = level(engine, principal, &entity, r.field.as_str(), s);
                if rank(l) < rank(worst) {
                    worst = l;
                    masked_field = format!("{entity}.{}", r.field);
                }
            }
            match worst {
                FieldLevel::Hidden => {}
                FieldLevel::Masked(rule) if plain => {
                    masks.push(MaskedColumn {
                        column: sel.alias.clone(),
                        field: masked_field,
                        rule,
                    });
                    kept.push(sel.clone());
                }
                FieldLevel::Masked(_) => {}
                FieldLevel::Read | FieldLevel::ReadReveal => kept.push(sel.clone()),
            }
        }
        let mut query = query.clone();
        query.select = kept;
        Restricted {
            query,
            masks: MaskPlan { columns: masks },
        }
    }
}

#[cfg(test)]
mod tests {
    //! The fail-closed default itself: the level `restrict_columns` computes for one column,
    //! which the integration tests can only see after a column is kept or dropped.
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::collections::BTreeSet;
    use std::sync::Arc;

    use spot_meta::{Definition, FieldSpec, FieldType, Registry};
    use spot_permissions::{
        AssignmentContext, FieldFacts, FieldGrant, FieldLevel, FieldRight, FieldTarget, GroupName,
        MaskFormat, Model, PermissionEngine, Principal, ProtectionGroup, Right, Rights, Role,
        RoleGrant, Snapshot,
    };

    use super::{level, shapes};
    use crate::check::SYSTEM_COLUMNS;

    /// `note`, with `title` in the protection group `pay` and masked as a placeholder.
    fn engine() -> PermissionEngine {
        let mut note = Definition::new("note");
        note.fields = vec![
            FieldSpec::new("title", FieldType::Text),
            FieldSpec::new("body", FieldType::Text),
        ];
        let meta = Registry::new(vec![note]).unwrap().effective();
        let grant = |role: &str| RoleGrant {
            doctype: "note".into(),
            role: Role(role.into()),
            rights: Rights::new([Right::Read]),
            if_owner: false,
        };
        let field_grant = |role: &str, right: FieldRight| FieldGrant {
            doctype: "note".into(),
            role: Role(role.into()),
            target: FieldTarget::Group(GroupName("pay".into())),
            access: BTreeSet::from([right]),
            deny: false,
            condition: None,
        };
        let model = Model {
            grants: vec![
                grant("reader"),
                grant("full_reader"),
                grant("masked_reader"),
            ],
            field_grants: vec![
                field_grant("full_reader", FieldRight::Read),
                field_grant("masked_reader", FieldRight::ReadMasked),
            ],
            groups: vec![ProtectionGroup {
                name: GroupName("pay".into()),
                trace: false,
            }],
            ..Model::default()
        };
        let snapshot = Snapshot::build(&meta, &model)
            .unwrap()
            .with_field_facts(
                "note",
                vec![FieldFacts {
                    path: "title".into(),
                    groups: vec![GroupName("pay".into())],
                    tags: vec![],
                    mask: Some(MaskFormat::Placeholder),
                }],
            )
            .unwrap();
        PermissionEngine::new(Arc::new(snapshot))
    }

    fn at(e: &PermissionEngine, p: &Principal, doctype: &str, field: &str) -> FieldLevel {
        level(e, p, doctype, field, &shapes(e, p, doctype))
    }

    #[test]
    fn a_known_field_keeps_its_level() {
        let e = engine();
        let full = Principal::with_roles("a", &["full_reader"]);
        let masked = Principal::with_roles("b", &["masked_reader"]);
        let plain = Principal::with_roles("c", &["reader"]);
        assert_eq!(at(&e, &full, "note", "title"), FieldLevel::Read);
        assert_eq!(
            at(&e, &masked, "note", "title"),
            FieldLevel::Masked(MaskFormat::Placeholder)
        );
        // No grant in the group closes `title`; `body` is in no group, so plain read sees it.
        assert_eq!(at(&e, &plain, "note", "title"), FieldLevel::Hidden);
        assert_eq!(at(&e, &plain, "note", "body"), FieldLevel::Read);
    }

    #[test]
    fn a_system_principal_reads_a_known_field_but_is_not_granted_reveal() {
        let e = engine();
        assert_eq!(
            at(&e, &Principal::system(), "note", "title"),
            FieldLevel::Read,
            "a masked field is plain Read for the system, never ReadReveal"
        );
        assert_eq!(
            at(&e, &Principal::system(), "note", "body"),
            FieldLevel::Read
        );
    }

    #[test]
    fn a_name_the_snapshot_does_not_know_is_hidden_for_every_principal() {
        let e = engine();
        let service = Principal::user(
            "service_sync",
            vec![AssignmentContext::unscoped("a1", &["full_reader"])],
        );
        for p in [
            Principal::with_roles("a", &["full_reader"]),
            Principal::with_roles("c", &["reader"]),
            service,
            Principal::system(),
        ] {
            for field in ["ghost", "token__sealed", "token__bidx"] {
                assert_eq!(
                    at(&e, &p, "note", field),
                    FieldLevel::Hidden,
                    "{field} for {:?}",
                    p.user
                );
            }
        }
        // A DocType the snapshot does not know fails closed the same way, the system included.
        assert_eq!(
            at(&e, &Principal::system(), "ghost", "title"),
            FieldLevel::Hidden
        );
    }

    #[test]
    fn the_system_columns_are_the_contracts_reserved_names() {
        assert_eq!(
            SYSTEM_COLUMNS,
            ["id", "version", "parent", "parent_field", "idx"]
        );
    }
}
