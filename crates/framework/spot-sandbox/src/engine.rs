//! The masking engine: one pass over every surface of a closed sandbox, and the ledger guard
//! that proves no amount changed.
//!
//! The engine reads the effective metadata ([`ClassificationSource`]) and the plan's policy,
//! masks documents, journal rows and audit diffs, replaces attachments, strips secrets and
//! integrations and purges caches and the search index. It returns the counts a reviewer can
//! read and the [`SourceIndex`] the leak scan needs — never a value.
//!
//! # Referential integrity
//!
//! A pseudonym depends on the value's shape and the sandbox key, not on the field that holds
//! it. A `fetch_from` copy of `customer.name` therefore receives exactly the pseudonym the
//! source field received, in every document and in every journal and audit row. Link fields
//! keep their record ids; the identity on the target is what changes.
//!
//! # Ledger guard
//!
//! Before the journal is written back, every row whose field is an amount (a numeric field
//! tagged `financial`) is compared with its original value. If any amount moved, masking fails
//! with `sandbox.mask`: a sandbox whose ledger does not balance is not a sandbox, it is a bug
//! factory.

use std::collections::BTreeMap;

use serde_json::Value;
use spot_meta::FieldType;

use crate::classify::{ClassificationSource, FieldClass};
use crate::data::{JournalRow, SandboxStore};
use crate::error::{Error, Result};
use crate::mask::{MaskKind, MaskReport, Masker, SandboxKey, kind_for};
use crate::plan::{FieldPlan, MaskingPolicy};
use crate::scan::SourceIndex;

/// What one masking run produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaskOutcome {
    /// The counts; no value, no key.
    pub report: MaskReport,
    /// The original values of masked fields, digest-only, for the leak scan.
    pub index: SourceIndex,
    /// Every field and the class it got; the run only starts when this lists a rule for each.
    pub field_plan: FieldPlan,
}

/// Masks a sandbox from the effective metadata and a policy.
pub struct MaskingEngine<'a> {
    classes: &'a dyn ClassificationSource,
    policy: &'a MaskingPolicy,
}

impl<'a> MaskingEngine<'a> {
    /// An engine over `classes` and `policy`.
    #[must_use]
    pub fn new(classes: &'a dyn ClassificationSource, policy: &'a MaskingPolicy) -> Self {
        Self { classes, policy }
    }

    /// The policy the engine applies.
    #[must_use]
    pub fn policy(&self) -> &MaskingPolicy {
        self.policy
    }

    /// Lists every field of the metadata and its class under the policy.
    ///
    /// # Errors
    /// [`Error::Policy`] when a field has no masking rule and no Keep decision.
    pub fn field_plan(&self) -> Result<FieldPlan> {
        FieldPlan::build(self.classes, self.policy)
    }

    /// Masks every surface of `store` with `key`. The key is consumed and wiped. Refuses to
    /// run when the field plan finds a field with no masking rule.
    ///
    /// # Errors
    /// [`Error::Policy`] from the field plan, [`Error::Store`] when a surface cannot be read or written, and [`Error::Mask`] when the
    /// ledger guard finds that an amount changed.
    pub fn mask(&self, store: &mut dyn SandboxStore, key: SandboxKey) -> Result<MaskOutcome> {
        // Fail closed: with no metadata nothing would be classified, nothing masked, and the
        // leak scan would have an empty index and pass on a clear-text clone.
        if self.classes.doctypes().is_empty() {
            return Err(Error::Mask {
                reason: "the classification source knows no DocType; refusing to mask without \
                         metadata"
                    .to_owned(),
            });
        }
        let field_plan = self.field_plan()?;
        let masker = Masker::new(key);
        let mut index = SourceIndex::new(self.policy.scan_min_chars);
        let mut report = MaskReport::default();

        self.mask_documents(store, &masker, &mut index, &mut report)?;
        self.mask_journal(store, &masker, &mut index, &mut report)?;
        self.mask_audit(store, &masker, &mut index, &mut report)?;
        self.mask_attachments(store, &masker, &mut index, &mut report)?;
        self.purge_search(store, &mut report)?;
        self.purge_caches(store, &mut report)?;
        self.strip_secrets(store, &mut index, &mut report)?;
        self.strip_integrations(store, &mut index, &mut report)?;

        report.key_discarded = true;
        drop(masker);
        Ok(MaskOutcome {
            report,
            index,
            field_plan,
        })
    }

    fn mask_documents(
        &self,
        store: &mut dyn SandboxStore,
        masker: &Masker,
        index: &mut SourceIndex,
        report: &mut MaskReport,
    ) -> Result<()> {
        let mut documents = store.documents();
        report.documents = documents.len();
        for document in &mut documents {
            let fields: Vec<String> = document.fields.keys().cloned().collect();
            for field in fields {
                let class = self
                    .classes
                    .class(&document.doctype, &field)
                    .unwrap_or_else(|| {
                        FieldClass::plain(&document.doctype, &field, FieldType::Text)
                    });
                if !self.policy.mask_derived_copies && class.derived_from.is_some() {
                    continue;
                }
                let kind = kind_for(&class, self.policy);
                match kind {
                    MaskKind::Unprotected => continue,
                    MaskKind::AmountKept => {
                        report.amounts_kept += 1;
                        continue;
                    }
                    MaskKind::LinkKept => {
                        report.links_kept += 1;
                        continue;
                    }
                    MaskKind::Attachment => {
                        // The document keeps the file id; the attachment row is replaced below.
                        report.links_kept += 1;
                        continue;
                    }
                    MaskKind::Identifier | MaskKind::FreeText | MaskKind::Secret => {}
                }
                let Some(value) = document.fields.get(&field).cloned() else {
                    continue;
                };
                index.add_json(&value);
                let masked = masker.mask_value(kind, &value, &self.policy.attachment_placeholder);
                if masked == value {
                    continue;
                }
                document.fields.insert(field, masked);
                report.fields_masked += 1;
                if kind == MaskKind::FreeText {
                    report.free_text_scrubbed += 1;
                }
                if class.derived_from.is_some() {
                    report.derived_masked += 1;
                }
            }
        }
        store.replace_documents(documents)
    }

    fn mask_journal(
        &self,
        store: &mut dyn SandboxStore,
        masker: &Masker,
        index: &mut SourceIndex,
        report: &mut MaskReport,
    ) -> Result<()> {
        let before = store.journal();
        let mut rows = before.clone();
        for row in &mut rows {
            let class = self
                .classes
                .class(&row.doctype, &row.field)
                .unwrap_or_else(|| FieldClass::plain(&row.doctype, &row.field, FieldType::Text));
            let kind = kind_for(&class, self.policy);
            if kind == MaskKind::AmountKept {
                report.amounts_kept += 1;
                continue;
            }
            if !kind.masks() {
                continue;
            }
            index.add_json(&row.value);
            let masked = masker.mask_value(kind, &row.value, &self.policy.attachment_placeholder);
            if masked != row.value {
                row.value = masked;
                report.journal_masked += 1;
            }
        }
        if !amounts_preserved(&before, &rows, self.classes, self.policy) {
            return Err(Error::Mask {
                reason: "a ledger amount changed while masking; refused to write the journal"
                    .to_owned(),
            });
        }
        report.ledger_checked = true;
        store.replace_journal(rows)
    }

    fn mask_audit(
        &self,
        store: &mut dyn SandboxStore,
        masker: &Masker,
        index: &mut SourceIndex,
        report: &mut MaskReport,
    ) -> Result<()> {
        let mut rows = store.audit();
        for row in &mut rows {
            let class = self
                .classes
                .class(&row.doctype, &row.field)
                .unwrap_or_else(|| FieldClass::plain(&row.doctype, &row.field, FieldType::Text));
            let kind = kind_for(&class, self.policy);
            if kind == MaskKind::AmountKept {
                report.amounts_kept += 1;
                continue;
            }
            if !kind.masks() {
                continue;
            }
            let mut changed = false;
            for side in [&mut row.before, &mut row.after] {
                index.add_json(side);
                let masked = masker.mask_value(kind, side, &self.policy.attachment_placeholder);
                if masked != *side {
                    *side = masked;
                    changed = true;
                }
            }
            if changed {
                report.audit_masked += 1;
            }
        }
        store.replace_audit(rows)
    }

    fn mask_attachments(
        &self,
        store: &mut dyn SandboxStore,
        masker: &Masker,
        index: &mut SourceIndex,
        report: &mut MaskReport,
    ) -> Result<()> {
        let mut attachments = store.attachments();
        for attachment in &mut attachments {
            // Every attachment is personal data by nature: its name and its content are
            // indexed, and the row keeps only a placeholder.
            index.add(&attachment.filename);
            index.add(&attachment.content);
            index.add(&attachment.content_hash);
            attachment.filename = masker.attachment_name(&attachment.filename);
            attachment.content_hash = masker.tag("attachment-hash", &attachment.content_hash);
            attachment.content = self.policy.attachment_placeholder.clone();
            attachment.size = 0;
            report.attachments_replaced += 1;
        }
        store.replace_attachments(attachments)
    }

    fn purge_search(&self, store: &mut dyn SandboxStore, report: &mut MaskReport) -> Result<()> {
        let rows = store.search();
        report.search_rows_purged = rows.len();
        store.replace_search(Vec::new())
    }

    fn purge_caches(&self, store: &mut dyn SandboxStore, report: &mut MaskReport) -> Result<()> {
        let rows = store.caches();
        report.caches_purged = rows.len();
        store.replace_caches(Vec::new())
    }

    fn strip_secrets(
        &self,
        store: &mut dyn SandboxStore,
        index: &mut SourceIndex,
        report: &mut MaskReport,
    ) -> Result<()> {
        let rows = store.secrets();
        for row in &rows {
            index.add(&row.value);
            index.add(&row.name);
        }
        report.secrets_stripped = rows.len();
        store.replace_secrets(Vec::new())
    }

    fn strip_integrations(
        &self,
        store: &mut dyn SandboxStore,
        index: &mut SourceIndex,
        report: &mut MaskReport,
    ) -> Result<()> {
        let rows = store.integrations();
        for row in &rows {
            index.add(&row.endpoint);
            index.add(&row.credential);
        }
        report.integrations_stripped = rows.len();
        store.replace_integrations(Vec::new())
    }
}

/// Every amount field of the journal has the same value before and after masking.
fn amounts_preserved(
    before: &[JournalRow],
    after: &[JournalRow],
    classes: &dyn ClassificationSource,
    policy: &MaskingPolicy,
) -> bool {
    let mut amounts: BTreeMap<(u64, String), Value> = BTreeMap::new();
    for row in before {
        if is_amount(classes, policy, &row.doctype, &row.field) {
            amounts.insert((row.seq, row.field.clone()), row.value.clone());
        }
    }
    if amounts.is_empty() {
        return true;
    }
    for row in after {
        let key = (row.seq, row.field.clone());
        if let Some(expected) = amounts.get(&key)
            && *expected != row.value
        {
            return false;
        }
    }
    true
}

fn is_amount(
    classes: &dyn ClassificationSource,
    policy: &MaskingPolicy,
    doctype: &str,
    field: &str,
) -> bool {
    classes
        .class(doctype, field)
        .is_some_and(|class| kind_for(&class, policy) == MaskKind::AmountKept)
}
