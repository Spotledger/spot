//! Storage side of the SCIM service (WP-IAM-06.T2): reading a tenant's directory into a
//! [`Unit`] and writing the difference back, all inside the caller's transaction, as the system
//! principal (the direct-to-storage path the operator commands use). Row decoding is defensive: a
//! row that does not decode is skipped, never guessed.
//!
//! Which document holds what: users in `user` (the SCIM-only attributes in `scim_profile`, the ETag
//! source in `scim_version`; only `internal` users are visible), SCIM groups and the group-to-role
//! table in one `group_mapping` document per IdP group name, assignments in `role_assignment`
//! with their scope in the `scope` child rows.

use std::collections::BTreeMap;

use serde_json::{Value as Json, json};
use spot_events_contract::Topic;
use spot_events_contract::payload::{EventMeta, PayloadDocument};
use spot_http::api::ports::scim::ScimReply;
use spot_iam_assign::StaticRestriction;
use spot_iam_directory::{User, UserType};
use spot_iam_roles::{Assignment, RoleName, Scope, Source};
use spot_iam_scim::{
    Email, EnterpriseUser, GroupMapping, Name as ScimName, ScimError, ScimEvent, ScimProfile,
    StoredGroup, StoredUser, parse_record_id,
};
use spot_permissions::model::{AppliesTo, UserPermission};
use spot_platform_app::doctype;
use spot_storage_contract::{
    ChildRow, DocumentData, ErrorKind, Expr, Fields, Name, NodeRef, OutboxEvent, Query, QueryPath, RecordId, RowLock, Selection, StorageError,
    TextMatch, TextMode, Transaction, Value,
};
use time::OffsetDateTime;

use crate::services::scim_unit::{Unit, source_name, source_of};

pub(crate) type Rep<T> = std::result::Result<T, ScimReply>;

// ---------------------------------------------------------------------------------------------
// Storage helpers
// ---------------------------------------------------------------------------------------------

pub(crate) fn reply(error: &ScimError) -> ScimReply {
    ScimReply::error(error.status, error.scim_type, &error.detail)
}

/// The catalogue code of a directory that cannot be read or written (HTTP 503, Retry-After).
pub(crate) const CODE_UNAVAILABLE: &str = "iam_scim.unavailable";

pub(crate) fn unavailable() -> ScimReply {
    tracing::warn!(code = CODE_UNAVAILABLE, "the SCIM directory is unavailable");
    let mut out = ScimReply::error(503, None, "The directory is unavailable; retry later.");
    out.retry_after = Some(2);
    out
}

pub(crate) fn deny() -> ScimReply {
    ScimReply::error(401, None, "A SCIM credential is required.")
}

pub(crate) fn nm(text: &str) -> Rep<Name> {
    Name::new(text).map_err(|_| unavailable())
}

/// A storage failure as the SCIM answer: a taken name is `409 uniqueness`, everything else is a
/// retryable `503` (nothing was written). The text of the failure is dropped.
pub(crate) fn storage(error: &StorageError) -> ScimReply {
    match error.kind {
        ErrorKind::UniqueViolation => reply(&ScimError::uniqueness(
            "a user name, e-mail address or group name is already in use",
        )),
        _ => unavailable(),
    }
}

pub(crate) fn text(v: &Value) -> Option<String> {
    match v {
        Value::Text(t) => Some(t.clone()),
        _ => None,
    }
}

pub(crate) fn idv(v: &Value) -> Option<RecordId> {
    match v {
        Value::Id(id) => Some(*id),
        _ => None,
    }
}

pub(crate) fn at(v: &Value) -> Option<OffsetDateTime> {
    match v {
        Value::DateTime(t) => Some(*t),
        _ => None,
    }
}

pub(crate) fn int(v: &Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(*i),
        _ => None,
    }
}

pub(crate) fn jsonv(v: &Value) -> Json {
    match v {
        Value::Json(t) => serde_json::from_str(t).unwrap_or(Json::Null),
        _ => Json::Null,
    }
}

pub(crate) fn dt(t: OffsetDateTime) -> Value {
    Value::DateTime(t)
}

pub(crate) fn opt<T: Into<Value>>(v: Option<T>) -> Value {
    v.map_or(Value::Null, Into::into)
}

/// Base16 of `bytes`, the stored text form of a digest.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    text.as_bytes()
        .chunks(2)
        .map(|p| {
            let d = |b: u8| (b as char).to_digit(16).map(|v| v as u8);
            Some(d(p[0])? * 16 + d(p[1])?)
        })
        .collect()
}

/// Every selected column of every row of `entity` (no filter, no permission path).
pub(crate) async fn rows<T: Transaction>(
    tx: &mut T,
    entity: &str,
    cols: &[&str],
) -> Rep<Vec<Vec<Value>>> {
    let (e, alias) = (nm(entity)?, nm("t")?);
    let mut q = Query::from(&e, &alias, QueryPath::Report);
    for c in cols {
        q.select.push(Selection {
            expr: Expr::field(&alias, &nm(c)?),
            alias: nm(c)?,
        });
    }
    tx.query(&q).await.map(|r| r.rows).map_err(|e| storage(&e))
}

/// The id of the record of `entity` whose text `field` equals `value`.
pub(crate) async fn find_id<T: Transaction>(
    tx: &mut T,
    entity: &str,
    field: &str,
    value: &str,
) -> Rep<Option<RecordId>> {
    let (e, alias) = (nm(entity)?, nm("t")?);
    let mut q = Query::from(&e, &alias, QueryPath::Report);
    q.select = vec![Selection {
        expr: Expr::field(&alias, &nm("id")?),
        alias: nm("v")?,
    }];
    q.filter = Some(Expr::Text(
        Box::new(Expr::field(&alias, &nm(field)?)),
        TextMatch {
            mode: TextMode::Equals,
            pattern: value.to_owned(),
            case_insensitive: false,
        },
    ));
    q.limit = Some(1);
    let found = tx.query(&q).await.map_err(|e| storage(&e))?;
    Ok(found.rows.into_iter().flatten().find_map(|v| idv(&v)))
}

/// Every stored field of a platform DocType set to `Null`, as storage demands full writes.
pub(crate) fn blank(defs: &[spot_meta::Definition], doctype: &str) -> Rep<Fields> {
    let def = defs
        .iter()
        .find(|d| d.name == doctype)
        .ok_or_else(unavailable)?;
    let mut fields = Fields::new();
    for f in def.fields.iter().filter(|f| !f.ty.is_table()) {
        if f.ty == spot_meta::FieldType::Secret {
            fields.insert(nm(&spot_meta::sealed_column(&f.name))?, Value::Null);
            if f.lookup_index() {
                fields.insert(nm(&spot_meta::blind_index_column(&f.name))?, Value::Null);
            }
        } else {
            fields.insert(nm(&f.name)?, Value::Null);
        }
    }
    Ok(fields)
}

pub(crate) fn put(fields: &mut Fields, pairs: Vec<(&str, Value)>) -> Rep<()> {
    for (k, v) in pairs {
        fields.insert(nm(k)?, v);
    }
    Ok(())
}

pub(crate) fn doc(entity: &str, id: RecordId, fields: Fields) -> Rep<DocumentData> {
    Ok(DocumentData {
        entity: nm(entity)?,
        id,
        fields,
        children: BTreeMap::new(),
    })
}

// ---------------------------------------------------------------------------------------------
// Snapshot: storage to the unit
// ---------------------------------------------------------------------------------------------

pub(crate) const USER_COLS: [&str; 13] = [
    "id",
    "user_name",
    "email",
    "full_name",
    "user_type",
    "state",
    "manager",
    "deactivated_at",
    "scim_profile",
    "scim_external_id",
    "scim_version",
    "scim_created_at",
    "scim_updated_at",
];
pub(crate) const GROUP_COLS: [&str; 10] = [
    "id",
    "idp_group",
    "role",
    "scope",
    "is_group",
    "external_id",
    "members",
    "scim_version",
    "scim_created_at",
    "scim_updated_at",
];

pub(crate) fn user_of(site: &str, r: &[Value]) -> Option<StoredUser> {
    let epoch = |v: &Value| at(v).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    if text(&r[4])? != "internal" {
        return None;
    }
    let user_name = text(&r[1])?;
    let mut user = User::new(
        idv(&r[0])?,
        UserType::Internal,
        &user_name,
        text(&r[3]).unwrap_or_default(),
        epoch(&r[11]),
    );
    user.enabled = matches!(text(&r[5])?.as_str(), "active" | "invited");
    user.manager = idv(&r[6]);
    user.deactivated_at = at(&r[7]);
    let p = jsonv(&r[8]);
    let part = |k: &str| p.get(k).cloned().unwrap_or(Json::Null);
    let emails: Vec<Email> = serde_json::from_value(part("emails")).unwrap_or_else(|_| {
        text(&r[2])
            .map(|value| Email {
                value,
                ..Email::default()
            })
            .into_iter()
            .collect()
    });
    let profile = ScimProfile {
        user_name: p
            .get("user_name")
            .and_then(Json::as_str)
            .map_or(user_name, str::to_owned),
        external_id: text(&r[9]),
        name: serde_json::from_value::<Option<ScimName>>(part("name"))
            .ok()
            .flatten(),
        emails,
        enterprise: serde_json::from_value::<Option<EnterpriseUser>>(part("enterprise"))
            .ok()
            .flatten(),
    };
    let version = u64::try_from(int(&r[10]).unwrap_or(1)).unwrap_or(1).max(1);
    Some(StoredUser {
        tenant: site.to_owned(),
        user,
        profile,
        version,
        updated_at: epoch(&r[12]),
    })
}

pub(crate) fn restriction_of(v: &Json) -> Option<StaticRestriction> {
    Some(StaticRestriction {
        doctype: v.get("doctype").and_then(Json::as_str).map(str::to_owned),
        link: v.get("link")?.as_str()?.to_owned(),
        allowed: v
            .get("allowed")?
            .as_array()?
            .iter()
            .filter_map(|a| a.as_str().map(str::to_owned))
            .collect(),
        include_descendants: v
            .get("include_descendants")
            .and_then(Json::as_bool)
            .unwrap_or(false),
    })
}

pub(crate) fn restriction_json(r: &StaticRestriction) -> Json {
    json!({"doctype": r.doctype, "link": r.link, "allowed": r.allowed,
           "include_descendants": r.include_descendants})
}

/// The group documents as the core's groups and mapping rows.
pub(crate) fn groups_of(
    site: &str,
    role_names: &BTreeMap<RecordId, String>,
    rs: &[Vec<Value>],
) -> (Vec<StoredGroup>, Vec<GroupMapping>) {
    let epoch = |v: &Value| at(v).unwrap_or(OffsetDateTime::UNIX_EPOCH);
    let (mut groups, mut maps) = (Vec::new(), Vec::new());
    for r in rs {
        let (Some(id), Some(name)) = (idv(&r[0]), text(&r[1])) else {
            continue;
        };
        if let Some(role) = idv(&r[2]).and_then(|rid| role_names.get(&rid)) {
            let scope = jsonv(&r[3])
                .as_array()
                .map(|a| a.iter().filter_map(restriction_of).collect())
                .unwrap_or_default();
            maps.push(GroupMapping {
                idp_group: name.clone(),
                role: RoleName::from(role.as_str()),
                scope,
            });
        }
        if matches!(r[4], Value::Bool(true)) {
            let members = jsonv(&r[6])
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|m| m.as_str().and_then(parse_record_id))
                        .collect()
                })
                .unwrap_or_default();
            groups.push(StoredGroup {
                tenant: site.to_owned(),
                id,
                external_id: text(&r[5]),
                display_name: name,
                members,
                version: u64::try_from(int(&r[7]).unwrap_or(1)).unwrap_or(1).max(1),
                created_at: epoch(&r[8]),
                updated_at: epoch(&r[9]),
            });
        }
    }
    (groups, maps)
}

pub(crate) fn permission_of(row: &ChildRow) -> Option<UserPermission> {
    let f = |k: &str| row.fields.get(&Name::new(k).ok()?).cloned();
    let list = |k: &str| -> Vec<String> {
        f(k).map(|v| jsonv(&v))
            .and_then(|j| serde_json::from_value(j).ok())
            .unwrap_or_default()
    };
    Some(UserPermission {
        user: f("user").as_ref().and_then(text).unwrap_or_default(),
        doctype: f("doctype").as_ref().and_then(text),
        field_or_link_doctype: f("field_or_link_doctype").as_ref().and_then(text)?,
        allowed: list("allowed"),
        applies_to: if f("applies_to").as_ref().and_then(text).as_deref() == Some("only") {
            AppliesTo::Only(list("applies_to_doctypes"))
        } else {
            AppliesTo::AllDoctypes
        },
        include_descendants: matches!(f("include_descendants"), Some(Value::Bool(true))),
    })
}

/// The directory of the tenant as a unit (users, group documents, live assignments, roles).
pub(crate) async fn load<T: Transaction>(site: &str, tx: &mut T) -> Rep<Unit> {
    let users = rows(tx, doctype::USER, &USER_COLS).await?;
    let users: Vec<StoredUser> = users.iter().filter_map(|r| user_of(site, r)).collect();
    let roles = rows(tx, doctype::ROLE, &["id", "role_name"]).await?;
    let role_names: BTreeMap<RecordId, String> = roles
        .iter()
        .filter_map(|r| Some((idv(&r[0])?, text(&r[1])?)))
        .collect();
    let (groups, mappings) = groups_of(
        site,
        &role_names,
        &rows(tx, doctype::GROUP_MAPPING, &GROUP_COLS).await?,
    );
    let raw = rows(
        tx,
        doctype::ROLE_ASSIGNMENT,
        &[
            "id",
            "user",
            "role",
            "source",
            "state",
            "valid_from",
            "valid_to",
            "reason",
        ],
    )
    .await?;
    let mut assignments = Vec::new();
    for r in raw
        .iter()
        .filter(|r| text(&r[4]).as_deref() == Some("active"))
    {
        let (Some(id), Some(user), Some(role)) = (
            idv(&r[0]),
            idv(&r[1]),
            idv(&r[2]).and_then(|x| role_names.get(&x)),
        ) else {
            continue;
        };
        let source = source_of(&text(&r[3]).unwrap_or_default());
        let mut restrictions = Vec::new();
        if source == Source::Idp {
            let loaded = tx
                .load(&nm(doctype::ROLE_ASSIGNMENT)?, id, RowLock::None)
                .await
                .map_err(|e| storage(&e))?;
            let rows = loaded
                .and_then(|d| d.data.children.get(&nm("scope").ok()?).cloned())
                .unwrap_or_default();
            restrictions = rows.iter().filter_map(permission_of).collect();
        }
        assignments.push(Assignment {
            id: id.to_string(),
            user: user.to_string(),
            role: RoleName::from(role.as_str()),
            scope: Scope { restrictions },
            valid_from: at(&r[5]),
            valid_to: at(&r[6]),
            source,
            reason: text(&r[7]),
        });
    }
    let live_keys = BTreeMap::new();
    Ok(Unit {
        tenant: site.to_owned(),
        users,
        groups,
        mappings,
        assignments,
        roles: role_names.into_values().collect(),
        events: vec![],
        audits: vec![],
        live_keys,
        keys_to_disable: vec![],
    })
}

/// The active API keys of `owner`: their count (for the outcome) and their record ids.
pub(crate) async fn keys_of<T: Transaction>(tx: &mut T, owner: RecordId) -> Rep<Vec<RecordId>> {
    let keys = rows(tx, doctype::API_KEY, &["id", "owner", "state"]).await?;
    Ok(keys
        .iter()
        .filter(|r| idv(&r[1]) == Some(owner) && text(&r[2]).as_deref() == Some("active"))
        .filter_map(|r| idv(&r[0]))
        .collect())
}

// ---------------------------------------------------------------------------------------------
// Persist: the unit's difference to storage
// ---------------------------------------------------------------------------------------------

pub(crate) fn user_fields(f: &mut Fields, su: &StoredUser, state: Option<&str>) -> Rep<()> {
    let u = &su.user;
    let state = match (u.enabled, state) {
        (true, Some(s @ ("active" | "invited"))) => s,
        (true, _) => "active",
        (false, Some("anonymised")) => "anonymised",
        (false, _) => "disabled",
    };
    let primary = su
        .profile
        .emails
        .iter()
        .find(|e| e.primary == Some(true))
        .or(su.profile.emails.first());
    let profile = json!({
        "user_name": su.profile.user_name,
        "name": serde_json::to_value(&su.profile.name).unwrap_or(Json::Null),
        "emails": serde_json::to_value(&su.profile.emails).unwrap_or(Json::Null),
        "enterprise": serde_json::to_value(&su.profile.enterprise).unwrap_or(Json::Null),
    });
    put(
        f,
        vec![
            ("user_name", u.sign_in_name.as_str().into()),
            ("email", opt(primary.map(|e| e.value.to_lowercase()))),
            ("full_name", u.display_name.as_str().into()),
            ("user_type", "internal".into()),
            ("state", state.into()),
            ("manager", opt(u.manager)),
            ("deactivated_at", u.deactivated_at.map_or(Value::Null, dt)),
            ("scim_profile", Value::Json(profile.to_string())),
            ("scim_external_id", opt(su.profile.external_id.as_deref())),
            (
                "scim_version",
                Value::Int(i64::try_from(su.version).unwrap_or(i64::MAX)),
            ),
            ("scim_created_at", dt(u.created_at)),
            ("scim_updated_at", dt(su.updated_at)),
        ],
    )
}

/// A group document's desired content.
#[derive(Clone, PartialEq)]
pub(crate) struct Desired {
    pub(crate) name: String,
    pub(crate) group: Option<StoredGroup>,
    pub(crate) mapping: Option<GroupMapping>,
}

/// One document per SCIM group (its mapping row attached) and per mapping row with no group.
/// `existing` maps a lower-cased name to a document that already holds it, so a mapping-only row
/// keeps its document; `fresh` gives the ids of new ones and is shared by both sides of a diff.
pub(crate) fn desired(
    groups: &[StoredGroup],
    mappings: &[GroupMapping],
    existing: &BTreeMap<String, RecordId>,
    fresh: &mut BTreeMap<String, RecordId>,
) -> BTreeMap<RecordId, Desired> {
    let mut out = BTreeMap::new();
    for g in groups {
        let m = mappings
            .iter()
            .find(|m| m.idp_group.eq_ignore_ascii_case(&g.display_name))
            .cloned();
        out.insert(
            g.id,
            Desired {
                name: g.display_name.clone(),
                group: Some(g.clone()),
                mapping: m,
            },
        );
    }
    for m in mappings.iter().filter(|m| {
        !groups
            .iter()
            .any(|g| g.display_name.eq_ignore_ascii_case(&m.idp_group))
    }) {
        let key = m.idp_group.to_lowercase();
        let id = *existing
            .get(&key)
            .or_else(|| fresh.get(&key))
            .unwrap_or(&RecordId::new_v7());
        fresh.insert(key, id);
        out.insert(
            id,
            Desired {
                name: m.idp_group.clone(),
                group: None,
                mapping: Some(m.clone()),
            },
        );
    }
    out
}

pub(crate) fn group_fields(
    f: &mut Fields,
    d: &Desired,
    roles: &BTreeMap<String, RecordId>,
    now: OffsetDateTime,
) -> Rep<()> {
    let role = d
        .mapping
        .as_ref()
        .and_then(|m| roles.get(m.role.as_str()).copied());
    let scope = d
        .mapping
        .as_ref()
        .map(|m| Json::Array(m.scope.iter().map(restriction_json).collect()));
    let g = d.group.as_ref();
    put(
        f,
        vec![
            ("idp_group", d.name.as_str().into()),
            ("role", opt(role)),
            (
                "scope",
                scope.map_or(Value::Null, |s| Value::Json(s.to_string())),
            ),
            ("is_group", Value::Bool(g.is_some())),
            ("external_id", opt(g.and_then(|g| g.external_id.as_deref()))),
            (
                "members",
                g.map_or(Value::Null, |g| {
                    Value::Json(
                        json!(
                            g.members
                                .iter()
                                .map(ToString::to_string)
                                .collect::<Vec<_>>()
                        )
                        .to_string(),
                    )
                }),
            ),
            (
                "scim_version",
                Value::Int(g.map_or(1, |g| i64::try_from(g.version).unwrap_or(1))),
            ),
            ("scim_created_at", dt(g.map_or(now, |g| g.created_at))),
            ("scim_updated_at", dt(g.map_or(now, |g| g.updated_at))),
        ],
    )
}

pub(crate) fn assignment_fields(
    f: &mut Fields,
    a: &Assignment,
    roles: &BTreeMap<String, RecordId>,
    now: OffsetDateTime,
) -> Rep<()> {
    let ended = a.valid_to.is_some_and(|t| t <= now);
    let (Some(user), Some(role)) = (parse_record_id(&a.user), roles.get(a.role.as_str())) else {
        return Err(unavailable());
    };
    put(
        f,
        vec![
            ("user", user.into()),
            ("role", (*role).into()),
            ("source", source_name(a.source).into()),
            ("state", (if ended { "ended" } else { "active" }).into()),
            ("valid_from", a.valid_from.map_or(Value::Null, dt)),
            ("valid_to", a.valid_to.map_or(Value::Null, dt)),
            ("reason", opt(a.reason.as_deref())),
        ],
    )
}

pub(crate) fn scope_rows(defs: &[spot_meta::Definition], a: &Assignment) -> Rep<Vec<ChildRow>> {
    let mut out = Vec::new();
    for p in &a.scope.restrictions {
        let mut f = blank(defs, doctype::ROLE_ASSIGNMENT_SCOPE)?;
        let (applies, only) = match &p.applies_to {
            AppliesTo::AllDoctypes => ("all_doctypes", Value::Null),
            AppliesTo::Only(list) => ("only", Value::Json(json!(list).to_string())),
        };
        put(
            &mut f,
            vec![
                ("doctype", opt(p.doctype.as_deref())),
                (
                    "field_or_link_doctype",
                    p.field_or_link_doctype.as_str().into(),
                ),
                ("allowed", Value::Json(json!(p.allowed).to_string())),
                ("applies_to", applies.into()),
                ("applies_to_doctypes", only),
                ("include_descendants", Value::Bool(p.include_descendants)),
            ],
        )?;
        out.push(ChildRow {
            id: RecordId::new_v7(),
            fields: f,
        });
    }
    Ok(out)
}

/// The identity event as an outbox record, about the user it names.
pub(crate) fn outbox_event(
    e: &ScimEvent,
    actor: &spot_events_contract::Actor,
    now: OffsetDateTime,
) -> Rep<OutboxEvent> {
    let user = e
        .data
        .get("user")
        .and_then(Json::as_str)
        .and_then(parse_record_id)
        .ok_or_else(unavailable)?;
    let id = RecordId::new_v7();
    let meta = EventMeta {
        occurred_at: now,
        actor: actor.clone(),
        command_id: None,
        causation_id: None,
        correlation_id: id,
        trace_id: None,
    };
    let topic = Topic::any(&e.topic).map_err(|_| unavailable())?;
    let subject = NodeRef {
        entity: nm(doctype::USER)?,
        id: user,
    };
    PayloadDocument {
        meta,
        data: e.data.clone(),
    }
    .to_outbox_event(id, &topic, subject, None, 1)
    .map_err(|_| unavailable())
}

/// Writes what `after` changed relative to `before`, the revoked keys and the events.
pub(crate) async fn persist<T: Transaction>(
    tx: &mut T,
    before: &Unit,
    after: &Unit,
    actor: &spot_events_contract::Actor,
    now: OffsetDateTime,
) -> Rep<()> {
    let defs = spot_platform_app::definitions().map_err(|_| unavailable())?;
    let role_ids: BTreeMap<String, RecordId> = {
        let r = rows(tx, doctype::ROLE, &["id", "role_name"]).await?;
        r.iter()
            .filter_map(|r| Some((text(&r[1])?, idv(&r[0])?)))
            .collect()
    };
    let by_id: BTreeMap<RecordId, &StoredUser> =
        before.users.iter().map(|u| (u.user.id, u)).collect();
    for su in after
        .users
        .iter()
        .filter(|u| by_id.get(&u.user.id) != Some(u))
    {
        let entity = nm(doctype::USER)?;
        match tx
            .load(
                &entity,
                su.user.id,
                RowLock::Update(spot_storage_contract::Wait::Block),
            )
            .await
            .map_err(|e| storage(&e))?
        {
            Some(loaded) => {
                let mut data = loaded.data;
                let state = data.fields.get(&nm("state")?).and_then(text);
                user_fields(&mut data.fields, su, state.as_deref())?;
                tx.update(&data, loaded.version)
                    .await
                    .map_err(|e| storage(&e))?;
            }
            None => {
                let mut f = blank(&defs, doctype::USER)?;
                put(
                    &mut f,
                    vec![
                        ("language", "en".into()),
                        ("time_zone", "UTC".into()),
                        ("failed_attempts", Value::Int(0)),
                    ],
                )?;
                user_fields(&mut f, su, None)?;
                tx.insert(&doc(doctype::USER, su.user.id, f)?)
                    .await
                    .map_err(|e| storage(&e))?;
            }
        }
    }
    persist_groups(tx, &defs, &role_ids, before, after, now).await?;
    persist_assignments(tx, &defs, &role_ids, before, after, now).await?;
    for user in after
        .users
        .iter()
        .filter(|u| !u.user.enabled && by_id.get(&u.user.id).is_some_and(|b| b.user.enabled))
        .map(|u| u.user.id)
    {
        for key in keys_of(tx, user).await? {
            let entity = nm(doctype::API_KEY)?;
            if let Some(loaded) = tx
                .load(
                    &entity,
                    key,
                    RowLock::Update(spot_storage_contract::Wait::Block),
                )
                .await
                .map_err(|e| storage(&e))?
            {
                let mut data = loaded.data;
                data.fields.insert(nm("state")?, "revoked".into());
                tx.update(&data, loaded.version)
                    .await
                    .map_err(|e| storage(&e))?;
            }
        }
    }
    for event in &after.events {
        tx.enqueue(&outbox_event(event, actor, now)?)
            .await
            .map_err(|e| storage(&e))?;
    }
    Ok(())
}

async fn persist_groups<T: Transaction>(
    tx: &mut T,
    defs: &[spot_meta::Definition],
    role_ids: &BTreeMap<String, RecordId>,
    before: &Unit,
    after: &Unit,
    now: OffsetDateTime,
) -> Rep<()> {
    let existing: BTreeMap<String, RecordId> =
        rows(tx, doctype::GROUP_MAPPING, &["id", "idp_group"])
            .await?
            .iter()
            .filter_map(|r| Some((text(&r[1])?.to_lowercase(), idv(&r[0])?)))
            .collect();
    let mut fresh = BTreeMap::new();
    let old = desired(&before.groups, &before.mappings, &existing, &mut fresh);
    let new = desired(&after.groups, &after.mappings, &existing, &mut fresh);
    let entity = nm(doctype::GROUP_MAPPING)?;
    let lock = RowLock::Update(spot_storage_contract::Wait::Block);
    for id in old.keys().filter(|id| !new.contains_key(*id)) {
        if let Some(loaded) = tx.load(&entity, *id, lock).await.map_err(|e| storage(&e))? {
            tx.delete(&entity, *id, loaded.version)
                .await
                .map_err(|e| storage(&e))?;
        }
    }
    for (id, d) in new.iter().filter(|(id, d)| old.get(*id) != Some(*d)) {
        match tx.load(&entity, *id, lock).await.map_err(|e| storage(&e))? {
            Some(loaded) => {
                let mut data = loaded.data;
                group_fields(&mut data.fields, d, role_ids, now)?;
                tx.update(&data, loaded.version)
                    .await
                    .map_err(|e| storage(&e))?;
            }
            None => {
                let mut f = blank(defs, doctype::GROUP_MAPPING)?;
                group_fields(&mut f, d, role_ids, now)?;
                tx.insert(&doc(doctype::GROUP_MAPPING, *id, f)?)
                    .await
                    .map_err(|e| storage(&e))?;
            }
        }
    }
    Ok(())
}

async fn persist_assignments<T: Transaction>(
    tx: &mut T,
    defs: &[spot_meta::Definition],
    role_ids: &BTreeMap<String, RecordId>,
    before: &Unit,
    after: &Unit,
    now: OffsetDateTime,
) -> Rep<()> {
    let was: BTreeMap<&str, &Assignment> = before
        .assignments
        .iter()
        .map(|a| (a.id.as_str(), a))
        .collect();
    let entity = nm(doctype::ROLE_ASSIGNMENT)?;
    for a in after
        .assignments
        .iter()
        .filter(|a| was.get(a.id.as_str()) != Some(a))
    {
        let id = parse_record_id(&a.id).ok_or_else(unavailable)?;
        let rows_new = scope_rows(defs, a)?;
        let children = BTreeMap::from([(nm("scope")?, rows_new)]);
        match tx
            .load(
                &entity,
                id,
                RowLock::Update(spot_storage_contract::Wait::Block),
            )
            .await
            .map_err(|e| storage(&e))?
        {
            Some(loaded) => {
                let mut data = loaded.data;
                assignment_fields(&mut data.fields, a, role_ids, now)?;
                data.children = children;
                tx.update(&data, loaded.version)
                    .await
                    .map_err(|e| storage(&e))?;
            }
            None => {
                let mut f = blank(defs, doctype::ROLE_ASSIGNMENT)?;
                assignment_fields(&mut f, a, role_ids, now)?;
                let mut data = doc(doctype::ROLE_ASSIGNMENT, id, f)?;
                data.children = children;
                tx.insert(&data).await.map_err(|e| storage(&e))?;
            }
        }
    }
    Ok(())
}
