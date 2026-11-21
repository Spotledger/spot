//! The plan: which tenant is cloned, from which snapshot, under which masking policy, into
//! which sandbox tenant.
//!
//! A plan is validated before any work starts: a source equal to the sandbox name, an empty
//! identifier or a snapshot taken from another tenant is refused. The policy is data (TOML)
//! so a customer's tightening of the classification set needs no code change.

use serde::{Deserialize, Serialize};
use spot_meta::FieldType;

use crate::classify::ClassificationSource;
use crate::error::{Error, Result};
use crate::mask::{MaskKind, kind_for};

/// The default classification tags that close a field: personal, financial, secret, health.
pub const DEFAULT_TAGS: [&str; 4] = ["pii", "financial", "secret", "health"];

/// The default number of characters below which a value is not indexed for the leak scan.
pub const DEFAULT_SCAN_MIN_CHARS: usize = 4;

/// The default placeholder written where an attachment used to be.
pub const DEFAULT_ATTACHMENT_PLACEHOLDER: &str = "attachment removed by the sandbox masker";

/// Which snapshot a sandbox is cloned from.
///
/// It is deliberately a plain record, not the snapshot crate's `Snapshot` handle: the
/// provider that owns the bytes implements [`crate::SnapshotPort`] and returns this.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotRef {
    /// The provider's snapshot id, unique per provider.
    pub id: String,
    /// The provider that took it (`spot-snapshot`, a platform provider, a test double).
    pub provider: String,
    /// The tenant the snapshot was taken from; it must be the plan's source tenant.
    pub source_tenant: String,
    /// When it was taken, Unix microseconds.
    pub taken_at: i64,
    /// Its size in bytes, as the provider reported it.
    pub bytes: u64,
}

impl SnapshotRef {
    /// A snapshot reference with the fields a caller always knows.
    #[must_use]
    pub fn new(
        id: impl Into<String>,
        provider: impl Into<String>,
        source_tenant: impl Into<String>,
        taken_at: i64,
        bytes: u64,
    ) -> Self {
        Self {
            id: id.into(),
            provider: provider.into(),
            source_tenant: source_tenant.into(),
            taken_at,
            bytes,
        }
    }
}

/// What the masking engine must treat as closed, and how.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaskingPolicy {
    /// Classification tags that close a field. Defaults to `pii`, `financial`, `secret` and
    /// `health`; a tag never grants anything, it only selects fields for the masker.
    #[serde(default = "default_tags")]
    pub tags: Vec<String>,
    /// Protection groups that close a field. Empty means **every** protection group: a group
    /// exists to be closed, so listing groups would only let a new one be forgotten.
    #[serde(default)]
    pub groups: Vec<String>,
    /// Mask denormalised `fetch_from` copies with the protection of their source field.
    #[serde(default = "default_true")]
    pub mask_derived_copies: bool,
    /// The place where an attachment's content was.
    #[serde(default = "default_placeholder")]
    pub attachment_placeholder: String,
    /// Values shorter than this are not indexed for the leak scan (see the crate docs).
    #[serde(default = "default_scan_min_chars")]
    pub scan_min_chars: usize,
    /// The explicit **Keep** decisions of the administrator: fields that stay in clear. A field
    /// without a classification and without an entry here is masked (fail closed). Recorded in
    /// the plan so the decision is reviewable and travels with the run.
    #[serde(default)]
    pub keep: Vec<KeepField>,
}

/// One administrator decision to keep a field in clear in the sandbox.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeepField {
    /// The DocType that holds the field.
    pub doctype: String,
    /// The field's storage name.
    pub field: String,
}

impl KeepField {
    /// A keep decision for `doctype.field`.
    #[must_use]
    pub fn new(doctype: impl Into<String>, field: impl Into<String>) -> Self {
        Self {
            doctype: doctype.into(),
            field: field.into(),
        }
    }
}

fn default_tags() -> Vec<String> {
    DEFAULT_TAGS.iter().map(|tag| (*tag).to_owned()).collect()
}

fn default_placeholder() -> String {
    DEFAULT_ATTACHMENT_PLACEHOLDER.to_owned()
}

const fn default_scan_min_chars() -> usize {
    DEFAULT_SCAN_MIN_CHARS
}

const fn default_true() -> bool {
    true
}

impl Default for MaskingPolicy {
    fn default() -> Self {
        Self {
            tags: default_tags(),
            groups: Vec::new(),
            mask_derived_copies: true,
            attachment_placeholder: default_placeholder(),
            scan_min_chars: DEFAULT_SCAN_MIN_CHARS,
            keep: Vec::new(),
        }
    }
}

impl MaskingPolicy {
    /// Loads a policy from TOML. Unknown keys are refused so a misspelt switch cannot silently
    /// leave a class of field open.
    ///
    /// # Errors
    /// [`Error::Policy`] with the TOML parse text (never a value).
    pub fn from_toml(text: &str) -> Result<Self> {
        let policy: Self = toml::from_str(text).map_err(|error| Error::Policy {
            reason: error.to_string(),
        })?;
        policy.validate()?;
        Ok(policy)
    }

    /// Checks the policy's invariants.
    ///
    /// # Errors
    /// [`Error::Policy`] when a tag is empty, a duplicate, carries whitespace, or the scan
    /// minimum is zero.
    pub fn validate(&self) -> Result<()> {
        if self.scan_min_chars == 0 {
            return Err(Error::Policy {
                reason: "scan_min_chars must be at least one".to_owned(),
            });
        }
        let mut kept = std::collections::BTreeSet::new();
        for entry in &self.keep {
            if entry.doctype.is_empty() || entry.field.is_empty() {
                return Err(Error::Policy {
                    reason: "a keep decision names no DocType or no field".to_owned(),
                });
            }
            if !kept.insert(entry) {
                return Err(Error::Policy {
                    reason: "a field is kept twice".to_owned(),
                });
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for tag in &self.tags {
            if tag.is_empty()
                || tag.trim() != tag
                || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                return Err(Error::Policy {
                    reason: "a tag is empty or is not a lower-case identifier".to_owned(),
                });
            }
            if !seen.insert(tag.clone()) {
                return Err(Error::Policy {
                    reason: "a tag is listed twice".to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Whether the administrator explicitly decided to keep `doctype.field` in clear.
    #[must_use]
    pub fn keeps(&self, doctype: &str, field: &str) -> bool {
        self.keep
            .iter()
            .any(|entry| entry.doctype == doctype && entry.field == field)
    }

    /// Whether `tag` is one of the policy's closing tags.
    #[must_use]
    pub fn closes_tag(&self, tag: &str) -> bool {
        self.tags.iter().any(|candidate| candidate == tag)
    }
}

/// The whole sandbox plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SandboxPlan {
    /// The production tenant to clone.
    pub source_tenant: String,
    /// The snapshot to restore. `None` means "take a fresh one now"; `Some` pins an existing
    /// snapshot (a nightly backup, for example) and the workflow only verifies it.
    pub snapshot: Option<SnapshotRef>,
    /// What must be masked.
    pub masking_policy: MaskingPolicy,
    /// The new tenant that becomes the sandbox.
    pub sandbox_tenant: String,
}

impl SandboxPlan {
    /// Builds a plan, leaving validation to [`SandboxPlan::validate`].
    #[must_use]
    pub fn new(
        source_tenant: impl Into<String>,
        snapshot: Option<SnapshotRef>,
        masking_policy: MaskingPolicy,
        sandbox_tenant: impl Into<String>,
    ) -> Self {
        Self {
            source_tenant: source_tenant.into(),
            snapshot,
            masking_policy,
            sandbox_tenant: sandbox_tenant.into(),
        }
    }

    /// Validates the plan before any work starts.
    ///
    /// # Errors
    /// [`Error::InvalidPlan`] when a tenant identifier is not a safe name, the source and the
    /// sandbox are the same tenant, a pinned snapshot has no id or provider, or it was taken
    /// from a tenant other than the source.
    pub fn validate(&self) -> Result<()> {
        validate_name("source tenant", &self.source_tenant)?;
        validate_name("sandbox tenant", &self.sandbox_tenant)?;
        if self.source_tenant == self.sandbox_tenant {
            return Err(Error::InvalidPlan {
                reason: "the source tenant and the sandbox tenant are the same; a sandbox is a \
                         copy, never the production tenant"
                    .to_owned(),
            });
        }
        if let Some(snapshot) = &self.snapshot {
            if snapshot.id.is_empty() || snapshot.provider.is_empty() {
                return Err(Error::InvalidPlan {
                    reason: "the pinned snapshot has no id or no provider".to_owned(),
                });
            }
            if snapshot.source_tenant != self.source_tenant {
                return Err(Error::InvalidPlan {
                    reason: "the pinned snapshot was taken from another tenant than the plan's \
                             source"
                        .to_owned(),
                });
            }
        }
        self.masking_policy.validate()?;
        Ok(())
    }
}

/// A tenant name the control plane accepts: 1–63 characters of ASCII letters, digits, `-` and
/// `_`, starting with a letter.
fn validate_name(what: &str, value: &str) -> Result<()> {
    let valid = !value.is_empty()
        && value.len() <= 63
        && value.starts_with(|c: char| c.is_ascii_alphabetic())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(Error::InvalidPlan {
            reason: format!("{what} is not a valid tenant name"),
        })
    }
}

/// One row of the [`FieldPlan`]: a field and what the run will do with it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannedField {
    /// The DocType that holds the field.
    pub doctype: String,
    /// The field's storage name.
    pub field: String,
    /// What the masker does with it.
    pub kind: MaskKind,
    /// Whether it stays in clear because of an explicit Keep decision in the policy.
    pub kept: bool,
}

/// Every field the metadata knows and the class it gets, listed before anything is masked.
///
/// Building it is the check that a run may start: every field has a masking rule, or an
/// explicit Keep decision of the administrator. A field whose classification the policy does
/// not close (a tag or group the policy neither lists nor keeps) has no rule, and the plan
/// refuses to run rather than leave it open by omission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FieldPlan {
    /// Every field, ordered by DocType then field.
    pub fields: Vec<PlannedField>,
}

impl FieldPlan {
    /// Lists every field of `classes` under `policy`.
    ///
    /// # Errors
    /// [`Error::Policy`] when a field has no masking rule and no Keep decision, or when a Keep
    /// decision names a field the metadata does not know. The message names metadata (a
    /// DocType and a field), never a value.
    pub fn build(classes: &dyn ClassificationSource, policy: &MaskingPolicy) -> Result<Self> {
        let mut fields = Vec::new();
        for doctype in classes.doctypes() {
            for name in classes.fields(&doctype) {
                let Some(class) = classes.class(&doctype, &name) else {
                    return Err(Error::Policy {
                        reason: format!("the metadata lists {doctype}.{name} but gives no class"),
                    });
                };
                let kept = policy.keeps(&doctype, &name);
                if kept && (class.ty == FieldType::Secret || class.tags.contains("secret")) {
                    return Err(Error::Policy {
                        reason: format!("{doctype}.{name} is a secret and cannot be kept"),
                    });
                }
                let kind = kind_for(&class, policy);
                if kind == MaskKind::Unprotected && !kept {
                    return Err(Error::Policy {
                        reason: format!(
                            "{doctype}.{name} has a classification with no masking rule in the \
                             policy and no Keep decision"
                        ),
                    });
                }
                fields.push(PlannedField {
                    doctype: doctype.clone(),
                    field: name,
                    kind,
                    kept,
                });
            }
        }
        for entry in &policy.keep {
            if !fields
                .iter()
                .any(|f| f.doctype == entry.doctype && f.field == entry.field)
            {
                return Err(Error::Policy {
                    reason: format!(
                        "a Keep decision names {}.{}, which the metadata does not know",
                        entry.doctype, entry.field
                    ),
                });
            }
        }
        Ok(Self { fields })
    }

    /// The row for `doctype.field`, if listed.
    #[must_use]
    pub fn get(&self, doctype: &str, field: &str) -> Option<&PlannedField> {
        self.fields
            .iter()
            .find(|f| f.doctype == doctype && f.field == field)
    }
}
