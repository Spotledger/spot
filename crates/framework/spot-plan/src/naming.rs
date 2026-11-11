//! Parsing and validation of naming patterns (WP-MET-02.T2).
//!
//! A naming pattern describes how to generate a document's name/number field.
//! Patterns are like `INV-.YYYY.-.#####` and can include:
//! - Text literals
//! - `.YYYY` or `.YY` for year
//! - `.MM` for month
//! - `.DD` for day
//! - `.#####` for a counter
//! - `field:<name>` to reference a field
//! - `hash` for a content hash

use std::collections::BTreeSet;

/// One part of a naming pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NamingPart {
    /// A literal text string.
    Literal(String),
    /// A year component (YYYY for 4 digits, YY for 2).
    Year {
        /// Number of digits (2 or 4).
        width: u8,
    },
    /// A month component (always 2 digits).
    Month,
    /// A day component (always 2 digits).
    Day,
    /// A counter with the given number of digits.
    Counter {
        /// Number of digits for the counter.
        width: u8,
    },
    /// Reference to a field by name.
    Field(String),
    /// A content hash.
    Hash,
}

/// The parsed naming pattern for a DocType.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamingPlan {
    /// The pattern parts in order.
    pub parts: Vec<NamingPart>,
    /// Fields referenced in the pattern.
    pub field_refs: BTreeSet<String>,
}

impl NamingPlan {
    /// Parses a naming pattern.
    ///
    /// Returns an error if the pattern syntax is invalid.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let pattern = "INV-.YYYY.-.#####";
    /// let plan = NamingPlan::parse(pattern).unwrap();
    /// assert_eq!(plan.parts.len(), 4); // "INV-", Year, ".-", Counter
    /// ```
    pub fn parse(pattern: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        let mut field_refs = BTreeSet::new();
        let mut i = 0;
        let bytes = pattern.as_bytes();

        while i < bytes.len() {
            // Check for special patterns starting with '.'
            if bytes[i] == b'.' && i + 1 < bytes.len() {
                let next_byte = bytes[i + 1];
                match next_byte {
                    b'Y' => {
                        // Parse .Y+ (year)
                        i += 1; // skip '.'
                        let mut width = 1;
                        i += 1; // consume first 'Y'
                        while i < bytes.len() && bytes[i] == b'Y' && width < 4 {
                            i += 1;
                            width += 1;
                        }
                        parts.push(NamingPart::Year { width: width as u8 });
                        continue;
                    }
                    b'M' => {
                        // Parse .M[M] (month)
                        i += 1; // skip '.'
                        i += 1; // consume first 'M'
                        if i < bytes.len() && bytes[i] == b'M' {
                            i += 1; // consume second 'M'
                        }
                        parts.push(NamingPart::Month);
                        continue;
                    }
                    b'D' => {
                        // Parse .D[D] (day)
                        i += 1; // skip '.'
                        i += 1; // consume first 'D'
                        if i < bytes.len() && bytes[i] == b'D' {
                            i += 1; // consume second 'D'
                        }
                        parts.push(NamingPart::Day);
                        continue;
                    }
                    b'#' => {
                        // Parse .#+ (counter)
                        i += 1; // skip '.'
                        let mut width = 1;
                        i += 1; // consume first '#'
                        while i < bytes.len() && bytes[i] == b'#' && width < 20 {
                            i += 1;
                            width += 1;
                        }
                        parts.push(NamingPart::Counter { width: width as u8 });
                        continue;
                    }
                    _ => {}
                }
            }

            // Check for field: reference
            if i + 6 <= bytes.len() && &bytes[i..i + 6] == b"field:" {
                i += 6;
                let mut field_name = String::new();
                while i < bytes.len() {
                    let ch = bytes[i] as char;
                    if ch.is_alphanumeric() || ch == '_' {
                        field_name.push(ch);
                        i += 1;
                    } else {
                        break;
                    }
                }
                if !field_name.is_empty() {
                    field_refs.insert(field_name.clone());
                    parts.push(NamingPart::Field(field_name));
                }
                continue;
            }

            // Check for "hash" (word boundary required)
            if i + 4 <= bytes.len() && &bytes[i..i + 4] == b"hash" {
                let is_word_start = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
                let is_word_end = i + 4 == bytes.len() || !bytes[i + 4].is_ascii_alphanumeric();
                if is_word_start && is_word_end {
                    parts.push(NamingPart::Hash);
                    i += 4;
                    continue;
                }
            }

            // Accumulate literal text
            let mut literal = String::new();
            while i < bytes.len() {
                // Look ahead for special patterns
                if bytes[i] == b'.' && i + 1 < bytes.len() {
                    match bytes[i + 1] {
                        b'Y' | b'M' | b'D' | b'#' => break,
                        _ => {}
                    }
                }
                if i + 6 <= bytes.len() && &bytes[i..i + 6] == b"field:" {
                    break;
                }
                if i + 4 <= bytes.len() && &bytes[i..i + 4] == b"hash" {
                    let is_word_start = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
                    let is_word_end = i + 4 == bytes.len() || !bytes[i + 4].is_ascii_alphanumeric();
                    if is_word_start && is_word_end {
                        break;
                    }
                }
                literal.push(bytes[i] as char);
                i += 1;
            }
            if !literal.is_empty() {
                parts.push(NamingPart::Literal(literal));
            }
        }

        Ok(NamingPlan { parts, field_refs })
    }

    /// Returns the field names referenced in this pattern.
    pub fn field_names(&self) -> Vec<&str> {
        self.field_refs.iter().map(|s| s.as_str()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn parse_invoice_pattern() {
        let plan = NamingPlan::parse("INV-.YYYY.-.#####").unwrap();
        println!("Parsed parts: {:?}", plan.parts);
        assert_eq!(
            plan.parts.len(),
            4,
            "should have 4 parts: literal, year, literal, counter"
        );
        assert!(matches!(plan.parts[0], NamingPart::Literal(ref s) if s == "INV-"));
        assert!(matches!(plan.parts[1], NamingPart::Year { width: 4 }));
        assert!(matches!(plan.parts[2], NamingPart::Literal(ref s) if s == ".-"));
        assert!(matches!(plan.parts[3], NamingPart::Counter { width: 5 }));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn parse_pattern_with_field_ref() {
        let plan = NamingPlan::parse("SO-field:division-#####").unwrap();
        println!("Parsed parts: {:?}", plan.parts);
        println!("Field refs: {:?}", plan.field_refs);
        assert!(
            plan.field_refs.contains("division"),
            "should have 'division' field ref"
        );
    }
}
