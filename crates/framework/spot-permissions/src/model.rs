//! The permission model: role grants, field grants (ADR-070 section 12), assignments with
//! their scope, and shares, all serde and TOML loadable, loaded with every error reported at
//! once. There is no `permlevel` here: protection groups and field grants replace it.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};
use spot_app_api::prelude::RecordId;

use crate::error::{PermError, PermErrors};

/// A named role, for example `Sales User` or `Accounts Viewer`.
///
/// The grant table keys roles by this name; the roles of a principal's assignments name the
/// grants that apply. Role names are free text, so a site's own roles need no code change.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Role(pub String);

impl Role {
    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A bit a grant can allow on a DocType.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Right {
    /// Load and read a document.
    Read,
    /// Edit a draft document.
    Write,
    /// Create a new document.
    Create,
    /// Delete a document.
    Delete,
    /// Post a draft (the lifecycle `post` operation; `submit` in API speech).
    Submit,
    /// Cancel (unpost) a posted document.
    Cancel,
    /// Correct: cancel and amend a posted document in one transaction (ADR-42).
    Correct,
    /// Update the `allow_on_posted` fields of a posted document (`update_posted`).
    #[serde(rename = "update_posted", alias = "post_update")]
    PostUpdate,
    /// Export documents to other formats or systems.
    Export,
    /// Import documents from other systems.
    Import,
    /// Print a document.
    Print,
    /// Send a document by e-mail.
    Email,
    /// Share a document with another user.
    Share,
    /// Run reports over this DocType.
    Report,
    /// Pick (select) records of this DocType in a lookup or list.
    Select,
}

impl Right {
    /// Every right, in canonical (enum) order. A right set serialises in this order
    /// whatever order it was built in, so equal sets compare and serialise the same.
    #[must_use]
    pub fn all() -> [Right; 15] {
        [
            Right::Read,
            Right::Write,
            Right::Create,
            Right::Delete,
            Right::Submit,
            Right::Cancel,
            Right::Correct,
            Right::PostUpdate,
            Right::Export,
            Right::Import,
            Right::Print,
            Right::Email,
            Right::Share,
            Right::Report,
            Right::Select,
        ]
    }

    /// The stable text form used in reason codes and the matrix.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Right::Read => "read",
            Right::Write => "write",
            Right::Create => "create",
            Right::Delete => "delete",
            Right::Submit => "submit",
            Right::Cancel => "cancel",
            Right::Correct => "correct",
            Right::PostUpdate => "update_posted",
            Right::Export => "export",
            Right::Import => "import",
            Right::Print => "print",
            Right::Email => "email",
            Right::Share => "share",
            Right::Report => "report",
            Right::Select => "select",
        }
    }

    /// Parses the snake_case text form (`as_str`'s inverse), plus `post_update` as the
    /// accepted alias spelled in the brief.
    #[must_use]
    pub fn parse(text: &str) -> Option<Right> {
        match text {
            "read" => Some(Right::Read),
            "write" => Some(Right::Write),
            "create" => Some(Right::Create),
            "delete" => Some(Right::Delete),
            "submit" => Some(Right::Submit),
            "cancel" => Some(Right::Cancel),
            "correct" => Some(Right::Correct),
            "post_update" | "update_posted" => Some(Right::PostUpdate),
            "export" => Some(Right::Export),
            "import" => Some(Right::Import),
            "print" => Some(Right::Print),
            "email" => Some(Right::Email),
            "share" => Some(Right::Share),
            "report" => Some(Right::Report),
            "select" => Some(Right::Select),
            _ => None,
        }
    }
}

/// The rights of one grant: a non-empty subset of [`Right`], kept in canonical order so equal
/// sets serialise the same.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Rights(Vec<Right>);

impl Rights {
    /// The rights from an iterator, deduplicated and in canonical order.
    #[must_use]
    pub fn new(rights: impl IntoIterator<Item = Right>) -> Rights {
        let set: BTreeSet<Right> = rights.into_iter().collect();
        Rights(
            Right::all()
                .into_iter()
                .filter(|r| set.contains(r))
                .collect(),
        )
    }

    /// The rights, in canonical order.
    #[must_use]
    pub fn as_slice(&self) -> &[Right] {
        &self.0
    }

    /// Whether the set holds `right`.
    #[must_use]
    pub fn contains(&self, right: Right) -> bool {
        self.0.contains(&right)
    }

    /// The union of two sets, canonical order kept.
    #[must_use]
    pub fn union(&self, other: &Rights) -> Rights {
        let set: BTreeSet<Right> = self.0.iter().chain(other.0.iter()).copied().collect();
        Rights(
            Right::all()
                .into_iter()
                .filter(|r| set.contains(r))
                .collect(),
        )
    }

    /// Whether the set holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl FromIterator<Right> for Rights {
    fn from_iter<I: IntoIterator<Item = Right>>(iter: I) -> Self {
        Rights::new(iter)
    }
}

/// The reference role that holds every right, including every protected field and group
/// (ADR-070 section 12, rule 4). An ordinary assignable role, not a hidden bypass: its
/// assignments are evaluated, scoped and traced like any other, and an explicit `deny` still
/// beats it.
pub const TENANT_ADMINISTRATOR: &str = "Tenant Administrator";

/// Which role may do what on one DocType.
///
/// A DocType right. Rights on individual fields live in [`FieldGrant`]; there is no
/// permission level (ADR-068 amendment A1).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RoleGrant {
    /// The DocType this grant is about, by storage name.
    pub doctype: String,
    /// The role that holds it.
    pub role: Role,
    /// The rights granted.
    pub rights: Rights,
    /// Applies only to documents the acting user created (`created_by`).
    #[serde(default)]
    pub if_owner: bool,
}

/// A protection group's name (ADR-068 amendment A1).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GroupName(pub String);

impl GroupName {
    /// The name as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A classification tag (`pii`, `financial`, `secret`, `health`). Tags grant nothing here.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Tag(pub String);

/// A protection group a DocType declares: `trace` asks for a field access trace on every
/// `read` and `reveal` (the trace itself is WP-SEC-04).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct ProtectionGroup {
    /// The group's name.
    pub name: GroupName,
    /// Whether reads and reveals are traced.
    #[serde(default)]
    pub trace: bool,
}

/// One right on a field or protection group (ADR-070 section 12, rule 2).
///
/// The ladder: `update` implies `read`; `read` and `reveal` imply `read_masked`; `reveal_bulk`
/// implies `reveal`. `create` and `update` are separate, and `report` is separate from `read`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldRight {
    /// Sees a masked form; the real value never leaves the server.
    ReadMasked,
    /// Sees the value on a document.
    Read,
    /// Sees the unmasked value of a masked field, one document at a time.
    Reveal,
    /// Reveals in bulk: for back-end services only.
    RevealBulk,
    /// May set the field when the document is created.
    Create,
    /// May change the field later.
    Update,
    /// May use the field in list columns, reports, exports, print and search results.
    Report,
}

impl FieldRight {
    /// Every right, in enum order.
    #[must_use]
    pub fn all() -> [FieldRight; 7] {
        [
            FieldRight::ReadMasked,
            FieldRight::Read,
            FieldRight::Reveal,
            FieldRight::RevealBulk,
            FieldRight::Create,
            FieldRight::Update,
            FieldRight::Report,
        ]
    }

    /// The stable text form used in reason codes and the matrix.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            FieldRight::ReadMasked => "read_masked",
            FieldRight::Read => "read",
            FieldRight::Reveal => "reveal",
            FieldRight::RevealBulk => "reveal_bulk",
            FieldRight::Create => "create",
            FieldRight::Update => "update",
            FieldRight::Report => "report",
        }
    }

    /// The rights this one carries with it (not including itself).
    #[must_use]
    pub fn implied(self) -> &'static [FieldRight] {
        match self {
            FieldRight::Update => &[FieldRight::Read, FieldRight::ReadMasked],
            FieldRight::Read | FieldRight::Reveal => &[FieldRight::ReadMasked],
            FieldRight::RevealBulk => &[FieldRight::Reveal, FieldRight::ReadMasked],
            FieldRight::ReadMasked | FieldRight::Create | FieldRight::Report => &[],
        }
    }

    /// The set closed under the ladder: every right in `set` and everything they imply.
    #[must_use]
    pub fn close(set: &BTreeSet<FieldRight>) -> BTreeSet<FieldRight> {
        let mut out = set.clone();
        for r in set {
            out.extend(r.implied().iter().copied());
        }
        out
    }
}

/// What a field grant is about.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldTarget {
    /// One field, by name (a path for a child-table column).
    Field(String),
    /// Every field of a protection group.
    Group(GroupName),
}

/// When a field grant applies: a condition on the document or on the user's own attributes
/// (ADR-070 section 12, rule 11). A failing condition makes the grant not apply; it is not a
/// denial.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldCondition {
    /// The acting user created the document (a user sees their own salary).
    OwnRecord,
    /// The document is in one of these states ("credit limit editable only in draft").
    StatusIn(Vec<String>),
}

/// A right on a field or protection group for one role.
///
/// A `deny` grant takes away the listed rights (all of them when `access` is empty) from
/// everyone who holds the role, and no grant from any other role, duty or assignment lifts it
/// (Dynamics' "deny always overrides"). Between ordinary grants the most permissive wins.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct FieldGrant {
    /// The DocType the grant is about.
    pub doctype: String,
    /// The role that holds it.
    pub role: Role,
    /// The field or group.
    pub target: FieldTarget,
    /// The rights granted (or, with `deny`, taken away).
    #[serde(default)]
    pub access: BTreeSet<FieldRight>,
    /// An explicit denial.
    #[serde(default)]
    pub deny: bool,
    /// When the grant applies.
    #[serde(default)]
    pub condition: Option<FieldCondition>,
}

/// How a masked value is shown (ADR-070 section 12, rule 2); the same shape as the contract's
/// `MaskFormat`, defined here so this crate stays pure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaskFormat {
    /// A fixed run of `*` that reveals nothing.
    Placeholder,
    /// The last `n` characters, the rest masked.
    LastChars(u8),
}

/// Masks `value` under `format`: deterministic, **never longer than the input**, and never
/// showing more than the format says.
///
/// `Placeholder` is `*` repeated up to three times (fewer for a shorter value). `LastChars(n)`
/// shows the last `min(n, 8)` characters but never more than half of the value, so a short
/// value stays masked; the characters before them become `*`, and the output has as many
/// characters as the input.
#[must_use]
pub fn mask(value: &str, format: &MaskFormat) -> String {
    let chars: Vec<char> = value.chars().collect();
    match format {
        MaskFormat::Placeholder => "*".repeat(chars.len().min(3)),
        MaskFormat::LastChars(n) => {
            let shown = usize::from(*n).min(8).min(chars.len() / 2);
            let hidden = chars.len() - shown;
            let tail: String = chars.iter().skip(hidden).collect();
            format!("{}{tail}", "*".repeat(hidden))
        }
    }
}

/// A field's protection facts, as this crate reads them (ADR-068 amendment A1). Defined here;
/// the app-api contract carries the same facts from 0.3.0 and an adapter from
/// `spot_meta::EffectiveMeta` is a later task.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct FieldFacts {
    /// The field's name (a path for a child-table column).
    pub path: String,
    /// The field's own group plus every group it inherits from the fields it is derived from:
    /// a derived field needs a grant **in every group**.
    #[serde(default)]
    pub groups: Vec<GroupName>,
    /// Classification tags.
    #[serde(default)]
    pub tags: Vec<Tag>,
    /// How a masked reader sees the value.
    #[serde(default)]
    pub mask: Option<MaskFormat>,
}

impl FieldFacts {
    /// An unprotected field.
    #[must_use]
    pub fn plain(path: &str) -> FieldFacts {
        FieldFacts {
            path: path.to_owned(),
            ..FieldFacts::default()
        }
    }
}

/// Which DocTypes a user permission narrows: every DocType the field or link reaches, or only
/// the DocTypes of a list (the restricted document's own DocType).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppliesTo {
    /// The restriction follows the field (or link DocType) itself.
    #[default]
    AllDoctypes,
    /// The restriction applies only to documents of these DocTypes.
    Only(Vec<String>),
}

/// A user permission: which linked records (or select values) the holder of an assignment may
/// see or use. It lives in the scope of one [`AssignmentContext`], never on the user.
///
/// `field_or_link_doctype` is either the qualified path of a field on a document
/// (`"doctype.field"`, the shape a query filter writes) or a bare link target DocType name,
/// which restricts every link to it on every DocType in scope.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct UserPermission {
    /// The user's name; informational, the assignment already says whose scope this is.
    #[serde(default)]
    pub user: String,
    /// Restricts only documents of this DocType; `None` is every DocType.
    #[serde(default)]
    pub doctype: Option<String>,
    /// What the restriction is about: a qualified field path or a link target DocType name.
    pub field_or_link_doctype: String,
    /// The records the holder may see/use, by record id text, or for a `select` field by value.
    pub allowed: Vec<String>,
    /// Whom the restriction reaches.
    #[serde(default)]
    pub applies_to: AppliesTo,
    /// Also let the holder see the descendants of the allowed records (a hierarchy below an
    /// allowed node); the query compiler expands it.
    #[serde(default)]
    pub include_descendants: bool,
}

impl UserPermission {
    /// Whether this permission restricts a value of the link or select field `field_path`
    /// (`"doctype.field"` shape) on a document of `doc_doctype`; `target_doctype` is the
    /// DocType the link points at (empty for select values, which are not records).
    #[must_use]
    pub fn restricts(&self, doc_doctype: &str, field_path: &str, target_doctype: &str) -> bool {
        let self_path = self.path();
        let matches_field = !self_path.is_empty() && self_path == field_path;
        let matches_target = self.target_name() == target_doctype && !target_doctype.is_empty();
        (matches_field || matches_target)
            && match &self.applies_to {
                AppliesTo::AllDoctypes => true,
                AppliesTo::Only(list) => list.iter().any(|d| d == doc_doctype),
            }
    }

    /// The qualified field path of this permission when it names one, else `""`.
    #[must_use]
    pub fn path(&self) -> &str {
        match self.field_or_link_doctype.split_once('.') {
            Some((doctype, field)) if !doctype.is_empty() && !field.is_empty() => {
                self.field_or_link_doctype.as_str()
            }
            _ => "",
        }
    }

    /// The target DocType name of this permission when it is a bare link name, else `""`.
    #[must_use]
    pub fn target_name(&self) -> &str {
        match self.field_or_link_doctype.split_once('.') {
            Some(_) => "",
            None => self.field_or_link_doctype.as_str(),
        }
    }
}

/// A share: rights on one document granted to one user.
///
/// Defined locally as a plain struct (the brief): the runtime layer (WP-PLT-05) converts
/// `spot_activity::Share` records to this form, so the engine never depends on `spot-activity`.
/// Loaded shares carry a non-empty right set (the loader rejects an empty one).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Share {
    /// The document the rights are on, as its canonical UUIDv7 text on the wire.
    #[serde(with = "share_doc_id")]
    pub doc: RecordId,
    /// Who holds the rights.
    pub user: String,
    /// The rights granted, in canonical [`ShareRight`] order, non-empty after a load.
    pub rights: Vec<ShareRight>,
}

/// The `Share::doc` wire form: the canonical record id text.
mod share_doc_id {
    use serde::{Deserialize, Deserializer, Serializer};
    use spot_app_api::prelude::RecordId;

    /// The canonical lower-case hyphenated UUIDv7 text.
    pub fn serialize<S: Serializer>(id: &RecordId, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&id.to_string())
    }

    /// From its canonical text form via the shared parser.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<RecordId, D::Error> {
        let text = String::deserialize(d)?;
        crate::ids::parse_record_id(&text).map_err(serde::de::Error::custom)
    }
}

/// One right a share can grant. A share is per-document, so it carries only the per-document
/// rights; DocType-level rights like `create` are not sharable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShareRight {
    /// Read the document.
    Read,
    /// Edit it (a non-posted document; posted state rules still apply).
    Write,
    /// Share it onward.
    Share,
    /// Post it.
    Submit,
    /// Cancel it.
    Cancel,
    /// Correct it.
    Correct,
}

impl ShareRight {
    /// The stable text form used in reason codes and the matrix.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ShareRight::Read => "read",
            ShareRight::Write => "write",
            ShareRight::Share => "share",
            ShareRight::Submit => "submit",
            ShareRight::Cancel => "cancel",
            ShareRight::Correct => "correct",
        }
    }

    /// The rights in canonical enum order, whatever order the set was built in.
    #[must_use]
    pub fn sorted(set: BTreeSet<ShareRight>) -> Vec<ShareRight> {
        let all = [
            ShareRight::Read,
            ShareRight::Write,
            ShareRight::Share,
            ShareRight::Submit,
            ShareRight::Cancel,
            ShareRight::Correct,
        ];
        all.into_iter().filter(|r| set.contains(r)).collect()
    }
}

/// One role assignment of a user with its own scope (ADR-070 section 3): "Manager in Company
/// A" and "Viewer in Company B" are two assignments. **Scope belongs to the assignment, never
/// to the user.**
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct AssignmentContext {
    /// The assignment's id, named in every reason it contributes to.
    pub id: String,
    /// The roles the assignment gives.
    pub roles: Vec<Role>,
    /// The value restrictions of this assignment.
    #[serde(default)]
    pub scope: Vec<UserPermission>,
}

impl AssignmentContext {
    /// An assignment with no scope restriction.
    #[must_use]
    pub fn unscoped(id: &str, roles: &[&str]) -> AssignmentContext {
        AssignmentContext {
            id: id.to_owned(),
            roles: roles.iter().map(|r| Role((*r).to_owned())).collect(),
            scope: vec![],
        }
    }
}

/// A principal: who the acting user is, as the permission model sees them.
///
/// Grants and shares are stored by role and user name. `is_system` is what the API layer
/// decides for `Actor::System` invocations: they carry the platform role.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct Principal {
    /// The user's name, empty for the system.
    #[serde(default)]
    pub user: String,
    /// The user's assignments, each with its own scope.
    #[serde(default)]
    pub assignments: Vec<AssignmentContext>,
    /// The system is the platform itself (a job, the scheduler, sync apply).
    #[serde(default)]
    pub is_system: bool,
}

impl Principal {
    /// A user principal from a name and assignments.
    #[must_use]
    pub fn user(user: &str, assignments: Vec<AssignmentContext>) -> Principal {
        Principal {
            user: user.to_owned(),
            assignments,
            is_system: false,
        }
    }

    /// A user with one unscoped assignment named `default` holding `roles`.
    #[must_use]
    pub fn with_roles(user: &str, roles: &[&str]) -> Principal {
        Principal::user(user, vec![AssignmentContext::unscoped("default", roles)])
    }

    /// The platform acting as itself: full rights on every DocType (ADR-51 phases 3-4 run
    /// committed steps as the system).
    #[must_use]
    pub fn system() -> Principal {
        Principal {
            user: String::new(),
            assignments: vec![],
            is_system: true,
        }
    }
}

/// Load every model document at once and report every problem.
///
/// `grants` is the TOML of the DocType grant table (`[[entries]] role, doctype, rights,
/// if_owner`); `field_grants` the field grants; `groups` the protection groups; `shares` the
/// shares. A key the model does not know (`permlevel`, a typo) is an error, never ignored.
///
/// # Errors
///
/// [`PermErrors`], empty on success, naming each problem's file, key and reason.
pub fn load(texts: &ModelTexts) -> Result<Model, PermErrors> {
    let mut errors: Vec<PermError> = Vec::new();
    let grants = load_grants(&texts.grants, &mut errors);
    let field_grants = load_field_grants(&texts.field_grants, &mut errors);
    let groups = load_groups(&texts.groups, &mut errors);
    let shares = load_shares(&texts.shares, &mut errors);
    if errors.is_empty() {
        Ok(Model {
            grants,
            field_grants,
            groups,
            shares,
        })
    } else {
        Err(PermErrors(errors))
    }
}

/// The texts of the model documents, in whichever form a site keeps them. An empty text is an
/// empty document.
#[derive(Debug, Default, Clone)]
pub struct ModelTexts {
    /// The DocType grants table's TOML.
    pub grants: String,
    /// The field grants table's TOML.
    pub field_grants: String,
    /// The protection groups' TOML.
    pub groups: String,
    /// The shares table's TOML.
    pub shares: String,
}

/// The loaded, checked model the engine evaluates.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Model {
    /// Every DocType grant, checked and sorted.
    pub grants: Vec<RoleGrant>,
    /// Every field grant, checked and sorted.
    pub field_grants: Vec<FieldGrant>,
    /// Every declared protection group, sorted.
    pub groups: Vec<ProtectionGroup>,
    /// Every share, checked and sorted.
    pub shares: Vec<Share>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantFile {
    entries: Vec<RoleGrant>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldGrantFile {
    entries: Vec<FieldGrant>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupFile {
    entries: Vec<ProtectionGroup>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
struct ShareRow {
    doc: String,
    user: String,
    rights: Vec<String>,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ShareFile {
    entries: Vec<ShareRow>,
}

fn parse_file<T: serde::de::DeserializeOwned>(
    text: &str,
    at: &str,
    errors: &mut Vec<PermError>,
) -> Option<T> {
    if text.trim().is_empty() {
        return None;
    }
    match toml::from_str(text) {
        Ok(file) => Some(file),
        Err(e) => {
            errors.push(PermError::Parse {
                at: at.to_owned(),
                detail: e.to_string(),
            });
            None
        }
    }
}

fn load_grants(text: &str, errors: &mut Vec<PermError>) -> Vec<RoleGrant> {
    let Some(file) = parse_file::<GrantFile>(text, "grants", errors) else {
        return vec![];
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (i, mut row) in file.entries.into_iter().enumerate() {
        let src = format!("grants.entries[{i}]");
        row.doctype = row.doctype.trim().to_owned();
        row.role = Role(row.role.0.trim().to_owned());
        if row.rights.is_empty() {
            errors.push(PermError::Missing {
                at: src.clone(),
                detail: "a grant needs at least one right".to_owned(),
            });
        }
        if row.role.0.is_empty() {
            errors.push(PermError::Missing {
                at: src.clone(),
                detail: "`role` is required".to_owned(),
            });
        }
        if row.doctype.is_empty() {
            errors.push(PermError::Missing {
                at: src.clone(),
                detail: "`doctype` is required".to_owned(),
            });
        }
        if !seen.insert((row.doctype.clone(), row.role.clone(), row.if_owner)) {
            errors.push(PermError::Inconsistent {
                at: src,
                detail: "duplicate grant: the same (doctype, role, if_owner) twice".to_owned(),
            });
        }
        out.push(row);
    }
    out.sort();
    out
}

fn load_field_grants(text: &str, errors: &mut Vec<PermError>) -> Vec<FieldGrant> {
    let Some(file) = parse_file::<FieldGrantFile>(text, "field_grants", errors) else {
        return vec![];
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (i, row) in file.entries.into_iter().enumerate() {
        let src = format!("field_grants.entries[{i}]");
        if row.role.0.trim().is_empty() || row.doctype.trim().is_empty() {
            errors.push(PermError::Missing {
                at: src.clone(),
                detail: "`role` and `doctype` are required".to_owned(),
            });
        }
        let target_empty = match &row.target {
            FieldTarget::Field(f) => f.trim().is_empty(),
            FieldTarget::Group(g) => g.0.trim().is_empty(),
        };
        if target_empty {
            errors.push(PermError::Missing {
                at: src.clone(),
                detail: "`target` names no field or group".to_owned(),
            });
        }
        if row.access.is_empty() && !row.deny {
            errors.push(PermError::Missing {
                at: src.clone(),
                detail: "a field grant needs at least one right (only a deny may leave it empty)"
                    .to_owned(),
            });
        }
        if !seen.insert((
            row.doctype.clone(),
            row.role.clone(),
            row.target.clone(),
            row.deny,
            row.condition.clone(),
        )) {
            errors.push(PermError::Inconsistent {
                at: src,
                detail: "duplicate field grant".to_owned(),
            });
        }
        out.push(row);
    }
    out.sort();
    out
}

fn load_groups(text: &str, errors: &mut Vec<PermError>) -> Vec<ProtectionGroup> {
    let Some(file) = parse_file::<GroupFile>(text, "groups", errors) else {
        return vec![];
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (i, row) in file.entries.into_iter().enumerate() {
        if row.name.0.trim().is_empty() {
            errors.push(PermError::Missing {
                at: format!("groups.entries[{i}]"),
                detail: "`name` is required".to_owned(),
            });
        }
        if !seen.insert(row.name.clone()) {
            errors.push(PermError::Inconsistent {
                at: format!("groups.entries[{i}]"),
                detail: format!("protection group `{}` declared twice", row.name.0),
            });
        }
        out.push(row);
    }
    out.sort();
    out
}

fn load_shares(text: &str, errors: &mut Vec<PermError>) -> Vec<Share> {
    let Some(file) = parse_file::<ShareFile>(text, "shares", errors) else {
        return vec![];
    };
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (i, row) in file.entries.into_iter().enumerate() {
        let src = format!("shares.entries[{i}]");
        if row.user.trim().is_empty() {
            errors.push(PermError::Missing {
                at: src.clone(),
                detail: "`user` is required".to_owned(),
            });
        }
        let id = match crate::ids::parse_record_id(&row.doc) {
            Ok(id) => id,
            Err(e) => {
                errors.push(PermError::ValueInvalid {
                    at: src.clone(),
                    detail: e,
                });
                continue;
            }
        };
        let mut right_set = BTreeSet::new();
        for text in &row.rights {
            match text.as_str() {
                "read" => right_set.insert(ShareRight::Read),
                "write" => right_set.insert(ShareRight::Write),
                "share" => right_set.insert(ShareRight::Share),
                "submit" => right_set.insert(ShareRight::Submit),
                "cancel" => right_set.insert(ShareRight::Cancel),
                "correct" => right_set.insert(ShareRight::Correct),
                other => {
                    errors.push(PermError::ValueInvalid {
                        at: src.clone(),
                        detail: format!("`{other}` is not a share right"),
                    });
                    false
                }
            };
        }
        if right_set.is_empty() {
            errors.push(PermError::Missing {
                at: src.clone(),
                detail: "a share must grant at least one right".to_owned(),
            });
        }
        if !seen.insert((id, row.user.clone())) {
            errors.push(PermError::Inconsistent {
                at: src,
                detail: "duplicate share: the same (doc, user) twice".to_owned(),
            });
        }
        out.push(Share {
            doc: id,
            user: row.user.trim().to_owned(),
            rights: ShareRight::sorted(right_set),
        });
    }
    out.sort();
    out
}
