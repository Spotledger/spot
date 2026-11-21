//! The public API: types, errors, serialization and rule sets.
#![allow(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use rust_decimal::Decimal;
use spot_money::{
    Currency, CurrencyTable, Kind, Mode, Money, MoneyError, Quantity, RoundingRule, RuleSet,
    Source, Uom, allocate, mul, mul_exact, parse_decimal, percent, percent_rounded, sum_decimals,
};

fn d(s: &str) -> Decimal {
    parse_decimal(s).unwrap()
}

fn cur(s: &str) -> Currency {
    Currency::new(s).unwrap()
}

fn rule(scale: u32, mode: Mode) -> RoundingRule {
    RoundingRule::at_scale(scale, mode).unwrap()
}

#[test]
fn currency_codes_are_validated() {
    assert_eq!(cur("USD").code(), "USD");
    for bad in ["", "US", "USDX", "usd", "U$D", "EU1", " USD"] {
        assert_eq!(
            Currency::new(bad).unwrap_err().code(),
            "money.bad_currency",
            "{bad:?}"
        );
    }
}

#[test]
fn units_of_measure_are_validated() {
    assert_eq!(Uom::new("kg").unwrap().as_str(), "kg");
    for bad in ["", "two words", "tab\t", &"x".repeat(33)] {
        assert_eq!(
            Uom::new(bad).unwrap_err().code(),
            "money.bad_uom",
            "{bad:?}"
        );
    }
}

#[test]
fn the_currency_table_is_data() {
    let table = CurrencyTable::new([(cur("USD"), 2), (cur("JPY"), 0), (cur("KWD"), 3)]).unwrap();
    assert_eq!(table.exponent(&cur("JPY")).unwrap(), 0);
    assert_eq!(table.exponent(&cur("KWD")).unwrap(), 3);
    assert_eq!(
        table.exponent(&cur("EUR")).unwrap_err().code(),
        "money.unknown_currency"
    );
    assert!(table.contains(&cur("USD")) && !table.contains(&cur("EUR")));
    assert_eq!(
        table.currencies().map(Currency::code).collect::<Vec<_>>(),
        ["JPY", "KWD", "USD"]
    );
    assert_eq!(
        CurrencyTable::new([(cur("USD"), 2), (cur("USD"), 3)])
            .unwrap_err()
            .code(),
        "money.duplicate_currency"
    );
    assert_eq!(
        CurrencyTable::new([(cur("USD"), 29)]).unwrap_err().code(),
        "money.bad_scale"
    );
    // There is no built-in list: an empty table knows nothing.
    assert!(CurrencyTable::default().exponent(&cur("USD")).is_err());
}

#[test]
fn money_arithmetic_is_exact_and_typed() {
    let a = Money::new(d("0.1"), cur("USD"));
    let b = Money::new(d("0.2"), cur("USD"));
    assert_eq!(a.checked_add(&b).unwrap().value(), d("0.3"));
    assert_eq!(b.checked_sub(&a).unwrap().value(), d("0.1"));
    assert_eq!(a.neg().value(), d("-0.1"));
    assert_eq!(a.to_string(), "0.1 USD");
    let eur = Money::new(d("1"), cur("EUR"));
    let err = a.checked_add(&eur).unwrap_err();
    assert_eq!(err.code(), "money.currency_mismatch");
    assert!(
        err.to_string().contains("USD") && err.to_string().contains("EUR"),
        "{err}"
    );
    assert_eq!(
        a.checked_sub(&eur).unwrap_err().code(),
        "money.currency_mismatch"
    );
    // Overflow is an error, never a wrap or a saturation.
    let max = Money::new(Decimal::MAX, cur("USD"));
    assert_eq!(
        max.checked_add(&Money::new(d("1"), cur("USD")))
            .unwrap_err()
            .code(),
        "money.overflow"
    );
    assert_eq!(
        max.neg()
            .checked_sub(&Money::new(d("1"), cur("USD")))
            .unwrap_err()
            .code(),
        "money.overflow"
    );
}

#[test]
fn addition_never_rounds_silently() {
    // The decimal type's own addition drops digits when a result needs more than 28 places or 96
    // bits; ours reports it.
    let tiny = Money::new(d("0.0000000000000000000000000001"), cur("USD"));
    let big = Money::new(d("7922816251426433759354395033"), cur("USD")); // 28 digits
    let sum = big.checked_add(&tiny);
    match sum {
        Ok(m) => assert_eq!(m.checked_sub(&big).unwrap().value(), tiny.value()),
        Err(e) => assert!(["money.overflow", "money.inexact"].contains(&e.code())),
    }
}

#[test]
fn money_sums_check_the_currency_and_ignore_order() {
    let items = [
        Money::new(d("1.10"), cur("USD")),
        Money::new(d("2.20"), cur("USD")),
        Money::new(d("-0.05"), cur("USD")),
    ];
    let total = Money::sum(&cur("USD"), &items).unwrap();
    assert_eq!(total.value(), d("3.25"));
    assert_eq!(
        total.value().scale(),
        2,
        "a sum of two-digit amounts keeps its two digits"
    );
    assert_eq!(Money::sum(&cur("USD"), &[]).unwrap().value(), Decimal::ZERO);
    let mixed = [items[0], Money::new(d("1"), cur("EUR"))];
    assert_eq!(
        Money::sum(&cur("USD"), &mixed).unwrap_err().code(),
        "money.currency_mismatch"
    );
    // Overflow in the middle cannot depend on order: the same multiset fails either way.
    let max = Decimal::MAX;
    for order in [
        [max, d("1"), -max],
        [-max, max, d("1")],
        [d("1"), -max, max],
    ] {
        assert_eq!(sum_decimals(order).unwrap(), d("1"), "{order:?}");
    }
    assert_eq!(
        sum_decimals([max, d("1")]).unwrap_err().code(),
        "money.overflow"
    );
    assert_eq!(
        sum_decimals([d("1"), max]).unwrap_err().code(),
        "money.overflow"
    );
}

#[test]
fn quantities_do_not_mix_units() {
    let kg = Uom::new("kg").unwrap();
    let lb = Uom::new("lb").unwrap();
    let a = Quantity::new(d("1.5"), Some(kg.clone()));
    let b = Quantity::new(d("2.5"), Some(kg.clone()));
    assert_eq!(a.checked_add(&b).unwrap().value(), d("4.0"));
    assert_eq!(b.checked_sub(&a).unwrap().value(), d("1.0"));
    let err = a.checked_add(&Quantity::new(d("1"), Some(lb))).unwrap_err();
    assert_eq!(err.code(), "money.uom_mismatch");
    assert!(
        err.to_string().contains("kg") && err.to_string().contains("lb"),
        "{err}"
    );
    // A bare number is not a quantity of kilograms.
    assert_eq!(
        a.checked_add(&Quantity::new(d("1"), None))
            .unwrap_err()
            .code(),
        "money.uom_mismatch"
    );
    assert_eq!(
        Quantity::sum(Some(&kg), &[a.clone(), b]).unwrap().value(),
        d("4.0")
    );
    assert_eq!(a.to_string(), "1.5 kg");
    assert_eq!(a.rounded(&rule(0, Mode::HalfUp)).unwrap().value(), d("2"));
}

#[test]
fn multiplication_is_exact_then_rounded_by_the_callers_rule() {
    let price = Money::new(d("12.50"), cur("USD"));
    let qty = Quantity::new(d("3.333"), Some(Uom::new("m").unwrap()));
    assert_eq!(mul_exact(&price, &qty).unwrap().value(), d("41.66250"));
    assert_eq!(
        mul(&price, &qty, &rule(2, Mode::HalfUp))
            .unwrap()
            .value()
            .to_string(),
        "41.66"
    );
    assert_eq!(
        mul(&price, &qty, &rule(2, Mode::Up))
            .unwrap()
            .value()
            .to_string(),
        "41.67"
    );
    assert_eq!(
        mul(&price, &qty, &rule(2, Mode::HalfUp))
            .unwrap()
            .currency()
            .code(),
        "USD"
    );
    // More precision than a decimal holds is reported, not rounded away.
    let fine = Money::new(d("0.1234567890123456789"), cur("USD"));
    let err = mul_exact(&fine, &Quantity::new(d("0.1234567890123456789"), None)).unwrap_err();
    assert_eq!(err.code(), "money.inexact");
    let huge = Money::new(Decimal::MAX, cur("USD"));
    assert_eq!(
        mul_exact(&huge, &Quantity::new(d("2"), None))
            .unwrap_err()
            .code(),
        "money.overflow"
    );
}

#[test]
fn percentages_are_exact_and_rounded_on_request() {
    let amount = Money::new(d("99.99"), cur("USD"));
    assert_eq!(percent(&amount, d("7.5")).unwrap().value(), d("7.49925"));
    assert_eq!(
        percent_rounded(&amount, d("7.5"), &rule(2, Mode::HalfUp))
            .unwrap()
            .value()
            .to_string(),
        "7.50"
    );
}

#[test]
fn rounding_rules_validate_their_parts() {
    assert_eq!(
        RoundingRule::new(29, Mode::HalfUp, None)
            .unwrap_err()
            .code(),
        "money.bad_scale"
    );
    assert_eq!(
        RoundingRule::new(2, Mode::HalfUp, Some(d("0")))
            .unwrap_err()
            .code(),
        "money.bad_step"
    );
    assert_eq!(
        RoundingRule::new(2, Mode::HalfUp, Some(d("-0.05")))
            .unwrap_err()
            .code(),
        "money.bad_step"
    );
    assert_eq!(
        RoundingRule::new(1, Mode::HalfUp, Some(d("0.05")))
            .unwrap_err()
            .code(),
        "money.bad_step"
    );
    // Trailing zeros of the step do not count against the scale.
    assert!(RoundingRule::new(1, Mode::HalfUp, Some(d("0.50"))).is_ok());
    assert_eq!(rule(2, Mode::HalfUp).unit(), d("0.01"));
    assert_eq!(
        RoundingRule::new(2, Mode::HalfUp, Some(d("0.05")))
            .unwrap()
            .unit(),
        d("0.05")
    );
}

#[test]
fn rules_read_from_toml_and_json_and_refuse_binary_numbers() {
    #[derive(Debug, serde::Deserialize)]
    struct Holder {
        rule: RoundingRule,
    }
    let ok: Holder =
        toml::from_str("[rule]\nscale = 2\nmode = \"half_even\"\nstep = \"0.05\"\n").unwrap();
    assert_eq!(
        (ok.rule.scale(), ok.rule.mode(), ok.rule.step()),
        (2, Mode::HalfEven, Some(d("0.05")))
    );
    let whole: Holder = toml::from_str("[rule]\nscale = 0\nmode = \"down\"\nstep = 5\n").unwrap();
    assert_eq!(whole.rule.step(), Some(d("5")));
    // A TOML float is a binary number: refused, with the way to fix it.
    let err = toml::from_str::<Holder>("[rule]\nscale = 2\nmode = \"half_up\"\nstep = 0.05\n")
        .unwrap_err()
        .to_string();
    assert!(err.contains("string"), "{err}");
    for bad in [
        "[rule]\nscale = 2\nmode = \"banker\"\n",
        "[rule]\nscale = 2\n",
        "[rule]\nscale = 2\nmode = \"half_up\"\nextra = 1\n",
        "[rule]\nscale = 40\nmode = \"half_up\"\n",
    ] {
        assert!(toml::from_str::<Holder>(bad).is_err(), "{bad}");
    }
    let json = serde_json::to_string(&RoundingRule::new(2, Mode::HalfUp, Some(d("0.05"))).unwrap())
        .unwrap();
    assert_eq!(json, r#"{"scale":2,"mode":"half_up","step":"0.05"}"#);
    let back: RoundingRule = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back,
        RoundingRule::new(2, Mode::HalfUp, Some(d("0.05"))).unwrap()
    );
}

#[test]
fn money_and_quantity_serialize_as_strings() {
    let m = Money::new(d("12.50"), cur("USD"));
    let json = serde_json::to_string(&m).unwrap();
    assert_eq!(json, r#"{"value":"12.50","currency":"USD"}"#);
    assert_eq!(serde_json::from_str::<Money>(&json).unwrap(), m);
    assert!(serde_json::from_str::<Money>(r#"{"value":"1","currency":"usd"}"#).is_err());
    assert!(
        serde_json::from_str::<Money>(r#"{"value":12.5,"currency":"USD"}"#).is_err(),
        "binary numbers are refused"
    );
    let q = Quantity::new(d("3"), Some(Uom::new("kg").unwrap()));
    let json = serde_json::to_string(&q).unwrap();
    assert_eq!(json, r#"{"value":"3","uom":"kg"}"#);
    assert_eq!(serde_json::from_str::<Quantity>(&json).unwrap(), q);
    assert_eq!(
        serde_json::from_str::<Quantity>(r#"{"value":"3"}"#).unwrap(),
        Quantity::new(d("3"), None)
    );
}

#[test]
fn decimals_parse_exactly_or_not_at_all() {
    assert_eq!(parse_decimal("-0.05").unwrap().to_string(), "-0.05");
    assert_eq!(parse_decimal("+7").unwrap().to_string(), "7");
    for bad in [
        "",
        ".5",
        "5.",
        "1e3",
        "1,5",
        " 1",
        "1 ",
        "0x10",
        "--1",
        "NaN",
        "0.00000000000000000000000000001",
    ] {
        assert_eq!(
            parse_decimal(bad).unwrap_err().code(),
            "money.bad_decimal",
            "{bad:?}"
        );
    }
}

const RULES: &str = r#"
default = "money2"

[rules.money2]
scale = 2
mode = "half_up"

[rules.cash5]
scale = 2
mode = "half_up"
step = "0.05"

[rules.price4]
scale = 4
mode = "half_even"

[currency]
"*" = "money2"
CHF = "cash5"
JPY = "money0"

[rules.money0]
scale = 0
mode = "half_up"

[tax_rule]
"vat-reduced" = "price4"

[price]
"*" = "price4"
"#;

#[test]
fn rule_sets_resolve_by_the_documented_fallback_order() {
    let set = RuleSet::from_toml(RULES).unwrap();
    // 1. exact
    let chf = set.explain(Kind::Currency, "CHF").unwrap();
    assert_eq!(
        (chf.rule_name.as_str(), chf.source),
        ("cash5", Source::Exact)
    );
    assert_eq!(set.for_currency(&cur("JPY")).unwrap().scale(), 0);
    assert_eq!(set.lookup(Kind::TaxRule, "vat-reduced").unwrap().scale(), 4);
    // 2. the kind's `*`
    let usd = set.explain(Kind::Currency, "USD").unwrap();
    assert_eq!(
        (usd.rule_name.as_str(), usd.source),
        ("money2", Source::KindDefault)
    );
    assert_eq!(
        set.lookup(Kind::Price, "anything").unwrap().mode(),
        Mode::HalfEven
    );
    // 3. the top-level default
    let line = set.explain(Kind::LineTotal, "invoice").unwrap();
    assert_eq!(
        (line.rule_name.as_str(), line.source),
        ("money2", Source::GlobalDefault)
    );
    assert_eq!(set.lookup(Kind::TaxRule, "other").unwrap().scale(), 2);
    // The explanation says why, in words.
    assert!(
        chf.text.contains("currency `CHF` is bound to rule `cash5`"),
        "{}",
        chf.text
    );
    assert!(usd.text.contains("default `*`"), "{}", usd.text);
    assert!(line.text.contains("top-level default"), "{}", line.text);
    assert_eq!(set.rules().count(), 4);
}

#[test]
fn without_a_default_nothing_is_guessed() {
    let set =
        RuleSet::from_toml("[rules.a]\nscale = 2\nmode = \"up\"\n[price]\nbook = \"a\"\n").unwrap();
    assert!(set.lookup(Kind::Price, "book").is_ok());
    let err = set.lookup(Kind::Price, "pen").unwrap_err();
    assert_eq!(err.code(), "money.no_rule");
    assert!(err.to_string().contains("price `pen`"), "{err}");
    assert_eq!(
        RuleSet::new()
            .lookup(Kind::Currency, "USD")
            .unwrap_err()
            .code(),
        "money.no_rule"
    );
}

#[test]
fn rule_sets_can_be_built_in_code() {
    let set = RuleSet::new()
        .with_rule("r2", rule(2, Mode::HalfUp))
        .unwrap()
        .bind(Kind::Currency, "USD", "r2")
        .unwrap()
        .with_default("r2")
        .unwrap();
    assert_eq!(set.lookup(Kind::Currency, "USD").unwrap().scale(), 2);
    assert!(
        RuleSet::new()
            .bind(Kind::Currency, "USD", "missing")
            .is_err()
    );
    assert!(RuleSet::new().with_default("missing").is_err());
    assert!(
        RuleSet::new()
            .with_rule("bad name", rule(2, Mode::Up))
            .is_err()
    );
}

#[test]
fn a_bad_rule_file_reports_every_problem_at_once() {
    let bad = r#"
default = "nope"
surprise = 1

[rules.ok]
scale = 2
mode = "half_up"

[rules.bad_scale]
scale = 40
mode = "half_up"

[rules.bad_mode]
scale = 2
mode = "banker"

[rules.no_mode]
scale = 2

[rules.float_step]
scale = 2
mode = "up"
step = 0.05

[rules.step_too_fine]
scale = 1
mode = "up"
step = "0.05"

[rules.extra]
scale = 2
mode = "up"
colour = "red"

[currency]
usd = "ok"
USD = "ghost"
EUR = 7

[tax_rule]
"" = "ok"
"#;
    let err = RuleSet::from_toml(bad).unwrap_err();
    let codes = err.codes();
    for want in [
        "money.unknown_rule", // default = "nope", and USD = "ghost"
        "money.unknown_key",  // surprise, and colour
        "money.bad_scale",
        "money.bad_mode",
        "money.missing_field",
        "money.bad_type", // the float step, and EUR = 7
        "money.bad_step",
        "money.bad_currency", // usd
        "money.bad_key",      // the empty tax_rule key
    ] {
        assert!(codes.contains(&want), "missing {want} in {codes:?}\n{err}");
    }
    assert!(err.problems.len() >= 11, "{err}");
    // Each problem says where.
    let paths: Vec<&str> = err.problems.iter().map(|p| p.path.as_str()).collect();
    for want in [
        "default",
        "surprise",
        "rules.bad_scale.scale",
        "rules.bad_mode.mode",
        "rules.no_mode.mode",
        "rules.float_step.step",
        "rules.step_too_fine.step",
        "rules.extra.colour",
        "currency.usd",
        "currency.USD",
        "currency.EUR",
    ] {
        assert!(paths.contains(&want), "no problem at {want} in {paths:?}");
    }
    assert!(
        err.to_string()
            .starts_with(&format!("{} problem(s)", err.problems.len()))
    );
    // A file that is not TOML at all is one problem.
    let syntax = RuleSet::from_toml("this is = = not toml").unwrap_err();
    assert_eq!(syntax.codes(), ["money.syntax"]);
}

#[test]
fn allocation_follows_its_documented_rules() {
    let total = |s: &str| Money::new(d(s), cur("USD"));
    let r = rule(2, Mode::HalfUp);
    let texts = |parts: Vec<Money>| {
        parts
            .iter()
            .map(|p| p.value().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        texts(allocate(&total("100.00"), &[d("1"), d("1"), d("1")], &r).unwrap()),
        ["33.34", "33.33", "33.33"]
    );
    assert_eq!(
        allocate(&total("10.005"), &[d("1")], &r)
            .unwrap_err()
            .code(),
        "money.off_grid"
    );
    assert!(
        allocate(&total("10.005"), &[d("1")], &r)
            .unwrap_err()
            .to_string()
            .contains("USD")
    );
    assert_eq!(
        allocate(&total("1.00"), &[], &r).unwrap_err().code(),
        "money.bad_weights"
    );
    assert_eq!(
        allocate(&total("1.00"), &[d("1"), d("-1")], &r)
            .unwrap_err()
            .code(),
        "money.bad_weights"
    );
    // Negative weights apportion with sign: 100 split 3:-1 is 150 and -50.
    assert_eq!(
        texts(allocate(&total("100.00"), &[d("3"), d("-1")], &r).unwrap()),
        ["150.00", "-50.00"]
    );
    // The result keeps the currency.
    assert_eq!(
        allocate(&total("1.00"), &[d("1")], &r).unwrap()[0]
            .currency()
            .code(),
        "USD"
    );
    // Weights of any scale: only ratios matter.
    assert_eq!(
        texts(allocate(&total("1.00"), &[d("0.5"), d("0.25"), d("0.25")], &r).unwrap()),
        ["0.50", "0.25", "0.25"]
    );
}

#[test]
fn error_codes_are_stable_and_prefix_the_message() {
    let errors = [
        MoneyError::CurrencyMismatch {
            left: "USD".into(),
            right: "EUR".into(),
        },
        MoneyError::UomMismatch {
            left: "kg".into(),
            right: "lb".into(),
        },
        MoneyError::Overflow {
            operation: "addition",
        },
        MoneyError::Inexact {
            operation: "multiplication",
        },
        MoneyError::BadCurrency("x".into()),
        MoneyError::UnknownCurrency("XXX".into()),
        MoneyError::DuplicateCurrency("USD".into()),
        MoneyError::BadUom("".into()),
        MoneyError::BadScale(30),
        MoneyError::BadStep("x".into()),
        MoneyError::BadDecimal("x".into()),
        MoneyError::OffGrid("1.005 USD".into()),
        MoneyError::BadWeights("x"),
        MoneyError::BadRate("x"),
        MoneyError::NoRule {
            kind: "price".into(),
            key: "pen".into(),
        },
    ];
    let mut seen = std::collections::BTreeSet::new();
    for e in &errors {
        assert!(e.to_string().starts_with(&format!("{}: ", e.code())), "{e}");
        assert!(e.code().starts_with("money."));
        assert!(seen.insert(e.code()), "duplicate code {}", e.code());
    }
}
