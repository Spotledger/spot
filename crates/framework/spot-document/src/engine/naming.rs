//! Naming new documents (WP-DOC-05.T2; ADR-33, ADR-46, ADR-059 "Numbering", ADR-077).
//!
//! A DocType with a `naming` property gets its name when it is created: the engine asks
//! [`spot_naming::Namer`] for it **inside the operation's own storage transaction**, with that
//! transaction's own `Sequences`, and writes it into the DocType's `title_field`. A rollback of
//! the operation (a refusal, a storage error, or the rerun of a retryable one) returns the number
//! to the series, so committed numbers have no gaps.
//!
//! The pattern is compiled once per snapshot ([`NamingTable`]). A DocType with `naming` but no
//! `title_field` is refused when the snapshot is built, not at the first create.
//!
//! The date parts of a series come from the invocation's instant in the site's business zone
//! ([`BusinessZone`]), never from the server clock or its local zone (ADR-077).

use std::collections::BTreeMap;
use std::sync::Arc;

use spot_app_api::context::Invocation;
use spot_app_api::error::StorageOp;
use spot_app_api::prelude::{Name, SiteId, Value};
use spot_app_api::{AppError, AppResult, DocTypeDef, Document, ErrorClass, FieldPath};
use spot_meta::EffectiveMeta;
use spot_naming::{GapPolicy, Namer, NamingContext, NamingError, PlanRegistry, Strategy};
use spot_plan::NamingPlan;
use spot_storage_contract::{
    Expr, LockKey, LockMode, Locks, Nulls, Order, OrderDir, Query, QueryExecutor, QueryPath,
    Selection, Transaction, Wait,
};
use time::UtcOffset;

use super::Engine;
use super::participant::Cause;
use super::work::Work;
use crate::lookup::name;

/// Which business time zone a site's documents are dated in (ADR-077).
pub trait BusinessZone: Send + Sync + 'static {
    /// The UTC offset of the site's business zone at the invocation's instant.
    fn offset(&self, site: &SiteId, now: time::OffsetDateTime) -> UtcOffset;
}

/// Every site is dated in UTC.
#[derive(Clone, Copy, Debug, Default)]
pub struct UtcZone;

impl BusinessZone for UtcZone {
    fn offset(&self, _site: &SiteId, _now: time::OffsetDateTime) -> UtcOffset {
        UtcOffset::UTC
    }
}

/// Every site is dated in one fixed offset.
#[derive(Clone, Copy, Debug)]
pub struct FixedZone(pub UtcOffset);

impl BusinessZone for FixedZone {
    fn offset(&self, _site: &SiteId, _now: time::OffsetDateTime) -> UtcOffset {
        self.0
    }
}

/// One DocType's compiled naming.
#[derive(Clone, Debug)]
pub(crate) struct DocNaming {
    pub(crate) strategy: Strategy,
    pub(crate) title_field: Name,
}

/// The compiled naming plans of one metadata snapshot.
#[derive(Clone, Debug, Default)]
pub(crate) struct NamingTable {
    docs: BTreeMap<String, DocNaming>,
    plans: BTreeMap<String, NamingPlan>,
}

impl PlanRegistry for NamingTable {
    fn naming_plan(&self, doctype: &str) -> Option<NamingPlan> {
        self.plans.get(doctype).cloned()
    }
}

fn config_error(code: &str, message: String) -> AppError {
    AppError::new(ErrorClass::Internal, code, message)
}

impl NamingTable {
    /// Compiles every DocType's naming property.
    ///
    /// # Errors
    ///
    /// `document.naming_title_field_missing` for a DocType with `naming` and no `title_field`;
    /// `document.naming_pattern_invalid` for a pattern that is not one of the four strategies.
    pub(crate) fn build(meta: &EffectiveMeta) -> AppResult<Self> {
        let mut table = Self::default();
        for def in meta.definitions() {
            let Some(pattern) = def.naming.as_deref() else {
                continue;
            };
            if def.is_child {
                continue;
            }
            let Some(title) = def.title_field.as_deref() else {
                return Err(config_error(
                    "document.naming_title_field_missing",
                    format!(
                        "The DocType `{}` has a naming property but no title field to hold the name.",
                        def.name
                    ),
                ));
            };
            let invalid = |why: &str| {
                config_error(
                    "document.naming_pattern_invalid",
                    format!(
                        "The naming property of `{}` (`{pattern}`) is not usable: {why}.",
                        def.name
                    ),
                )
            };
            let plan = NamingPlan::parse(pattern).map_err(|e| invalid(&e))?;
            table.plans.insert(def.name.clone(), plan);
            let strategy = Namer::new(Arc::new(table.clone()))
                .strategy(&def.name)
                .map_err(|e| invalid(&e.to_string()))?;
            table.docs.insert(
                def.name.clone(),
                DocNaming {
                    strategy,
                    title_field: name(title)?,
                },
            );
        }
        Ok(table)
    }

    pub(crate) fn get(&self, doctype: &str) -> Option<&DocNaming> {
        self.docs.get(doctype)
    }

    /// The title field to clear on an amended copy: the name of a series or hash DocType is
    /// never copied (the copy gets its own).
    pub(crate) fn generated_title(&self, doctype: &str) -> Option<&Name> {
        let naming = self.docs.get(doctype)?;
        matches!(naming.strategy, Strategy::Series | Strategy::Hash).then_some(&naming.title_field)
    }

    /// The title field of a DocType whose name the document itself carries (`field:` or
    /// `prompt`): the copy made by `correct` cannot keep that name, so it takes `<base>-<n>`
    /// ([`Engine::amendment_name`]). `None` for a generated name and for no naming at all.
    pub(crate) fn amended_title(&self, doctype: &str) -> Option<&Name> {
        let naming = self.docs.get(doctype)?;
        matches!(naming.strategy, Strategy::Field(_) | Strategy::Prompt)
            .then_some(&naming.title_field)
    }

    /// Whether the DocType's name is generated by the system (a series or a hash), so `rename`
    /// has nothing to rewrite: the name is not a title a caller chooses.
    pub(crate) fn generates_name(&self, doctype: &str) -> bool {
        self.docs
            .get(doctype)
            .is_some_and(|naming| matches!(naming.strategy, Strategy::Series | Strategy::Hash))
    }
}

/// The text of the title field, when it holds a non-blank one.
fn supplied(doc: &Document, title: &Name) -> Option<String> {
    match doc.get(title.as_str()) {
        Value::Text(text) if !text.trim().is_empty() => Some(text.clone()),
        _ => None,
    }
}

/// A refusal on the title field.
fn title_error(title: &Name, code: &str, message: String) -> AppError {
    AppError::field(FieldPath::field(title.as_str()), code, message)
}

fn map_naming_error<T: Transaction>(
    work: &mut Work<T>,
    title: &Name,
    error: NamingError,
) -> AppError {
    let code = error.code();
    match error {
        NamingError::Duplicate { .. } => duplicate_name(title, &error.to_string()),
        NamingError::PromptRequired { .. }
        | NamingError::InvalidName { .. }
        | NamingError::MissingField { .. } => title_error(title, code, error.to_string()),
        NamingError::Storage(e) => work.fail(e, StorageOp::Write),
        other => AppError::new(ErrorClass::Internal, code, other.to_string()),
    }
}

fn duplicate_name(title: &Name, message: &str) -> AppError {
    // The error's own code is the catalogued `document.duplicate` (users get its help);
    // the field error says it was the name.
    AppError::new(
        ErrorClass::Duplicate,
        "document.duplicate",
        message.to_owned(),
    )
    .with_field(
        FieldPath::field(title.as_str()),
        "document.duplicate_name",
        message.to_owned(),
    )
}

/// The refusal of a rename on a DocType whose name the system generates (a series or a hash):
/// the name is not a title the caller owns, so there is nothing to rewrite.
pub(super) fn name_not_renameable(def: &DocTypeDef, title: &Name) -> AppError {
    title_error(
        title,
        "document.name_not_renameable",
        format!(
            "The name of a `{}` is generated; a document of this type cannot be renamed.",
            def.name
        ),
    )
}

/// The base an amendment's name is built from: the original name without one trailing
/// `-<digits>` segment (the base of `ACME-2` is `ACME`, and of `ACME` is `ACME`). A segment with
/// no base in front of it (`-5`) or with anything but digits in it (`ACME-x`) is part of the
/// name, not a suffix.
pub(super) fn amendment_base(original: &str) -> &str {
    match original.rsplit_once('-') {
        Some((base, suffix))
            if !base.is_empty()
                && !suffix.is_empty()
                && suffix.bytes().all(|b| b.is_ascii_digit()) =>
        {
            base
        }
        _ => original,
    }
}

/// How many suffixes an amendment tries before it gives up: a DocType with a million
/// amendments of one base is a runaway, not a business case.
const MAX_SUFFIX: u64 = 1_000_000;

impl<B: spot_storage_contract::StorageBackend> Engine<B> {
    /// Names a document being created, in its transaction, before the checks and the write.
    /// A DocType without a naming property is returned as it came.
    pub(super) async fn assign_name(
        &self,
        snap: &super::Snapshot,
        work: &mut Work<B::Tx>,
        inv: &Invocation,
        def: &DocTypeDef,
        mut doc: Document,
    ) -> AppResult<Document> {
        let Some(naming) = snap.naming.get(def.name.as_str()) else {
            return Ok(doc);
        };
        let title = naming.title_field.clone();
        let given = supplied(&doc, &title);
        // A `correct` gives its amendment a name it chose for it (`<base>-<n>`, see
        // `amendment_name`); it is used as it is instead of being derived again, which for a
        // `field:` DocType would produce the original's name.
        let amended = match work.cause {
            Cause::CorrectAmend => work.amended_name.take(),
            _ => None,
        };
        let mut ctx = NamingContext::new(
            inv.site.clone(),
            inv.now,
            self.zone.offset(&inv.site, inv.now),
        );
        match naming.strategy {
            Strategy::Series | Strategy::Hash if given.is_some() => {
                return Err(title_error(
                    &title,
                    "document.name_not_assignable",
                    format!(
                        "The name of a `{}` is generated; a caller cannot choose it.",
                        def.name
                    ),
                ));
            }
            Strategy::Prompt => {
                if let Some(amended) = amended.clone() {
                    ctx = ctx.with_supplied_name(amended);
                } else if let Some(given) = given {
                    ctx = ctx.with_supplied_name(given);
                }
            }
            _ => {}
        }
        let record_name = match (&naming.strategy, amended) {
            (Strategy::Field(_), Some(amended)) => amended,
            _ => {
                let namer = Namer::new(Arc::new(snap.naming.clone()))
                    .with_id_source(Arc::clone(&self.naming_ids));
                namer
                    .name_for(
                        &def.name,
                        &doc,
                        &ctx,
                        GapPolicy::GaplessStrict,
                        &mut work.tx,
                    )
                    .await
                    .map_err(|e| map_naming_error(work, &title, e))?
                    .as_str()
                    .to_owned()
            }
        };
        if !matches!(naming.strategy, Strategy::Series) {
            self.refuse_taken_name(work, def, &title, &record_name)
                .await?;
        }
        doc.set(title, Value::Text(record_name));
        Ok(doc)
    }

    /// Takes the transaction lock on a name, so two concurrent writers of the same name (a
    /// create and a rename too) cannot both pass the check that follows.
    pub(super) async fn lock_name(
        &self,
        work: &mut Work<B::Tx>,
        def: &DocTypeDef,
        wanted: &str,
    ) -> AppResult<()> {
        let key = LockKey {
            namespace: name("doc_name")?,
            key: format!("{}\u{1f}{wanted}", def.name),
        };
        work.tx
            .lock_key(&key, LockMode::Exclusive, Wait::Block)
            .await
            .map_err(|e| work.fail(e, StorageOp::Write))
    }

    /// Refuses a name another document of the DocType already holds. Names that a caller or a
    /// field supplies are guarded by a transaction lock on the name, so two concurrent creates
    /// of the same name cannot both pass the check.
    async fn refuse_taken_name(
        &self,
        work: &mut Work<B::Tx>,
        def: &DocTypeDef,
        title: &Name,
        wanted: &str,
    ) -> AppResult<()> {
        if self.name_taken(work, def, title, wanted).await? {
            let error = NamingError::duplicate(def.name.as_str(), wanted);
            Err(duplicate_name(title, &error.to_string()))
        } else {
            Ok(())
        }
    }

    /// The name an amended copy takes: `<base>-<n>`, where `base` is the original name without
    /// one trailing `-<digits>` segment (the base of `ACME-2` is `ACME`, so amending `ACME-1`
    /// gives `ACME-2`, never `ACME-1-1`) and `n` is the smallest integer at least 1 that no
    /// document of the DocType holds. Every candidate is checked under the lock a create takes
    /// on that name, inside this transaction, so two corrections choosing for the same base
    /// cannot take one suffix.
    ///
    /// Nothing is allocated from a series: a rolled-back or refused correct leaves no suffix
    /// behind, and the next one takes the smallest free one again.
    ///
    /// # Errors
    ///
    /// The title field's refusal when the candidate is not a legal name, and
    /// `document.internal_error` when [`MAX_SUFFIX`] candidates are all taken.
    pub(super) async fn amendment_name(
        &self,
        work: &mut Work<B::Tx>,
        def: &DocTypeDef,
        title: &Name,
        original: &str,
    ) -> AppResult<String> {
        let base = amendment_base(original);
        for suffix in 1..=MAX_SUFFIX {
            let candidate = format!("{base}-{suffix}");
            // A name the storage contract would refuse is refused here, on the title field,
            // never truncated to fit.
            let candidate = match spot_naming::RecordName::new(&candidate) {
                Ok(candidate) => candidate.as_str().to_owned(),
                Err(e) => return Err(map_naming_error(work, title, e)),
            };
            if !self.name_taken(work, def, title, &candidate).await? {
                return Ok(candidate);
            }
        }
        Err(AppError::new(
            ErrorClass::Internal,
            "document.internal_error",
            format!(
                "Every name from `{base}-1` to `{base}-{MAX_SUFFIX}` is taken; the amendment of `{original}` cannot be named."
            ),
        ))
    }

    /// Whether another document of the DocType holds `wanted` as its name, checked under the
    /// transaction lock on that name.
    async fn name_taken(
        &self,
        work: &mut Work<B::Tx>,
        def: &DocTypeDef,
        title: &Name,
        wanted: &str,
    ) -> AppResult<bool> {
        self.lock_name(work, def, wanted).await?;
        let alias = name("d")?;
        let id_col = name("id")?;
        let mut query = Query::from(&def.name, &alias, QueryPath::Transactional);
        query.select = vec![Selection {
            expr: Expr::field(&alias, &id_col),
            alias: id_col.clone(),
        }];
        query.filter =
            Some(Expr::field(&alias, title).eq(Expr::value(Value::Text(wanted.to_owned()))));
        query.order_by = vec![Order {
            expr: Expr::field(&alias, &id_col),
            dir: OrderDir::Asc,
            nulls: Nulls::Last,
        }];
        query.limit = Some(1);
        let rows = work
            .tx
            .query(&query)
            .await
            .map_err(|e| work.fail(e, StorageOp::Write))?;
        Ok(!rows.rows.is_empty())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn the_base_of_an_amended_name_drops_one_trailing_number() {
        // One trailing `-<digits>` segment is the suffix an amendment put there.
        assert_eq!(amendment_base("ACME-2"), "ACME");
        assert_eq!(amendment_base("ACME-12"), "ACME");
        assert_eq!(amendment_base("ACME-05"), "ACME");
        assert_eq!(amendment_base("INV-ACME-1"), "INV-ACME");
        // A name without one is its own base, so the first amendment is `-1`.
        assert_eq!(amendment_base("ACME"), "ACME");
        assert_eq!(amendment_base("ACME-"), "ACME-");
        assert_eq!(amendment_base("ACME-x"), "ACME-x");
        assert_eq!(amendment_base("-5"), "-5");
        assert_eq!(amendment_base(""), "");
    }
}
