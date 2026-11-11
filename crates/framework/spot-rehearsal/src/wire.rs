//! The wire form of an `i128` measure: a JSON string of decimal digits.
//!
//! serde cannot buffer an `i128`. When a value sits inside an internally tagged enum (for example
//! `#[serde(tag = "record")]` on a journal record) serialising works and reading back fails, so a
//! journal holding a fingerprint could not be read. These helpers (used with
//! `#[serde(with = "...")]`) write every `i128` as a string such as `"-12345678901234567890123"`,
//! always, whatever its size. They read that string and also a JSON integer that fits `i64` or
//! `u64` (data written by the earlier form, hand-written specs). Anything else is refused with an
//! error that never carries the value.

use std::collections::BTreeMap;
use std::fmt;

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serializer};

const EXPECTED: &str = "a decimal integer string or a JSON integer";

/// Parse the canonical decimal form: optional `-`, digits, no leading zero except `0` itself,
/// no `-0`, no sign `+`, no whitespace. `None` for everything else, including overflow.
fn parse(text: &str) -> Option<i128> {
    let digits = text.strip_prefix('-').unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if digits.len() > 1 && digits.starts_with('0') {
        return None;
    }
    if digits == "0" && text.starts_with('-') {
        return None;
    }
    text.parse::<i128>().ok()
}

struct I128Visitor;

impl Visitor<'_> for I128Visitor {
    type Value = i128;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(EXPECTED)
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<i128, E> {
        Ok(i128::from(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<i128, E> {
        Ok(i128::from(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<i128, E> {
        parse(v).ok_or_else(|| E::custom("invalid measure"))
    }
}

/// A wrapper that reads one measure; used for the map values.
struct Measure(i128);

impl<'de> Deserialize<'de> for Measure {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Measure, D::Error> {
        deserializer.deserialize_any(I128Visitor).map(Measure)
    }
}

/// Write an `i128` as a decimal string.
pub fn serialize<S: Serializer>(value: &i128, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

/// Read an `i128` from a decimal string or a JSON integer that fits `i64`/`u64`.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<i128, D::Error> {
    deserializer.deserialize_any(I128Visitor)
}

/// The same for a map of named measures.
pub mod map {
    use super::*;

    /// Write each value as a decimal string.
    pub fn serialize<S: Serializer>(
        value: &BTreeMap<String, i128>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_map(value.iter().map(|(k, v)| (k, v.to_string())))
    }

    struct MapVisitor;

    impl<'de> Visitor<'de> for MapVisitor {
        type Value = BTreeMap<String, i128>;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("a map of measure names to decimal integers")
        }

        fn visit_map<A: MapAccess<'de>>(self, mut access: A) -> Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some((name, Measure(value))) = access.next_entry::<String, Measure>()? {
                if out.insert(name, value).is_some() {
                    return Err(de::Error::custom("duplicate measure name"));
                }
            }
            Ok(out)
        }
    }

    /// Read each value from a decimal string or a JSON integer that fits `i64`/`u64`.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<String, i128>, D::Error> {
        deserializer.deserialize_map(MapVisitor)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use crate::fingerprint::{
        Dimension, Fingerprint, FingerprintDiff, FingerprintRow, FingerprintSection,
    };
    use crate::report::{ClassifiedDiff, DiffClass};
    use serde::{Deserialize, Serialize};

    fn values() -> Vec<i128> {
        vec![
            0,
            -1,
            i128::from(i64::MAX) + 1,
            i128::from(u64::MAX) + 1,
            i128::MAX,
            i128::MIN,
        ]
    }

    fn row(v: i128) -> FingerprintRow {
        FingerprintRow::new(
            vec![Dimension::new("account", "cash")],
            [
                ("debit".to_owned(), v),
                ("credit".to_owned(), v.wrapping_neg()),
            ]
            .into_iter()
            .collect(),
        )
    }

    #[test]
    fn row_diff_and_classified_diff_round_trip() {
        for v in values() {
            let r = row(v);
            let json = serde_json::to_string(&r).unwrap();
            assert!(json.contains(&format!(r#""debit":"{v}""#)), "{json}");
            assert_eq!(serde_json::from_str::<FingerprintRow>(&json).unwrap(), r);

            let d = FingerprintDiff {
                section: "s".into(),
                dimensions: vec![],
                measure: "m".into(),
                before: v,
                after: 0,
            };
            let json = serde_json::to_string(&d).unwrap();
            assert!(json.contains(&format!(r#""before":"{v}""#)), "{json}");
            assert_eq!(serde_json::from_str::<FingerprintDiff>(&json).unwrap(), d);

            let c = ClassifiedDiff {
                section: "s".into(),
                dimensions: vec![],
                measure: "m".into(),
                before: v,
                after: 0,
                delta: v.wrapping_neg(),
                class: DiffClass::Unexplained,
                explained_by: None,
            };
            let json = serde_json::to_string(&c).unwrap();
            assert!(json.contains(r#""delta":""#), "{json}");
            assert_eq!(serde_json::from_str::<ClassifiedDiff>(&json).unwrap(), c);
        }
    }

    // With the old form (a JSON integer) the next test failed when reading back: serde buffers the
    // content of an internally tagged enum and cannot hold an `i128` above `u64::MAX`.
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    #[serde(tag = "record", rename_all = "snake_case")]
    enum Record {
        Captured { fingerprint: Fingerprint },
        Other { n: u32 },
    }

    #[test]
    fn a_fingerprint_survives_an_internally_tagged_enum() {
        for v in values() {
            let fingerprint = Fingerprint::new(vec![FingerprintSection::new(
                "tb",
                "Trial balance",
                vec!["account".into()],
                vec!["debit".into(), "credit".into()],
                vec![row(v)],
            )]);
            let rec = Record::Captured { fingerprint };
            let json = serde_json::to_string(&rec).unwrap();
            assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), rec);
            let value: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::from_value::<Record>(value).unwrap(), rec);
        }
        let other = Record::Other { n: 1 };
        let json = serde_json::to_string(&other).unwrap();
        assert_eq!(serde_json::from_str::<Record>(&json).unwrap(), other);
    }

    #[derive(Debug, Deserialize)]
    struct One(#[serde(with = "super")] i128);

    fn read(text: &str) -> Result<i128, serde_json::Error> {
        serde_json::from_str::<One>(text).map(|o| o.0)
    }

    #[test]
    fn accepted_inputs() {
        assert_eq!(read(r#""0""#).unwrap(), 0);
        assert_eq!(read(r#""-1""#).unwrap(), -1);
        assert_eq!(
            read(r#""12345678901234567890123""#).unwrap(),
            12_345_678_901_234_567_890_123
        );
        assert_eq!(read(&format!(r#""{}""#, i128::MAX)).unwrap(), i128::MAX);
        assert_eq!(read(&format!(r#""{}""#, i128::MIN)).unwrap(), i128::MIN);
        assert_eq!(read("5").unwrap(), 5);
        assert_eq!(read("-5").unwrap(), -5);
        assert_eq!(read("0").unwrap(), 0);
        assert_eq!(read(&u64::MAX.to_string()).unwrap(), i128::from(u64::MAX));
        assert_eq!(read(&i64::MIN.to_string()).unwrap(), i128::from(i64::MIN));
    }

    #[test]
    fn rejected_inputs_carry_no_value() {
        let too_big = format!(r#""{}0""#, i128::MAX);
        let too_small = format!(r#""{}0""#, i128::MIN);
        for bad in [
            r#""""#,
            r#""+5""#,
            r#"" 5""#,
            r#""5 ""#,
            r#""1e3""#,
            r#""1.0""#,
            r#""007""#,
            r#""-0""#,
            r#""-""#,
            r#""0x10""#,
            r#""５""#,
            r#""1_000""#,
            "1.5",
            "1e3",
            "true",
            "null",
            "[]",
            "{}",
            "18446744073709551616",
            "-9223372036854775809",
            too_big.as_str(),
            too_small.as_str(),
        ] {
            let err = read(bad).expect_err(bad).to_string();
            for digits in ["5", "007", "18446744073709551616"] {
                if bad.contains(digits) && digits.len() > 3 {
                    assert!(!err.contains(digits), "{err}");
                }
            }
            assert!(!err.contains(&i128::MAX.to_string()), "{err}");
        }
    }

    #[test]
    fn the_old_integer_form_still_reads() {
        let row: FingerprintRow =
            serde_json::from_str(r#"{"dimensions":[],"measures":{"a":5,"b":-7}}"#).unwrap();
        assert_eq!(row.measure("a"), 5);
        assert_eq!(row.measure("b"), -7);
        let d: FingerprintDiff = serde_json::from_str(
            r#"{"section":"s","dimensions":[],"measure":"m","before":1,"after":2}"#,
        )
        .unwrap();
        assert_eq!((d.before, d.after), (1, 2));
    }

    #[test]
    fn a_bad_or_duplicate_measure_is_refused() {
        assert!(
            serde_json::from_str::<FingerprintRow>(r#"{"dimensions":[],"measures":{"a":1.5}}"#)
                .is_err()
        );
        assert!(
            serde_json::from_str::<FingerprintRow>(
                r#"{"dimensions":[],"measures":{"a":"1","a":"2"}}"#
            )
            .is_err()
        );
    }
}
