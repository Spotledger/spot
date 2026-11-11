//! The template filters registered on the engine environment.

use minijinja::value::Kwargs;
use minijinja::{Environment, Error, ErrorKind, Value};

use crate::error::TemplateError;
use crate::format::words::NumberSystem;
use crate::format::{self, LocaleInfo};

/// Wraps a typed [`TemplateError`] as a MiniJinja filter failure.
///
/// Filter failures travel as `ErrorKind::InvalidOperation` (the kind
/// MiniJinja's own filters use) with a `spot-template: ` detail prefix; the
/// engine's mapping turns prefixed errors into [`TemplateError::Filter`]
/// while unprefixed arithmetic failures become
/// [`TemplateError::NotComputable`].
fn engine_error(error: TemplateError) -> Error {
    Error::new(
        ErrorKind::InvalidOperation,
        format!("spot-template: {error}"),
    )
}

/// Reads the optional `locale` keyword argument.
fn read_locale(kwargs: &Kwargs) -> Result<&'static LocaleInfo, Error> {
    let requested: Option<String> = kwargs.get("locale")?;
    let id = requested.unwrap_or_else(|| format::DEFAULT_LOCALE.to_owned());
    format::locale(&id).map_err(engine_error)
}

/// Registers all platform filters on an environment.
pub(crate) fn register(env: &mut Environment<'static>) {
    env.add_filter("money", money_filter);
    env.add_filter("number", number_filter);
    env.add_filter("amount_in_words", words_filter);
    env.add_filter("date", date_filter);
    env.add_filter("datetime", datetime_filter);
}

fn money_filter(value: Value, currency_code: String, kwargs: Kwargs) -> Result<Value, Error> {
    let locale_info = read_locale(&kwargs)?;
    kwargs.assert_all_used()?;
    let currency_info = format::currency(&currency_code).map_err(engine_error)?;
    let text = format::money::money(&value, currency_info, locale_info).map_err(engine_error)?;
    Ok(Value::from(text))
}

fn number_filter(value: Value, decimals: i64, kwargs: Kwargs) -> Result<Value, Error> {
    let locale_info = read_locale(&kwargs)?;
    kwargs.assert_all_used()?;
    let decimals = match u32::try_from(decimals) {
        Ok(decimals) => decimals,
        Err(_) => {
            return Err(engine_error(TemplateError::InvalidInput {
                message: "number expects a non-negative decimals count".to_owned(),
            }));
        }
    };
    let text = format::money::number(&value, decimals, locale_info).map_err(engine_error)?;
    Ok(Value::from(text))
}

fn words_filter(value: Value, currency_code: String, kwargs: Kwargs) -> Result<Value, Error> {
    let language: Option<String> = kwargs.get("lang")?;
    let system_name: Option<String> = kwargs.get("system")?;
    kwargs.assert_all_used()?;
    let language = language.unwrap_or_else(|| "en".to_owned());
    let system = match system_name.as_deref().unwrap_or("international") {
        "international" => NumberSystem::International,
        "south_asian" | "south-asian" => NumberSystem::SouthAsian,
        other => {
            return Err(engine_error(TemplateError::InvalidInput {
                message: format!(
                    "unknown numbering system '{other}'; use 'international' or 'south-asian'"
                ),
            }));
        }
    };
    let currency_info = format::currency(&currency_code).map_err(engine_error)?;
    let text = crate::format::words::amount_in_words(&value, currency_info, &language, system)
        .map_err(engine_error)?;
    Ok(Value::from(text))
}

fn date_filter(value: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let format_name: Option<String> = kwargs.get("format")?;
    read_locale(&kwargs)?;
    kwargs.assert_all_used()?;
    let name = format_name.unwrap_or_else(|| "medium".to_owned());
    let text = format::dates::date(&value, &name).map_err(engine_error)?;
    Ok(Value::from(text))
}

fn datetime_filter(value: Value, kwargs: Kwargs) -> Result<Value, Error> {
    let format_name: Option<String> = kwargs.get("format")?;
    let tz: Option<String> = kwargs.get("tz")?;
    read_locale(&kwargs)?;
    kwargs.assert_all_used()?;
    let name = format_name.unwrap_or_else(|| "medium".to_owned());
    let text = format::dates::datetime(&value, &name, tz.as_deref()).map_err(engine_error)?;
    Ok(Value::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::{DateObject, DecimalObject};
    use minijinja::UndefinedBehavior;
    use rust_decimal::Decimal;

    fn env() -> Environment<'static> {
        let mut env = Environment::new();
        env.set_undefined_behavior(UndefinedBehavior::Strict);
        register(&mut env);
        env
    }

    fn render(template: &str, values: &[(&str, Value)]) -> Result<String, Error> {
        let mut env = env();
        env.add_template_owned("t.txt", template.to_owned())?;
        let template = env.get_template("t.txt")?;
        let mut context = std::collections::BTreeMap::new();
        for (key, value) in values {
            context.insert((*key).to_owned(), value.clone());
        }
        template.render(minijinja::Value::from(context))
    }

    fn decimal_value(text: &str) -> Value {
        Value::from_object(DecimalObject::new(
            text.parse::<Decimal>()
                .unwrap_or_else(|error| panic!("bad decimal: {error}")),
        ))
    }

    #[test]
    fn money_with_defaults_and_locale() {
        let rendered = render(
            "{{ amount | money('PKR') }}",
            &[("amount", decimal_value("1234.56"))],
        );
        match rendered {
            Ok(text) => assert_eq!(text, "Rs 1,234.56"),
            Err(error) => panic!("{error:#}"),
        }
        let rendered = render(
            "{{ amount | money('INR', locale='en-IN') }}",
            &[("amount", decimal_value("1234567.89"))],
        );
        match rendered {
            Ok(text) => assert_eq!(text, "₹12,34,567.89"),
            Err(error) => panic!("{error:#}"),
        }
    }

    #[test]
    fn south_asian_system_kwarg_uses_brief_spelling() {
        match render(
            "{{ amount | amount_in_words('PKR', system='south_asian') }}",
            &[("amount", decimal_value("125000"))],
        ) {
            Ok(text) => assert_eq!(text, "One Lakh Twenty-Five Thousand Rupees Only"),
            Err(error) => panic!("{error:#}"),
        }
    }

    #[test]
    fn words_filter_matches_brief_example() {
        let rendered = render(
            "{{ amount | amount_in_words('PKR') }}",
            &[("amount", decimal_value("123456.78"))],
        );
        match rendered {
            Ok(text) => assert_eq!(
                text,
                "One Hundred Twenty-Three Thousand Four Hundred Fifty-Six Rupees and Seventy-Eight Paisa Only"
            ),
            Err(error) => panic!("{error:#}"),
        }
    }

    #[test]
    fn filter_errors_name_the_problem() {
        for (template, needle) in [
            ("{{ amount | money('XYZ') }}", "unknown currency 'XYZ'"),
            (
                "{{ amount | money('PKR', locale='xx') }}",
                "unknown locale 'xx'",
            ),
            (
                "{{ amount | amount_in_words('PKR', system='roman') }}",
                "numbering system",
            ),
        ] {
            match render(template, &[("amount", decimal_value("1"))]) {
                Err(error) => assert!(error.to_string().contains(needle), "{error:#}"),
                Ok(output) => panic!("'{template}' rendered: {output}"),
            }
        }
    }

    #[test]
    fn unused_kwargs_are_rejected() {
        match render(
            "{{ amount | money('PKR', locale='en', typo='1') }}",
            &[("amount", decimal_value("1"))],
        ) {
            Err(error) => {
                let text = error.to_string();
                assert!(text.contains("typo"), "{text}");
            }
            Ok(output) => panic!("rendered: {output}"),
        }
    }

    #[test]
    fn date_and_datetime_filters() {
        let date = match time::Date::from_calendar_date(2025, time::Month::January, 15) {
            Ok(date) => date,
            Err(error) => panic!("bad date: {error}"),
        };
        let rendered = render(
            "{{ when | date(format='short') }}",
            &[("when", Value::from_object(DateObject::new(date)))],
        );
        match rendered {
            Ok(text) => assert_eq!(text, "15/01/2025"),
            Err(error) => panic!("{error:#}"),
        }
        let datetime = date
            .with_hms(9, 30, 0)
            .unwrap_or_else(|e| panic!("{e}"))
            .assume_utc();
        let rendered = render(
            "{{ when | datetime(format='medium', tz='+05:00') }}",
            &[(
                "when",
                Value::from_object(crate::value::DateTimeObject::new(datetime)),
            )],
        );
        match rendered {
            Ok(text) => assert_eq!(text, "Jan 15, 2025 14:30"),
            Err(error) => panic!("{error:#}"),
        }
    }
}
