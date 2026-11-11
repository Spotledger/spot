//! The namer: the invocation context and the naming entry points.

use std::sync::Arc;

use spot_app_api::Document;
use spot_core::SiteId;
use spot_plan::NamingPlan;
use spot_storage_contract::{ErrorKind, Name, RecordId, Sequences, SeriesKey, Transaction, Value};
use time::{Date, OffsetDateTime, UtcOffset};

use crate::error::NamingError;
use crate::fiscal::{CalendarFiscalYear, FiscalYear};
use crate::idsource::{NamingIdSource, UuidV7Source};
use crate::name::RecordName;
use crate::registry::PlanRegistry;
use crate::series::series_key;
use crate::strategy::{SeriesFormat, Strategy, detect};
use crate::ticket::{Block, GapPolicy, NamingTicket};

/// The facts of one naming invocation.
///
/// It carries the site, the run's instant, and the site's time zone. A date part of a name is
/// the invocation's instant in that zone (ADR-077), unless the caller passes the document's
/// business date; the server's local zone is never read.
#[derive(Clone, Debug)]
pub struct NamingContext {
    /// The tenant the name belongs to (ADR-34).
    pub site: SiteId,
    /// The run's instant, fixed for the whole operation (`Invocation.now`).
    pub now: OffsetDateTime,
    /// The site's business time zone, used to turn `now` into a calendar date.
    pub zone: UtcOffset,
    /// The scope key of a per-company (or per-branch, per-till) series, when the pattern is
    /// scoped. Its presence is the caller's decision; `None` means one series for the site.
    pub scope_key: Option<String>,
    /// The document's business date, when the caller has one; the date parts use it in
    /// preference to `now`.
    pub business_date: Option<Date>,
    /// The name the caller supplies for a `prompt`-named DocType.
    pub supplied_name: Option<String>,
}

impl NamingContext {
    /// A context for one site at one instant in one zone.
    pub fn new(site: SiteId, now: OffsetDateTime, zone: UtcOffset) -> Self {
        Self {
            site,
            now,
            zone,
            scope_key: None,
            business_date: None,
            supplied_name: None,
        }
    }

    /// Sets the series scope key.
    pub fn with_scope_key(mut self, scope: impl Into<String>) -> Self {
        self.scope_key = Some(scope.into());
        self
    }

    /// Sets the document's business date for the date parts.
    pub fn with_business_date(mut self, date: Date) -> Self {
        self.business_date = Some(date);
        self
    }

    /// Sets the caller-supplied name for a `prompt` DocType.
    pub fn with_supplied_name(mut self, name: impl Into<String>) -> Self {
        self.supplied_name = Some(name.into());
        self
    }

    /// The calendar date the date parts use.
    fn date(&self) -> Date {
        self.business_date
            .unwrap_or_else(|| self.now.to_offset(self.zone).date())
    }
}

/// Generates a document's name from its compiled naming plan, taking any counter from the
/// caller's transactional [`Sequences`].
pub struct Namer {
    registry: Arc<dyn PlanRegistry>,
    fiscal: Arc<dyn FiscalYear>,
    ids: Arc<dyn NamingIdSource>,
}

impl Namer {
    /// A namer over `registry`, with the calendar fiscal year and the real UUIDv7 id source.
    pub fn new(registry: Arc<dyn PlanRegistry>) -> Self {
        Self {
            registry,
            fiscal: Arc::new(CalendarFiscalYear),
            ids: Arc::new(UuidV7Source),
        }
    }

    /// Replaces the fiscal-year resolver.
    pub fn with_fiscal_year(mut self, fiscal: Arc<dyn FiscalYear>) -> Self {
        self.fiscal = fiscal;
        self
    }

    /// Replaces the id source (a fixed source makes `hash` naming deterministic in tests).
    pub fn with_id_source(mut self, ids: Arc<dyn NamingIdSource>) -> Self {
        self.ids = ids;
        self
    }

    /// The strategy a DocType's compiled plan asks for.
    ///
    /// # Errors
    ///
    /// [`NamingError::UnknownDoctype`] when the DocType has no plan, or
    /// [`NamingError::PatternInvalid`] when the plan is not a runnable strategy.
    pub fn strategy(&self, doctype: &str) -> Result<Strategy, NamingError> {
        let plan = self.plan(doctype)?;
        detect(doctype, &plan)
    }

    /// Names a document.
    ///
    /// A series name allocates its number inside the caller's transaction: a rollback returns
    /// it, so committed numbers have no gap. The `policy` decides what may be skipped;
    /// [`GapPolicy::Reserved`] must use [`Namer::reserve`] instead.
    ///
    /// # Errors
    ///
    /// [`NamingError`], one of `naming.*`, including `naming.prompt_required` for a `prompt`
    /// DocType with no [`NamingContext::supplied_name`] and `naming.policy_unsupported` for a
    /// reserved policy.
    pub async fn name_for<S: Sequences>(
        &self,
        doctype: &Name,
        doc: &Document,
        ctx: &NamingContext,
        policy: GapPolicy,
        seq: &mut S,
    ) -> Result<RecordName, NamingError> {
        let ticket = self.ticket(doctype, doc, ctx, policy, seq).await?;
        Ok(ticket.commit_name())
    }

    /// Like [`Namer::name_for`], but returns the [`NamingTicket`] so a
    /// [`GapPolicy::GaplessStrict`] caller can commit the name last.
    ///
    /// # Errors
    ///
    /// As [`Namer::name_for`].
    pub async fn ticket<S: Sequences>(
        &self,
        doctype: &Name,
        doc: &Document,
        ctx: &NamingContext,
        policy: GapPolicy,
        seq: &mut S,
    ) -> Result<NamingTicket, NamingError> {
        let plan = self.plan(doctype.as_str())?;
        match detect(doctype.as_str(), &plan)? {
            Strategy::Field(field) => {
                let name = self.field_value(doc, doctype, &field)?;
                Ok(NamingTicket::new(name, None, None, policy))
            }
            Strategy::Hash => {
                let name = RecordName::new(&short_id(self.ids.next_record_id()))?;
                Ok(NamingTicket::new(name, None, None, policy))
            }
            Strategy::Prompt => {
                let supplied =
                    ctx.supplied_name
                        .as_deref()
                        .ok_or_else(|| NamingError::PromptRequired {
                            doctype: doctype.as_str().to_owned(),
                        })?;
                let name = RecordName::new(supplied)?;
                Ok(NamingTicket::new(name, None, None, policy))
            }
            Strategy::Series => {
                if let GapPolicy::Reserved(_) = policy {
                    return Err(NamingError::PolicyUnsupported {
                        message: format!(
                            "the reserved policy allocates a block; call `reserve` for `{doctype}`"
                        ),
                    });
                }
                let format = self.series_format(doctype, ctx, &plan)?;
                let key = series_key(&format.prefix()?, ctx.scope_key.as_deref())?;
                let number = seq.allocate(&key, 1).await.map_err(NamingError::from)?;
                let name = RecordName::new(&format.render(Some(number))?)?;
                Ok(NamingTicket::new(name, Some(number), Some(key), policy))
            }
        }
    }

    /// Reserves a block of numbers for a bulk operation.
    ///
    /// # Errors
    ///
    /// [`NamingError::PolicyUnsupported`] when `policy` is not [`GapPolicy::Reserved`] or the
    /// DocType is not series-named, and [`NamingError::ReserveRefused`] when the policy is
    /// [`GapPolicy::GaplessStrict`].
    pub async fn reserve<S: Sequences>(
        &self,
        doctype: &Name,
        ctx: &NamingContext,
        policy: GapPolicy,
        seq: &mut S,
    ) -> Result<Block, NamingError> {
        let plan = self.plan(doctype.as_str())?;
        if !matches!(detect(doctype.as_str(), &plan)?, Strategy::Series) {
            return Err(NamingError::PolicyUnsupported {
                message: format!(
                    "only a series-named DocType can reserve a block, not `{doctype}`"
                ),
            });
        }
        let format = self.series_format(doctype, ctx, &plan)?;
        let key = series_key(&format.prefix()?, ctx.scope_key.as_deref())?;
        let count = match policy {
            GapPolicy::Reserved(count) => count,
            GapPolicy::GaplessStrict => {
                return Err(NamingError::ReserveRefused {
                    series: key.as_str().to_owned(),
                });
            }
            GapPolicy::Tolerant => {
                return Err(NamingError::PolicyUnsupported {
                    message: "reserving a block needs the reserved policy".to_owned(),
                });
            }
        };
        if count == 0 {
            return Err(NamingError::PolicyUnsupported {
                message: "a reserved block holds at least one number".to_owned(),
            });
        }
        let first = seq.allocate(&key, count).await.map_err(NamingError::from)?;
        Ok(Block::new(key, format, first, count))
    }

    /// Advances a series by `count` numbers without issuing a name (a deliberate gap).
    ///
    /// # Errors
    ///
    /// [`NamingError::SkipRefused`] under [`GapPolicy::GaplessStrict`], and any storage
    /// failure.
    pub async fn skip<S: Sequences>(
        &self,
        policy: GapPolicy,
        series: &SeriesKey,
        count: u32,
        seq: &mut S,
    ) -> Result<(), NamingError> {
        if policy.is_strict() {
            return Err(NamingError::SkipRefused {
                series: series.as_str().to_owned(),
                count,
            });
        }
        seq.allocate(series, count)
            .await
            .map_err(NamingError::from)?;
        Ok(())
    }

    /// The series key a DocType's pattern resolves to for this context, without allocating.
    ///
    /// # Errors
    ///
    /// [`NamingError::UnknownDoctype`], [`NamingError::PatternInvalid`] when the DocType is not
    /// series-named, or [`NamingError::SeriesKeyInvalid`].
    pub fn series_key(
        &self,
        doctype: &Name,
        ctx: &NamingContext,
    ) -> Result<SeriesKey, NamingError> {
        let plan = self.plan(doctype.as_str())?;
        if !matches!(detect(doctype.as_str(), &plan)?, Strategy::Series) {
            return Err(NamingError::PatternInvalid {
                doctype: doctype.as_str().to_owned(),
            });
        }
        let format = self.series_format(doctype, ctx, &plan)?;
        series_key(&format.prefix()?, ctx.scope_key.as_deref())
    }

    /// Sets the next number a series hands out, forward only.
    ///
    /// # Errors
    ///
    /// [`NamingError::SeriesBackwards`] when the allocator refuses a number already handed out
    /// (`InvalidValue`), and any other storage failure.
    pub async fn set_next<S: Sequences>(
        &self,
        series: &SeriesKey,
        next: u64,
        seq: &mut S,
    ) -> Result<(), NamingError> {
        seq.set_next(series, next).await.map_err(|e| {
            if e.kind == ErrorKind::InvalidValue {
                NamingError::SeriesBackwards {
                    series: series.as_str().to_owned(),
                    next,
                }
            } else {
                NamingError::Storage(e)
            }
        })
    }

    /// Reads a series' next number without consuming it.
    ///
    /// [`Sequences`] offers no read, so this peeks through a savepoint on the full
    /// [`Transaction`]: it allocates one, rolls the savepoint back, and returns the number.
    ///
    /// # Errors
    ///
    /// Any storage failure from the savepoint, allocation or rollback.
    pub async fn peek<T: Transaction>(
        &self,
        series: &SeriesKey,
        tx: &mut T,
    ) -> Result<u64, NamingError> {
        let savepoint = tx.savepoint().await.map_err(NamingError::from)?;
        match tx.allocate(series, 1).await {
            Ok(number) => {
                tx.rollback_to(savepoint).await.map_err(NamingError::from)?;
                Ok(number)
            }
            Err(e) => {
                let _ = tx.rollback_to(savepoint).await;
                Err(NamingError::from(e))
            }
        }
    }

    fn plan(&self, doctype: &str) -> Result<NamingPlan, NamingError> {
        self.registry
            .naming_plan(doctype)
            .ok_or_else(|| NamingError::UnknownDoctype {
                doctype: doctype.to_owned(),
            })
    }

    fn series_format(
        &self,
        doctype: &Name,
        ctx: &NamingContext,
        plan: &NamingPlan,
    ) -> Result<SeriesFormat, NamingError> {
        if !matches!(detect(doctype.as_str(), plan)?, Strategy::Series) {
            return Err(NamingError::PatternInvalid {
                doctype: doctype.as_str().to_owned(),
            });
        }
        Ok(SeriesFormat::new(
            plan.parts.clone(),
            ctx.date(),
            Arc::clone(&self.fiscal),
        ))
    }

    fn field_value(
        &self,
        doc: &Document,
        doctype: &Name,
        field: &str,
    ) -> Result<RecordName, NamingError> {
        match doc.get(field) {
            Value::Text(text) => RecordName::new(text),
            Value::Int(number) => RecordName::new(&number.to_string()),
            Value::Id(id) => RecordName::new(&id.to_string()),
            Value::Null => Err(NamingError::MissingField {
                doctype: doctype.as_str().to_owned(),
                field: field.to_owned(),
            }),
            _ => Err(NamingError::InvalidName {
                value: format!("<the value of `{field}`>"),
                reason: "the field is not text, an integer or a link",
            }),
        }
    }
}

/// A short, printable id derived from a UUIDv7: Crockford base32 of its 16 bytes.
fn short_id(id: RecordId) -> String {
    const ALPHABET: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";
    let mut out = String::with_capacity(26);
    let mut buffer: u64 = 0;
    let mut bits: u32 = 0;
    for &byte in id.as_bytes() {
        buffer = (buffer << 8) | u64::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            let index = usize::try_from((buffer >> bits) & 0x1f).unwrap_or(0);
            out.push(ALPHABET[index] as char);
        }
    }
    if bits > 0 {
        let index = usize::try_from((buffer << (5 - bits)) & 0x1f).unwrap_or(0);
        out.push(ALPHABET[index] as char);
    }
    out
}
