//! The refusals, the column restriction, the lookup filter and the filter's limits.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod world;

use std::collections::BTreeSet;
use std::sync::Arc;

use spot_app_api::prelude::{RecordId, Value};
use spot_perm_query::{LookupFilter, PermQueryError, PermissionFilter, ReadFilter};
use spot_permissions::{
    AppliesTo, AssignmentContext, FieldCondition, FieldFacts, FieldGrant, FieldRight, FieldTarget,
    GroupName, MaskFormat, Model, PermissionEngine, Principal, ProtectionGroup, Right, Rights,
    Role, RoleGrant, Share, ShareRight, Snapshot, UserPermission,
};
use spot_storage_contract::{
    AggFunc, Expr, Join, JoinKind, Nulls, Order, OrderDir, Query, QueryPath, Selection,
};
use world::{meta, n, target};

fn grant(role: &str, if_owner: bool) -> RoleGrant {
    RoleGrant {
        doctype: "project".into(),
        role: Role(role.into()),
        rights: Rights::new([Right::Read]),
        if_owner,
    }
}

fn group_grant(role: &str, right: FieldRight) -> FieldGrant {
    FieldGrant {
        doctype: "project".into(),
        role: Role(role.into()),
        target: FieldTarget::Group(GroupName("pay".into())),
        access: BTreeSet::from([right]),
        deny: false,
        condition: None,
    }
}

fn engine_with(shares: Vec<Share>) -> PermissionEngine {
    let model = Model {
        grants: vec![
            grant("reader", false),
            grant("full_reader", false),
            grant("masked_reader", false),
            grant("owner_reader", true),
        ],
        field_grants: vec![
            group_grant("full_reader", FieldRight::Read),
            group_grant("masked_reader", FieldRight::ReadMasked),
            FieldGrant {
                condition: Some(FieldCondition::OwnRecord),
                ..group_grant("owner_reader", FieldRight::Read)
            },
        ],
        groups: vec![ProtectionGroup {
            name: GroupName("pay".into()),
            trace: false,
        }],
        shares,
    };
    let snapshot = Snapshot::build(&meta(), &model)
        .unwrap()
        .with_field_facts(
            "project",
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

fn user(name: &str, roles: &[&str]) -> Principal {
    Principal::with_roles(name, roles)
}

fn col(field: &str) -> Expr {
    Expr::field(&n("d"), &n(field))
}

fn query() -> Query {
    let mut q = Query::from(&n("project"), &n("d"), QueryPath::Transactional);
    q.select = vec![Selection {
        expr: col("id"),
        alias: n("id"),
    }];
    q
}

fn refused(r: Result<(), PermQueryError>) -> String {
    match r {
        Err(e @ PermQueryError::FieldNotFilterable { .. }) => {
            assert_eq!(e.code(), "permquery.field_not_filterable");
            e.to_string()
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

fn check(e: &PermissionEngine, p: &Principal, q: &Query) -> Result<(), PermQueryError> {
    PermissionFilter::check_query(e, p, "project", q)
}

#[test]
fn a_filter_on_a_field_is_allowed_only_when_read_unmasked() {
    let e = engine_with(vec![]);
    let mut q = query();
    q.filter = Some(col("title").eq(Expr::value("x")));
    assert!(check(&e, &user("a", &["full_reader"]), &q).is_ok());
    let masked = refused(check(&e, &user("b", &["masked_reader"]), &q));
    let hidden = refused(check(&e, &user("c", &["reader"]), &q));
    assert_eq!(masked, hidden, "masked and hidden are refused alike");
    assert!(hidden.contains("project.title"));
    // A field that does not exist gets the same answer in the same words.
    q.filter = Some(col("ghost").eq(Expr::value("x")));
    assert!(refused(check(&e, &user("c", &["reader"]), &q)).contains("project.ghost"));
    assert_eq!(
        refused(check(&e, &user("c", &["reader"]), &q)).replace("ghost", "title"),
        hidden
    );
}

#[test]
fn sorting_grouping_aggregating_and_joining_on_a_hidden_field_are_refused() {
    let e = engine_with(vec![]);
    let p = user("c", &["reader"]);
    let mut sort = query();
    sort.order_by = vec![Order {
        expr: col("title"),
        dir: OrderDir::Asc,
        nulls: Nulls::Last,
    }];
    refused(check(&e, &p, &sort));
    let mut group = query();
    group.group_by = vec![col("title")];
    refused(check(&e, &p, &group));
    let mut agg = query();
    agg.select = vec![Selection {
        expr: Expr::Agg {
            func: AggFunc::Max,
            arg: Some(Box::new(col("title"))),
            distinct: false,
        },
        alias: n("m"),
    }];
    refused(check(&e, &p, &agg));
    let mut computed = query();
    computed.select = vec![Selection {
        expr: Expr::Coalesce(vec![col("title"), Expr::value("-")]),
        alias: n("t"),
    }];
    refused(check(&e, &p, &computed));
    let mut join = query();
    join.joins = vec![Join {
        kind: JoinKind::Inner,
        source: spot_storage_contract::Source {
            entity: n("task"),
            alias: n("t"),
        },
        on: Expr::field(&n("t"), &n("title")).eq(col("title")),
    }];
    refused(check(&e, &p, &join));
}

#[test]
fn readable_and_system_columns_pass_and_a_plain_select_is_left_to_restrict_columns() {
    let e = engine_with(vec![]);
    let p = user("c", &["reader"]);
    let mut q = query();
    q.select.push(Selection {
        expr: col("title"),
        alias: n("title"),
    });
    q.filter = Some(col("region").eq(Expr::value("north")));
    q.order_by = vec![Order {
        expr: col("id"),
        dir: OrderDir::Desc,
        nulls: Nulls::Last,
    }];
    assert!(check(&e, &p, &q).is_ok());
}

#[test]
fn columns_are_dropped_or_marked_for_masking() {
    let e = engine_with(vec![]);
    let mut q = query();
    for f in ["title", "region"] {
        q.select.push(Selection {
            expr: col(f),
            alias: n(f),
        });
    }
    let names = |r: &spot_perm_query::Restricted| -> Vec<String> {
        r.query.select.iter().map(|s| s.alias.to_string()).collect()
    };
    let full = PermissionFilter::restrict_columns(&e, &user("a", &["full_reader"]), "project", &q);
    assert_eq!(names(&full), ["id", "title", "region"]);
    assert!(full.masks.columns.is_empty());
    let masked =
        PermissionFilter::restrict_columns(&e, &user("b", &["masked_reader"]), "project", &q);
    assert_eq!(names(&masked), ["id", "title", "region"]);
    assert_eq!(masked.masks.columns.len(), 1);
    assert_eq!(masked.masks.columns[0].column.as_str(), "title");
    assert_eq!(masked.masks.columns[0].field, "project.title");
    assert_eq!(masked.masks.columns[0].rule, MaskFormat::Placeholder);
    let none = PermissionFilter::restrict_columns(&e, &user("c", &["reader"]), "project", &q);
    assert_eq!(names(&none), ["id", "region"]);
    // The system sees everything.
    let sys = PermissionFilter::restrict_columns(&e, &Principal::system(), "project", &q);
    assert_eq!(names(&sys), ["id", "title", "region"]);
}

#[test]
fn an_owner_only_reader_is_judged_on_the_rows_they_own() {
    let e = engine_with(vec![]);
    let mut q = query();
    q.select.push(Selection {
        expr: col("title"),
        alias: n("title"),
    });
    // The owner grant carries the protected group's `read`, so the title shows.
    let owner =
        PermissionFilter::restrict_columns(&e, &user("dave", &["owner_reader"]), "project", &q);
    assert_eq!(owner.query.select.len(), 2);
    // Someone who also reads others' rows with no group grant loses the title.
    let both = Principal::user(
        "dave",
        vec![
            AssignmentContext::unscoped("a1", &["owner_reader"]),
            AssignmentContext::unscoped("a2", &["reader"]),
        ],
    );
    let r = PermissionFilter::restrict_columns(&e, &both, "project", &q);
    assert_eq!(r.query.select.len(), 1);
}

#[test]
fn a_computed_column_over_a_masked_field_is_dropped() {
    let e = engine_with(vec![]);
    let mut q = query();
    q.select.push(Selection {
        expr: Expr::Coalesce(vec![col("title"), Expr::value("-")]),
        alias: n("t"),
    });
    let r = PermissionFilter::restrict_columns(&e, &user("b", &["masked_reader"]), "project", &q);
    assert_eq!(r.query.select.len(), 1);
    assert!(r.masks.columns.is_empty());
}

#[test]
fn lookup_filters_follow_the_allowed_values() {
    let e = engine_with(vec![]);
    let a = RecordId::new_v7();
    let scoped = |path: &str, allowed: Vec<String>| {
        Principal::user(
            "u",
            vec![AssignmentContext {
                id: "a".into(),
                roles: vec![Role("reader".into())],
                scope: vec![UserPermission {
                    user: String::new(),
                    doctype: None,
                    field_or_link_doctype: path.into(),
                    allowed,
                    applies_to: AppliesTo::AllDoctypes,
                    include_descendants: false,
                }],
            }],
        )
    };
    let t = target();
    let open = user("u", &["reader"]);
    assert_eq!(
        PermissionFilter::lookup_filter(&e, &open, "project", "company", &t),
        LookupFilter::Unrestricted
    );
    let only = scoped("company", vec![a.to_string()]);
    match PermissionFilter::lookup_filter(&e, &only, "project", "company", &t) {
        LookupFilter::Records(Expr::In(_, v)) => assert_eq!(v, vec![Value::Id(a)]),
        other => panic!("{other:?}"),
    }
    let select = scoped("project.region", vec!["north".into()]);
    assert_eq!(
        PermissionFilter::lookup_filter(&e, &select, "project", "region", &t),
        LookupFilter::Values(vec!["north".into()])
    );
    let nothing = scoped("company", vec![]);
    assert!(matches!(
        PermissionFilter::lookup_filter(&e, &nothing, "project", "company", &t),
        LookupFilter::Records(Expr::In(_, v)) if v.is_empty()
    ));
}

#[test]
fn many_shares_are_split_into_lists_of_at_most_a_thousand() {
    let shares: Vec<Share> = (0..2_500)
        .map(|_| Share {
            doc: RecordId::new_v7(),
            user: "frank".into(),
            rights: vec![ShareRight::Read],
        })
        .collect();
    let e = engine_with(shares);
    let f = PermissionFilter::for_read(&e, &Principal::user("frank", vec![]), "project", &target())
        .unwrap();
    let ReadFilter::Filter(Expr::Or(lists)) = f else {
        panic!("{f:?}");
    };
    let sizes: Vec<usize> = lists
        .iter()
        .map(|l| match l {
            Expr::In(_, v) => v.len(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(sizes, [1_000, 1_000, 500]);
}

#[test]
fn a_filter_that_would_crowd_out_the_callers_query_is_refused() {
    let e = engine_with(vec![]);
    let scope = |i: usize| UserPermission {
        user: String::new(),
        doctype: None,
        field_or_link_doctype: "company".into(),
        allowed: vec![RecordId::new_v7().to_string()],
        applies_to: AppliesTo::AllDoctypes,
        include_descendants: i == usize::MAX,
    };
    let p = Principal::user(
        "u",
        (0..1_200)
            .map(|i| AssignmentContext {
                id: format!("a{i:05}"),
                roles: vec![Role("reader".into())],
                scope: vec![scope(i)],
            })
            .collect(),
    );
    let err = PermissionFilter::for_read(&e, &p, "project", &target()).unwrap_err();
    assert_eq!(err.code(), "permquery.filter_too_large");
}

#[test]
fn an_unknown_doctype_reads_nothing_and_the_filter_is_a_function_of_its_inputs() {
    let e = engine_with(vec![]);
    let p = user("u", &["reader"]);
    assert_eq!(
        PermissionFilter::for_read(&e, &p, "ghost", &target()).unwrap(),
        ReadFilter::None
    );
    let a = PermissionFilter::for_read(&e, &p, "project", &target()).unwrap();
    let b = PermissionFilter::for_read(&e, &p, "project", &target()).unwrap();
    assert_eq!(a, b);
    assert_eq!(a, ReadFilter::All);
}

// --- A name the snapshot does not know is not readable by anybody (WP-SEC-03.T5) ---

/// The three kinds of principal a query comes from: a user, a named service holding the widest
/// read role, and the platform itself. The widest of them is the service on purpose: a name no
/// grant was ever written for must fail closed even for the most privileged ordinary principal.
fn every_principal() -> Vec<(&'static str, Principal)> {
    vec![
        ("user", user("c", &["reader"])),
        ("service", user("service_sync", &["full_reader"])),
        ("system", Principal::system()),
    ]
}

/// The select aliases `restrict_columns` kept.
fn kept(r: &spot_perm_query::Restricted) -> Vec<String> {
    r.query.select.iter().map(|s| s.alias.to_string()).collect()
}

fn selecting(fields: &[&str]) -> Query {
    let mut q = query();
    q.select = fields
        .iter()
        .map(|f| Selection {
            expr: col(f),
            alias: n(f),
        })
        .collect();
    q
}

#[test]
fn an_unknown_column_is_dropped_from_the_select_for_every_principal() {
    let e = engine_with(vec![]);
    for (who, p) in every_principal() {
        let r = PermissionFilter::restrict_columns(
            &e,
            &p,
            "project",
            &selecting(&["id", "ghost", "token__sealed", "token__bidx"]),
        );
        assert_eq!(kept(&r), ["id"], "{who} reads no unknown column");
        assert!(r.masks.columns.is_empty(), "{who} masks nothing it kept");
        // A select of unknown names only leaves nothing to fetch: the caller answers no rows.
        let none = PermissionFilter::restrict_columns(
            &e,
            &p,
            "project",
            &selecting(&["ghost", "token__sealed", "token__bidx"]),
        );
        assert!(
            none.query.select.is_empty(),
            "{who} selects no column at all"
        );
        assert!(
            none.masks.columns.is_empty(),
            "{who} masks nothing it dropped"
        );
    }
}

/// A query position (the name a failure message uses) and the mutation that puts a column in
/// it: one per position a query can name a column in.
type Positions<'a> = [(&'a str, &'a dyn Fn(&mut Query)); 5];

#[test]
fn an_unknown_column_in_a_filter_sort_group_having_or_join_is_refused_for_every_principal() {
    let e = engine_with(vec![]);
    let refusal = |build: &dyn Fn(&mut Query)| -> Vec<(&'static str, String)> {
        every_principal()
            .into_iter()
            .map(|(who, p)| {
                let mut q = query();
                build(&mut q);
                (who, refused(check(&e, &p, &q)))
            })
            .collect()
    };
    for column in ["ghost", "token__sealed", "token__bidx"] {
        let cases: Positions<'_> = [
            ("filter", &|q: &mut Query| {
                q.filter = Some(col(column).eq(Expr::value("x")));
            }),
            ("sort", &|q: &mut Query| {
                q.order_by = vec![Order {
                    expr: col(column),
                    dir: OrderDir::Asc,
                    nulls: Nulls::Last,
                }];
            }),
            ("group", &|q: &mut Query| {
                q.group_by = vec![col(column)];
            }),
            ("having", &|q: &mut Query| {
                q.having = Some(col(column).eq(Expr::value("x")));
            }),
            // The join condition names the column of the joined DocType.
            ("join", &|q: &mut Query| {
                q.joins = vec![Join {
                    kind: JoinKind::Inner,
                    source: spot_storage_contract::Source {
                        entity: n("task"),
                        alias: n("t"),
                    },
                    on: Expr::field(&n("t"), &n(column)).eq(col("id")),
                }];
            }),
        ];
        for (position, build) in cases {
            let said = refusal(build);
            for (who, message) in &said[1..] {
                assert_eq!(
                    message, &said[0].1,
                    "{position} on {column}: {who} is refused like {}",
                    said[0].0
                );
            }
        }
    }
}

#[test]
fn a_known_field_survives_every_position_for_every_principal() {
    let e = engine_with(vec![]);
    // `region` and `owner` are in no protection group, so every reader filters and sorts on
    // them. `title` is in `pay`: only a principal with `read` there (the service role and the
    // system) may use it in a filter, a sort, a group or a having clause — a known field that
    // one principal may not use, which is not the same thing as a name nobody knows.
    let cases: [(&str, Principal, &[&str]); 3] = [
        ("user", user("c", &["reader"]), &["region", "owner"]),
        (
            "service",
            user("service_sync", &["full_reader"]),
            &["title", "region", "owner"],
        ),
        ("system", Principal::system(), &["title", "region", "owner"]),
    ];
    for (who, p, columns) in cases {
        for &column in columns {
            let mut q = selecting(&[column]);
            q.filter = Some(col(column).eq(Expr::value("x")));
            q.order_by = vec![Order {
                expr: col(column),
                dir: OrderDir::Desc,
                nulls: Nulls::First,
            }];
            q.group_by = vec![col(column)];
            q.having = Some(col(column).eq(Expr::value("x")));
            assert!(check(&e, &p, &q).is_ok(), "{who} on {column}");
            assert_eq!(
                kept(&PermissionFilter::restrict_columns(&e, &p, "project", &q)),
                [column],
                "{who} on {column}"
            );
        }
    }
    // The plain reader is refused on `title` because the group closes it, not because the
    // name is unknown: the same principal reads `region` above.
    let mut q = selecting(&["title"]);
    q.filter = Some(col("title").eq(Expr::value("x")));
    refused(check(&e, &user("c", &["reader"]), &q));
}

/// One test per framework system column: each is known and readable by every principal, so a
/// legitimate query that names one keeps working.
fn a_system_column_survives_every_position(column: &str) {
    let e = engine_with(vec![]);
    for (who, p) in every_principal() {
        let mut q = selecting(&[column]);
        q.filter = Some(col(column).eq(Expr::value("x")));
        q.order_by = vec![Order {
            expr: col(column),
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        }];
        q.group_by = vec![col(column)];
        q.having = Some(col(column).eq(Expr::value("x")));
        assert!(check(&e, &p, &q).is_ok(), "{who} on {column}");
        assert_eq!(
            kept(&PermissionFilter::restrict_columns(&e, &p, "project", &q)),
            [column],
            "{who} on {column}"
        );
    }
}

#[test]
fn the_record_id_is_a_known_column() {
    a_system_column_survives_every_position("id");
}

#[test]
fn the_version_is_a_known_column() {
    a_system_column_survives_every_position("version");
}

#[test]
fn the_parent_is_a_known_column() {
    a_system_column_survives_every_position("parent");
}

#[test]
fn the_parent_field_is_a_known_column() {
    a_system_column_survives_every_position("parent_field");
}

#[test]
fn the_child_row_position_is_a_known_column() {
    a_system_column_survives_every_position("idx");
}

#[test]
fn the_t3_sealed_and_bindex_probes_are_refused_by_this_default_too() {
    // WP-SEC-03.T3 already refuses these names in `spot-document`'s query plan; that check
    // stays as defence in depth. This is the default underneath it: neither probe is a field
    // the snapshot knows, so neither is readable for any principal, in any position.
    let e = engine_with(vec![]);
    for (who, p) in every_principal() {
        for probe in ["token__sealed", "token__bidx"] {
            assert!(
                PermissionFilter::restrict_columns(&e, &p, "project", &selecting(&[probe]))
                    .query
                    .select
                    .is_empty(),
                "{who} selects {probe}"
            );
            let mut filter = query();
            filter.filter = Some(col(probe).eq(Expr::value("x")));
            refused(check(&e, &p, &filter));
        }
    }
}
