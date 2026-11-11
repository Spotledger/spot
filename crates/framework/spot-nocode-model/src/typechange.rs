//! Type-change safety: before a field's type changes, the platform says whether the change is
//! **safe**, **lossy** (some existing values change) or **refused** (the data cannot be carried
//! over at all), and what happens to the values already stored.
//!
//! The classification is pure and takes the sample statistics the caller gathered (the crate
//! never reads a database or a clock). A field with no values is always safe to retype; a
//! structural change (a column becoming a child table, an attachment or a secret, or the
//! reverse) is refused whatever the statistics say, because there is no column-to-column
//! conversion for it.

use spot_meta::FieldType;

/// The longest text a plain `text` field holds when the change does not say otherwise.
pub const DEFAULT_TEXT_MAX: u32 = 255;

/// What the existing values look like, gathered by the caller. `rows` is the row count,
/// `non_null` how many of them hold a value; a caller that knows nothing passes
/// [`SampleStats::default`] (no rows), which makes every convertible change safe.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SampleStats {
    /// How many rows the table holds.
    #[serde(default)]
    pub rows: u64,
    /// How many rows hold a value in this field.
    #[serde(default)]
    pub non_null: u64,
    /// The longest text value, when the caller measured it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_text_len: Option<u32>,
    /// Whether any value has a fractional part.
    #[serde(default)]
    pub fractional: bool,
    /// How many values do not parse as the target type (for a text-to-typed change).
    #[serde(default)]
    pub unparsable: u64,
}

impl SampleStats {
    /// Statistics for a field with no rows.
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// Whether the field holds no values at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.non_null == 0
    }
}

/// How a type change treats the existing values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    /// Every existing value carries over unchanged.
    Safe,
    /// Some existing values change (are cut, rounded or reinterpreted); the designer is warned.
    Lossy,
    /// The change cannot be made without losing or corrupting data; it is refused.
    Refused,
}

/// The classification of one type change, with a plain-language summary and the detail a
/// designer reads before deciding.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TypeChangePlan {
    /// The current type.
    pub from: FieldType,
    /// The proposed type.
    pub to: FieldType,
    /// How the change treats the existing values.
    pub classification: Classification,
    /// One sentence stating what happens to the existing data.
    pub summary: String,
    /// The detail lines (what is cut, rounded or refused, and why).
    pub detail: Vec<String>,
}

/// Whether a type has no column-to-column conversion (a child table, an attachment or a
/// secret, which stores no value in the document).
fn structural(ty: FieldType) -> bool {
    matches!(
        ty,
        FieldType::Table | FieldType::TableMultiselect | FieldType::Attach | FieldType::Secret
    )
}

/// Classifies a change from `from` to `to` against `stats`.
#[must_use]
pub fn classify(from: FieldType, to: FieldType, stats: &SampleStats) -> TypeChangePlan {
    let plan = |classification, summary: &str, detail: Vec<String>| TypeChangePlan {
        from,
        to,
        classification,
        summary: summary.to_owned(),
        detail,
    };
    if from == to {
        return plan(Classification::Safe, "the type does not change", Vec::new());
    }
    if structural(from) || structural(to) {
        return plan(
            Classification::Refused,
            "a child table, attachment or secret has no column to convert",
            vec![format!(
                "a {from:?} and a {to:?} field are not interchangeable"
            )],
        );
    }
    if stats.is_empty() {
        return plan(
            Classification::Safe,
            "there are no existing values to convert",
            Vec::new(),
        );
    }
    match (from, to) {
        (FieldType::Text, FieldType::LongText) | (FieldType::LongText, FieldType::Text) => {
            if from == FieldType::LongText && stats.max_text_len.unwrap_or(0) > DEFAULT_TEXT_MAX {
                plan(
                    Classification::Lossy,
                    "values longer than the text limit are cut",
                    vec![format!(
                        "the longest value is {} characters; a text field holds {DEFAULT_TEXT_MAX}",
                        stats.max_text_len.unwrap_or(0)
                    )],
                )
            } else {
                plan(
                    Classification::Safe,
                    "every existing value fits the new type",
                    Vec::new(),
                )
            }
        }
        (FieldType::Select, FieldType::Text) | (FieldType::Select, FieldType::LongText) => plan(
            Classification::Safe,
            "the stored values stay as they are; they are no longer checked against a list",
            Vec::new(),
        ),
        (FieldType::Text | FieldType::LongText, FieldType::Select) => plan(
            Classification::Refused,
            "a value that is not in the new option list would be stranded",
            vec!["map the existing values to the new options first".to_owned()],
        ),
        (FieldType::Int, FieldType::Decimal) => plan(
            Classification::Safe,
            "every whole number is a decimal",
            Vec::new(),
        ),
        (FieldType::Decimal, FieldType::Int) => {
            if stats.fractional {
                plan(
                    Classification::Lossy,
                    "values with a fractional part are rounded",
                    vec!["the fraction after the decimal point is dropped".to_owned()],
                )
            } else {
                plan(
                    Classification::Safe,
                    "every value is a whole number",
                    Vec::new(),
                )
            }
        }
        (FieldType::Int | FieldType::Decimal, FieldType::Text) => plan(
            Classification::Safe,
            "every number is written as text",
            Vec::new(),
        ),
        (FieldType::Text | FieldType::LongText, FieldType::Int | FieldType::Decimal) => {
            if stats.unparsable > 0 {
                plan(
                    Classification::Refused,
                    "some values are not numbers",
                    vec![format!("{} values do not parse", stats.unparsable)],
                )
            } else {
                plan(
                    Classification::Safe,
                    "every value parses as a number",
                    Vec::new(),
                )
            }
        }
        (
            FieldType::Text | FieldType::LongText,
            FieldType::Date | FieldType::Time | FieldType::DateTime,
        ) => {
            if stats.unparsable > 0 {
                plan(
                    Classification::Refused,
                    "some values are not dates or times",
                    vec![format!("{} values do not parse", stats.unparsable)],
                )
            } else {
                plan(
                    Classification::Safe,
                    "every value parses as a date or time",
                    Vec::new(),
                )
            }
        }
        (FieldType::Date, FieldType::DateTime) => plan(
            Classification::Safe,
            "every date becomes a midnight instant",
            Vec::new(),
        ),
        (FieldType::DateTime, FieldType::Date) => plan(
            Classification::Lossy,
            "the time of day is dropped from every value",
            vec!["only the calendar date is kept".to_owned()],
        ),
        (FieldType::DateTime, FieldType::Time) => plan(
            Classification::Lossy,
            "the date is dropped from every value",
            vec!["only the time of day is kept".to_owned()],
        ),
        (FieldType::Time, FieldType::DateTime) => plan(
            Classification::Refused,
            "a time of day has no date to combine into an instant",
            vec!["set the date on each record first".to_owned()],
        ),
        (FieldType::Bool, FieldType::Text) => plan(
            Classification::Safe,
            "every true/false becomes the text `true` or `false`",
            Vec::new(),
        ),
        (FieldType::Bool, FieldType::Int | FieldType::Decimal) => plan(
            Classification::Lossy,
            "true becomes 1 and false becomes 0",
            vec!["the value no longer reads as a yes/no".to_owned()],
        ),
        (FieldType::Int | FieldType::Decimal, FieldType::Bool) => plan(
            Classification::Refused,
            "a number cannot be read as true or false without a rule",
            vec!["add a rule that sets the flag, or keep the number".to_owned()],
        ),
        (FieldType::Link, FieldType::Text) => plan(
            Classification::Safe,
            "every link becomes the target document's name as text",
            Vec::new(),
        ),
        (FieldType::Text, FieldType::Link) => plan(
            Classification::Refused,
            "a text value cannot be checked against the target DocType",
            vec!["link each value to a document first".to_owned()],
        ),
        (FieldType::Link, FieldType::DynamicLink) | (FieldType::DynamicLink, FieldType::Link) => {
            plan(
                Classification::Lossy,
                "the link's target DocType changes from fixed to per-record, or the reverse",
                vec!["check that the stored names still resolve".to_owned()],
            )
        }
        (FieldType::Bytes, FieldType::Text) => plan(
            Classification::Lossy,
            "raw bytes are reinterpreted as text",
            vec!["bytes that are not valid text are lost".to_owned()],
        ),
        (FieldType::Json, FieldType::Text) | (FieldType::Text, FieldType::Json) => plan(
            Classification::Safe,
            "the stored text is unchanged",
            Vec::new(),
        ),
        (FieldType::Json, _) => plan(
            Classification::Refused,
            "a JSON document cannot be read as a single value",
            vec!["extract the value into a field of its own first".to_owned()],
        ),
        _ => plan(
            Classification::Lossy,
            "the existing values are reinterpreted under the new type",
            vec!["review the values before publishing".to_owned()],
        ),
    }
}

/// Classifies a field's type change on a draft and states what happens to the existing values.
/// This is the entry point the client's type-change dialog calls;
/// [`DraftOp::ChangeType`](crate::DraftOp::ChangeType) uses the same classification to refuse a
/// destructive change.
#[must_use]
pub fn plan_type_change(
    draft: &crate::draft::Draft,
    field: &str,
    new_type: FieldType,
    sample_stats: &SampleStats,
) -> TypeChangePlan {
    match draft.field(None, field) {
        Some(spec) => classify(spec.ty, new_type, sample_stats),
        None => TypeChangePlan {
            from: FieldType::Text,
            to: new_type,
            classification: Classification::Refused,
            summary: format!("`{field}` is not a field of this draft"),
            detail: Vec::new(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_values() -> SampleStats {
        SampleStats {
            rows: 10,
            non_null: 10,
            ..SampleStats::default()
        }
    }

    #[test]
    fn the_same_type_is_safe() {
        let plan = classify(FieldType::Text, FieldType::Text, &with_values());
        assert_eq!(plan.classification, Classification::Safe);
    }

    #[test]
    fn an_empty_field_makes_every_convertible_change_safe() {
        let plan = classify(FieldType::DateTime, FieldType::Time, &SampleStats::empty());
        assert_eq!(plan.classification, Classification::Safe);
    }

    #[test]
    fn a_structural_change_is_refused_even_without_values() {
        let plan = classify(FieldType::Text, FieldType::Table, &SampleStats::empty());
        assert_eq!(plan.classification, Classification::Refused);
        let plan = classify(FieldType::Secret, FieldType::Text, &SampleStats::empty());
        assert_eq!(plan.classification, Classification::Refused);
    }

    #[test]
    fn a_fractional_decimal_to_int_is_lossy() {
        let stats = SampleStats {
            fractional: true,
            ..with_values()
        };
        let plan = classify(FieldType::Decimal, FieldType::Int, &stats);
        assert_eq!(plan.classification, Classification::Lossy);
        assert!(!plan.detail.is_empty());
    }

    #[test]
    fn text_with_unparsable_values_is_refused() {
        let stats = SampleStats {
            unparsable: 2,
            ..with_values()
        };
        let plan = classify(FieldType::Text, FieldType::Int, &stats);
        assert_eq!(plan.classification, Classification::Refused);
    }

    #[test]
    fn a_long_long_text_to_text_is_lossy() {
        let stats = SampleStats {
            max_text_len: Some(4000),
            ..with_values()
        };
        let plan = classify(FieldType::LongText, FieldType::Text, &stats);
        assert_eq!(plan.classification, Classification::Lossy);
    }
}
