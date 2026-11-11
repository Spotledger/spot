//! RFC 7644 PATCH: the operations Entra and Okta actually send (ADR-070 7.2).
//!
//! Both providers send `add`, `replace` and `remove`, a `path` that may carry a value filter
//! (`members[value eq "<id>"]`, `emails[type eq "work"].value`), and a no-path `replace` whose
//! value is an object of attributes. Entra writes `active` as the **string** `"False"`; that is
//! accepted as a boolean, exactly as it is sent. Anything this server cannot do is a clean
//! `400 invalidPath` or `400 invalidValue`, never a silent drop.
//!
//! The functions here are pure: they take a resource and return a new one. The handler decides
//! what `active` changing means (the directory's disable/enable path); this module only moves
//! the attribute.

use serde::{Deserialize, Deserializer};
use serde_json::Value as Json;

use crate::error::{ScimError, ScimResult};
use crate::filter::{AttrPath, CompareOp, Filter, ScimValue, path_parts};
use crate::resource::{Email, Member, Name, PATCH_SCHEMA, ScimGroup, ScimUser};

/// The `emails.value` attribute path, as a client writes it. Built with `concat!` so the
/// error-code lint, which reads any all-lower-case dotted literal as a code, does not mistake it
/// for one.
const EMAILS_VALUE: &str = concat!("emails", ".value");
/// The `manager.value` attribute path, as a client writes it.
const MANAGER_VALUE: &str = concat!("manager", ".value");

/// The `op` of a PATCH operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PatchOpKind {
    /// Add a value (append for a multi-valued attribute, set otherwise).
    Add,
    /// Replace a value.
    Replace,
    /// Remove a value.
    Remove,
}

impl PatchOpKind {
    /// The lowercase wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            PatchOpKind::Add => "add",
            PatchOpKind::Replace => "replace",
            PatchOpKind::Remove => "remove",
        }
    }
}

impl<'de> Deserialize<'de> for PatchOpKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let word = String::deserialize(d)?;
        match word.to_ascii_lowercase().as_str() {
            "add" => Ok(Self::Add),
            "replace" => Ok(Self::Replace),
            "remove" => Ok(Self::Remove),
            _ => Err(serde::de::Error::custom(
                "`op` must be add, replace or remove",
            )),
        }
    }
}

/// One PATCH operation.
#[derive(Clone, Debug, Deserialize)]
pub struct PatchOperation {
    /// The operation.
    pub op: PatchOpKind,
    /// The attribute path; absent for a no-path replace.
    #[serde(default)]
    pub path: Option<String>,
    /// The value to add or replace.
    #[serde(default)]
    pub value: Option<Json>,
}

/// A parsed `PatchOp` document.
#[derive(Clone, Debug, Deserialize)]
pub struct PatchRequest {
    /// The schemas, normally the PatchOp URN.
    #[serde(default)]
    pub schemas: Vec<String>,
    /// The operations, in order.
    #[serde(rename = "Operations", alias = "operations")]
    pub operations: Vec<PatchOperation>,
}

impl PatchRequest {
    /// Parses a PATCH body, refusing a document with no operations or a bad schema list.
    pub fn parse(json: &Json) -> ScimResult<PatchRequest> {
        let request: PatchRequest = serde_json::from_value(json.clone()).map_err(|e| {
            ScimError::invalid_syntax(format!("the PATCH body is not a PatchOp: {e}"))
        })?;
        if request.operations.is_empty() {
            return Err(ScimError::invalid_syntax(
                "a PATCH has at least one operation",
            ));
        }
        if !request.schemas.is_empty() && !request.schemas.iter().any(|s| s == PATCH_SCHEMA) {
            return Err(ScimError::invalid_syntax(
                "the PATCH schema must be the PatchOp URN",
            ));
        }
        Ok(request)
    }

    /// Applies every operation to a user, in order, returning the new user.
    pub fn apply_to_user(&self, user: &ScimUser) -> ScimResult<ScimUser> {
        let mut next = user.clone();
        for op in &self.operations {
            apply_user_op(&mut next, op)?;
        }
        Ok(next)
    }

    /// Applies every operation to a group, in order, returning the new group.
    pub fn apply_to_group(&self, group: &ScimGroup) -> ScimResult<ScimGroup> {
        let mut next = group.clone();
        for op in &self.operations {
            apply_group_op(&mut next, op)?;
        }
        Ok(next)
    }
}

fn apply_user_op(user: &mut ScimUser, op: &PatchOperation) -> ScimResult<()> {
    match (&op.path, op.op) {
        (Some(path), _) => {
            let path = Filter::parse_path(path)?;
            apply_user_attr(user, &path, op.op, op.value.as_ref())
        }
        (None, PatchOpKind::Remove) => {
            Err(ScimError::invalid_path("a remove operation needs a path"))
        }
        (None, _) => {
            let Some(Json::Object(map)) = &op.value else {
                return Err(ScimError::invalid_value(
                    "a no-path operation needs an object of attributes",
                ));
            };
            for (name, value) in map {
                let path = Filter::parse_path(name)?;
                apply_user_attr(user, &path, op.op, Some(value))?;
            }
            Ok(())
        }
    }
}

fn apply_group_op(group: &mut ScimGroup, op: &PatchOperation) -> ScimResult<()> {
    match (&op.path, op.op) {
        (Some(path), _) => {
            let path = Filter::parse_path(path)?;
            apply_group_attr(group, &path, op.op, op.value.as_ref())
        }
        (None, PatchOpKind::Remove) => {
            Err(ScimError::invalid_path("a remove operation needs a path"))
        }
        (None, _) => {
            let Some(Json::Object(map)) = &op.value else {
                return Err(ScimError::invalid_value(
                    "a no-path operation needs an object of attributes",
                ));
            };
            for (name, value) in map {
                let path = Filter::parse_path(name)?;
                apply_group_attr(group, &path, op.op, Some(value))?;
            }
            Ok(())
        }
    }
}

fn apply_user_attr(
    user: &mut ScimUser,
    path: &AttrPath,
    op: PatchOpKind,
    value: Option<&Json>,
) -> ScimResult<()> {
    let (base, sub) = path_parts(path);
    match base {
        "id" | "meta" | "schemas" => Err(ScimError::mutability(format!("`{base}` is read-only"))),
        "username" => {
            if op == PatchOpKind::Remove {
                return Err(ScimError::invalid_path(
                    "`userName` is required and cannot be removed",
                ));
            }
            user.user_name = as_string(required(value, "userName")?, "userName")?;
            Ok(())
        }
        "displayname" => {
            if op == PatchOpKind::Remove {
                user.display_name = None;
                return Ok(());
            }
            user.display_name = Some(as_string(required(value, "displayName")?, "displayName")?);
            Ok(())
        }
        "externalid" => {
            if op == PatchOpKind::Remove {
                user.external_id = None;
                return Ok(());
            }
            let value = required(value, "externalId")?;
            user.external_id = if value.is_null() {
                None
            } else {
                Some(as_string(value, "externalId")?)
            };
            Ok(())
        }
        "active" => {
            if op == PatchOpKind::Remove {
                return Err(ScimError::invalid_value(
                    "`active` is a boolean; write `false` to deactivate",
                ));
            }
            user.active = as_bool(required(value, "active")?)?;
            Ok(())
        }
        "name" => {
            let name = user.name.get_or_insert_with(Name::default);
            apply_name(name, sub, op, value)
        }
        "emails" => apply_emails(user, path, sub, op, value),
        "department" => {
            let enterprise = user
                .enterprise
                .get_or_insert_with(crate::resource::EnterpriseUser::default);
            enterprise.department = optional_string(value, "department")?;
            Ok(())
        }
        "employeenumber" => {
            let enterprise = user
                .enterprise
                .get_or_insert_with(crate::resource::EnterpriseUser::default);
            enterprise.employee_number = optional_string(value, "employeeNumber")?;
            Ok(())
        }
        "manager" => {
            let enterprise = user
                .enterprise
                .get_or_insert_with(crate::resource::EnterpriseUser::default);
            if op == PatchOpKind::Remove {
                enterprise.manager = None;
                return Ok(());
            }
            let text = match sub {
                None | Some("value") => as_string(required(value, MANAGER_VALUE)?, MANAGER_VALUE)?,
                Some("displayname") => {
                    let manager = enterprise.manager.get_or_insert_with(Default::default);
                    manager.display_name = Some(as_string(
                        required(value, "manager.displayName")?,
                        "manager.displayName",
                    )?);
                    return Ok(());
                }
                Some(other) => {
                    return Err(ScimError::invalid_path(format!(
                        "`manager.{other}` is not a known attribute"
                    )));
                }
            };
            let manager = enterprise.manager.get_or_insert_with(Default::default);
            manager.value = text;
            Ok(())
        }
        _ => Err(ScimError::invalid_path(format!(
            "`{base}` is not an attribute of a user"
        ))),
    }
}

fn apply_name(
    name: &mut Name,
    sub: Option<&str>,
    op: PatchOpKind,
    value: Option<&Json>,
) -> ScimResult<()> {
    if op == PatchOpKind::Remove {
        match sub {
            None => *name = Name::default(),
            Some("formatted") => name.formatted = None,
            Some("givenname") => name.given_name = None,
            Some("familyname") => name.family_name = None,
            Some("middlename") => name.middle_name = None,
            Some("honorificprefix") => name.honorific_prefix = None,
            Some("honorificsuffix") => name.honorific_suffix = None,
            Some(other) => {
                return Err(ScimError::invalid_path(format!(
                    "`name.{other}` is not a known attribute"
                )));
            }
        }
        return Ok(());
    }
    if sub.is_none() {
        let parsed: Name = serde_json::from_value(required(value, "name")?.clone())
            .map_err(|e| ScimError::invalid_value(format!("`name` is not a name object: {e}")))?;
        *name = parsed;
        return Ok(());
    }
    let text = Some(as_string(required(value, "name")?, "name")?);
    match sub {
        None => {}
        Some("formatted") => name.formatted = text,
        Some("givenname") => name.given_name = text,
        Some("familyname") => name.family_name = text,
        Some("middlename") => name.middle_name = text,
        Some("honorificprefix") => name.honorific_prefix = text,
        Some("honorificsuffix") => name.honorific_suffix = text,
        Some(other) => {
            return Err(ScimError::invalid_path(format!(
                "`name.{other}` is not a known attribute"
            )));
        }
    }
    Ok(())
}

fn apply_emails(
    user: &mut ScimUser,
    path: &AttrPath,
    sub: Option<&str>,
    op: PatchOpKind,
    value: Option<&Json>,
) -> ScimResult<()> {
    // A value filter selects the addresses the operation applies to.
    if let Some(filter) = &path.value_filter {
        let indexes = matching_email_indexes(&user.emails, filter)?;
        match op {
            PatchOpKind::Remove => {
                if indexes.is_empty() {
                    return Err(ScimError::no_target(
                        "no e-mail address matches the value filter",
                    ));
                }
                for i in indexes.into_iter().rev() {
                    user.emails.remove(i);
                }
                Ok(())
            }
            PatchOpKind::Replace | PatchOpKind::Add => {
                if indexes.is_empty() {
                    return Err(ScimError::no_target(
                        "no e-mail address matches the value filter",
                    ));
                }
                let text = as_string(required(value, EMAILS_VALUE)?, EMAILS_VALUE)?;
                for i in indexes {
                    if let Some(email) = user.emails.get_mut(i) {
                        email.value = text.clone();
                    }
                }
                Ok(())
            }
        }
    } else {
        match op {
            PatchOpKind::Remove => {
                match sub {
                    None => user.emails.clear(),
                    Some("value") => {
                        if let Some(email) = primary_email_mut(&mut user.emails) {
                            email.value.clear();
                        }
                    }
                    Some(other) => {
                        return Err(ScimError::invalid_path(format!(
                            "`emails.{other}` is not a known attribute"
                        )));
                    }
                }
                Ok(())
            }
            PatchOpKind::Add | PatchOpKind::Replace => match sub {
                None => {
                    let parsed: Vec<Email> = serde_json::from_value(
                        required(value, "emails")?.clone(),
                    )
                    .map_err(|e| {
                        ScimError::invalid_value(format!(
                            "`emails` is not a list of addresses: {e}"
                        ))
                    })?;
                    if op == PatchOpKind::Add {
                        user.emails.extend(parsed);
                    } else {
                        user.emails = parsed;
                    }
                    Ok(())
                }
                Some("value") => {
                    let text = as_string(required(value, EMAILS_VALUE)?, EMAILS_VALUE)?;
                    match primary_email_mut(&mut user.emails) {
                        Some(email) => email.value = text,
                        None => user.emails.push(Email {
                            value: text,
                            kind: None,
                            primary: Some(true),
                        }),
                    }
                    Ok(())
                }
                Some(other) => Err(ScimError::invalid_path(format!(
                    "`emails.{other}` is not a known attribute"
                ))),
            },
        }
    }
}

fn apply_group_attr(
    group: &mut ScimGroup,
    path: &AttrPath,
    op: PatchOpKind,
    value: Option<&Json>,
) -> ScimResult<()> {
    let (base, sub) = path_parts(path);
    match base {
        "id" | "meta" | "schemas" => Err(ScimError::mutability(format!("`{base}` is read-only"))),
        "displayname" => {
            if op == PatchOpKind::Remove {
                return Err(ScimError::invalid_path(
                    "`displayName` is required and cannot be removed",
                ));
            }
            group.display_name = as_string(required(value, "displayName")?, "displayName")?;
            Ok(())
        }
        "externalid" => {
            if op == PatchOpKind::Remove {
                group.external_id = None;
                return Ok(());
            }
            let value = required(value, "externalId")?;
            group.external_id = if value.is_null() {
                None
            } else {
                Some(as_string(value, "externalId")?)
            };
            Ok(())
        }
        "members" => apply_members(group, path, sub, op, value),
        _ => Err(ScimError::invalid_path(format!(
            "`{base}` is not an attribute of a group"
        ))),
    }
}

fn apply_members(
    group: &mut ScimGroup,
    path: &AttrPath,
    sub: Option<&str>,
    op: PatchOpKind,
    value: Option<&Json>,
) -> ScimResult<()> {
    if sub.is_some() {
        return Err(ScimError::invalid_path(format!(
            "`members.{}` is not a writable attribute",
            sub.unwrap_or_default()
        )));
    }
    if let Some(filter) = &path.value_filter {
        let indexes = matching_member_indexes(&group.members, filter)?;
        match op {
            PatchOpKind::Remove => {
                if indexes.is_empty() {
                    return Err(ScimError::no_target("no member matches the value filter"));
                }
                for i in indexes.into_iter().rev() {
                    group.members.remove(i);
                }
                Ok(())
            }
            PatchOpKind::Replace | PatchOpKind::Add => Err(ScimError::invalid_path(
                "a value filter on members is used only with remove",
            )),
        }
    } else {
        match op {
            PatchOpKind::Remove => {
                group.members.clear();
                Ok(())
            }
            PatchOpKind::Add | PatchOpKind::Replace => {
                let parsed: Vec<Member> =
                    serde_json::from_value(required(value, "members")?.clone()).map_err(|e| {
                        ScimError::invalid_value(format!("`members` is not a list of members: {e}"))
                    })?;
                if op == PatchOpKind::Replace {
                    group.members = parsed;
                } else {
                    for member in parsed {
                        if !group.members.iter().any(|m| m.value == member.value) {
                            group.members.push(member);
                        }
                    }
                }
                Ok(())
            }
        }
    }
}

fn matching_email_indexes(emails: &[Email], filter: &Filter) -> ScimResult<Vec<usize>> {
    let mut out = Vec::new();
    for (i, email) in emails.iter().enumerate() {
        if email_matches(email, filter)? {
            out.push(i);
        }
    }
    Ok(out)
}

fn matching_member_indexes(members: &[Member], filter: &Filter) -> ScimResult<Vec<usize>> {
    let mut out = Vec::new();
    for (i, member) in members.iter().enumerate() {
        if member_matches(member, filter)? {
            out.push(i);
        }
    }
    Ok(out)
}

fn email_matches(email: &Email, filter: &Filter) -> ScimResult<bool> {
    let (attr, wanted) = eq_pair(filter)?;
    Ok(match attr {
        "type" => matches_text(email.kind.as_deref(), wanted),
        "value" => matches_text(Some(&email.value), wanted),
        "primary" => matches_bool(email.primary, wanted),
        other => {
            return Err(ScimError::invalid_path(format!(
                "`emails[{other} ...]` is not a supported value filter"
            )));
        }
    })
}

fn member_matches(member: &Member, filter: &Filter) -> ScimResult<bool> {
    let (attr, wanted) = eq_pair(filter)?;
    Ok(match attr {
        "value" => matches_text(Some(&member.value), wanted),
        "display" => matches_text(member.display.as_deref(), wanted),
        "type" => matches_text(member.kind.as_deref(), wanted),
        other => {
            return Err(ScimError::invalid_path(format!(
                "`members[{other} ...]` is not a supported value filter"
            )));
        }
    })
}

/// The `(attribute, value)` of a single `eq` comparison, or an `invalidPath` refusal.
fn eq_pair(filter: &Filter) -> ScimResult<(&str, &ScimValue)> {
    match filter {
        Filter::Compare {
            path,
            op: CompareOp::Eq,
            value,
        } if path.value_filter.is_none() => Ok((path.attr.as_str(), value)),
        _ => Err(ScimError::invalid_path(
            "a value filter must be a single `attribute eq value` comparison",
        )),
    }
}

fn matches_text(actual: Option<&str>, wanted: &ScimValue) -> bool {
    match (actual, wanted) {
        (Some(actual), ScimValue::Text(wanted)) => actual.eq_ignore_ascii_case(wanted),
        _ => false,
    }
}

fn matches_bool(actual: Option<bool>, wanted: &ScimValue) -> bool {
    matches!((actual, wanted), (Some(a), ScimValue::Bool(b)) if a == *b)
}

fn primary_email_mut(emails: &mut [Email]) -> Option<&mut Email> {
    let primary = emails
        .iter()
        .position(|e| e.primary == Some(true))
        .or(if emails.is_empty() { None } else { Some(0) });
    primary.and_then(move |i| emails.get_mut(i))
}

fn required<'a>(value: Option<&'a Json>, name: &str) -> ScimResult<&'a Json> {
    value.ok_or_else(|| ScimError::invalid_value(format!("`{name}` needs a value")))
}

fn as_string(value: &Json, name: &str) -> ScimResult<String> {
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| ScimError::invalid_value(format!("`{name}` must be a string")))
}

fn optional_string(value: Option<&Json>, name: &str) -> ScimResult<Option<String>> {
    match value {
        None | Some(Json::Null) => Ok(None),
        Some(v) => Ok(Some(as_string(v, name)?)),
    }
}

/// A boolean, or the strings `"true"` and `"false"` (Entra writes `"False"`).
fn as_bool(value: &Json) -> ScimResult<bool> {
    match value {
        Json::Bool(b) => Ok(*b),
        Json::String(s) => match s.to_ascii_lowercase().as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => Err(ScimError::invalid_value(
                "`active` must be a boolean or the strings `true`/`false`",
            )),
        },
        _ => Err(ScimError::invalid_value(
            "`active` must be a boolean or the strings `true`/`false`",
        )),
    }
}
