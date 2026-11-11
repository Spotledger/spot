//! Sandbox-safe custom objects for money and points in time.
//!
//! These objects exist so decimal amounts, dates and datetimes can flow
//! through templates without ever becoming floats (ADR-12) and without
//! exposing compute-capable methods. Arithmetic operators on them use the
//! engine defaults, which fail with `InvalidOperation`, and the engine
//! maps that failure to [`crate::TemplateError::NotComputable`] whose
//! message states that templates do not compute.
//!
//! Deviation note (recorded in the task report): the task brief asks for
//! comparisons on decimals to be errors too, but MiniJinja 2.24's
//! comparison path cannot raise errors for object operands — its `Ord`
//! fallback silently orders plain objects. `DecimalObject` therefore
//! implements `custom_cmp` so decimal-vs-decimal comparisons order
//! numerically (exact `Decimal` ordering, never text, never f64) instead
//! of by string; decimal-vs-non-decimal comparisons keep MiniJinja's
//! silent kind ordering. `DateObject`/`DateTimeObject` render as ISO 8601,
//! whose text ordering is chronological, so no `custom_cmp` is needed
//! there.

use std::cmp::Ordering;
use std::fmt;
use std::sync::Arc;

use minijinja::value::{DynObject, Object, ObjectRepr, Value};
use minijinja::{Error, ErrorKind, State};
use rust_decimal::Decimal;
use time::{Date, OffsetDateTime};

/// An exact decimal amount as a template value.
#[derive(Debug, Clone)]
pub(crate) struct DecimalObject {
    amount: Decimal,
}

impl DecimalObject {
    /// Wraps an amount.
    pub(crate) fn new(amount: Decimal) -> Self {
        Self { amount }
    }

    /// The wrapped amount.
    pub(crate) fn amount(&self) -> Decimal {
        self.amount
    }
}

impl Object for DecimalObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn is_true(self: &Arc<Self>) -> bool {
        self.amount != Decimal::ZERO
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result
    where
        Self: Sized + 'static,
    {
        write!(f, "{}", self.amount)
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        method: &str,
        _args: &[Value],
    ) -> Result<Value, Error> {
        Err(Error::new(
            ErrorKind::InvalidOperation,
            format!(
                "decimals are read-only in templates: method '{method}' is \
                 not available; money is formatted with filters, never \
                 computed (ADR-12)"
            ),
        ))
    }

    fn custom_cmp(self: &Arc<Self>, other: &DynObject) -> Option<Ordering> {
        let other = other.downcast_ref::<DecimalObject>()?;
        Some(self.amount.cmp(&other.amount))
    }
}

/// A calendar date as a template value; renders as ISO 8601 (`2025-01-15`).
#[derive(Debug, Clone)]
pub(crate) struct DateObject {
    date: Date,
}

impl DateObject {
    /// Wraps a date.
    pub(crate) fn new(date: Date) -> Self {
        Self { date }
    }

    /// The wrapped date.
    pub(crate) fn date(&self) -> Date {
        self.date
    }
}

impl Object for DateObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn is_true(self: &Arc<Self>) -> bool {
        true
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result
    where
        Self: Sized + 'static,
    {
        write!(f, "{}", self.date)
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        method: &str,
        _args: &[Value],
    ) -> Result<Value, Error> {
        Err(Error::new(
            ErrorKind::InvalidOperation,
            format!(
                "dates are read-only in templates: method '{method}' is not \
                 available; use the 'date' filter"
            ),
        ))
    }
}

/// A point in time with a UTC offset as a template value; renders as ISO 8601
/// with the offset.
#[derive(Debug, Clone)]
pub(crate) struct DateTimeObject {
    datetime: OffsetDateTime,
}

impl DateTimeObject {
    /// Wraps a datetime.
    pub(crate) fn new(datetime: OffsetDateTime) -> Self {
        Self { datetime }
    }

    /// The wrapped datetime.
    pub(crate) fn datetime(&self) -> OffsetDateTime {
        self.datetime
    }
}

impl Object for DateTimeObject {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn is_true(self: &Arc<Self>) -> bool {
        true
    }

    fn render(self: &Arc<Self>, f: &mut fmt::Formatter<'_>) -> fmt::Result
    where
        Self: Sized + 'static,
    {
        write!(f, "{}", self.datetime)
    }

    fn call_method(
        self: &Arc<Self>,
        _state: &State<'_, '_>,
        method: &str,
        _args: &[Value],
    ) -> Result<Value, Error> {
        Err(Error::new(
            ErrorKind::InvalidOperation,
            format!(
                "datetimes are read-only in templates: method '{method}' is \
                 not available; use the 'datetime' filter"
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use minijinja::{Environment, UndefinedBehavior};

    fn decimal_value(amount: Decimal) -> Value {
        Value::from_object(DecimalObject::new(amount))
    }

    fn render_with(template: &str, amount: &Value, other: &Value) -> Result<String, Error> {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        env.add_template_owned("t.txt", template.to_owned())?;
        let template = env.get_template("t.txt")?;
        template.render(minijinja::context! {
            a => amount.clone(),
            b => other.clone(),
        })
    }

    #[test]
    fn decimal_renders_exactly() {
        let value = decimal_value(Decimal::new(123_450, 2));
        assert_eq!(value.to_string(), "1234.50");
    }

    #[test]
    fn decimal_is_true_only_when_non_zero() {
        assert!(!decimal_value(Decimal::ZERO).is_true());
        assert!(decimal_value(Decimal::new(1, 2)).is_true());
    }

    #[test]
    fn decimal_method_calls_are_rejected() {
        let error = render_with(
            "{{ a.round() }}",
            &decimal_value(Decimal::ONE),
            &Value::from(0),
        );
        match error {
            Err(error) => assert!(error.to_string().contains("read-only"), "{error:#}"),
            Ok(output) => panic!("method call rendered: {output}"),
        }
    }

    #[test]
    fn decimal_addition_is_rejected() {
        let error = render_with("{{ a + b }}", &decimal_value(Decimal::ONE), &Value::from(1));
        match error {
            Err(error) => {
                assert!(
                    matches!(error.kind(), ErrorKind::InvalidOperation),
                    "{error:#}"
                );
            }
            Ok(output) => panic!("addition rendered: {output}"),
        }
    }

    #[test]
    fn decimal_custom_cmp_orders_numerically() {
        // Textual ordering would say "9.99" > "10.00"; numeric does not.
        let rendered = render_with(
            "{% if a > b %}A{% else %}B{% endif %}",
            &decimal_value(Decimal::new(999, 2)),
            &decimal_value(Decimal::new(1000, 2)),
        );
        match rendered {
            Ok(order) => assert_eq!(order, "B"),
            Err(error) => panic!("comparison failed: {error:#}"),
        }
    }

    #[test]
    fn date_renders_iso_8601() {
        let date = match time::Date::from_calendar_date(2025, time::Month::January, 15) {
            Ok(date) => date,
            Err(error) => panic!("bad date: {error}"),
        };
        let value = Value::from_object(DateObject::new(date));
        assert_eq!(value.to_string(), "2025-01-15");
    }

    #[test]
    fn datetime_renders_iso_8601() {
        let date = match time::Date::from_calendar_date(2025, time::Month::January, 15) {
            Ok(date) => date,
            Err(error) => panic!("bad date: {error}"),
        };
        let datetime = date
            .with_hms(9, 30, 0)
            .unwrap_or_else(|e| panic!("{e}"))
            .assume_utc();
        let value = Value::from_object(DateTimeObject::new(datetime));
        let text = value.to_string();
        assert!(text.contains("2025-01-15"), "{text}");
        assert!(text.contains("9:30"), "{text}");
    }
}
