//! Canonical path numbers and the `Range` header of a segment read.

/// A canonical decimal: digits only, no sign, no leading zero (except `0` itself), at most 20
/// digits, fits a `u64`. `..`, an encoded separator, an empty or padded part is `None`, so a
/// hand-built path never reaches a key.
#[must_use]
pub fn canonical_number(part: &str) -> Option<u64> {
    if part.is_empty() || part.len() > 20 || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    if part.len() > 1 && part.starts_with('0') {
        return None;
    }
    part.parse().ok()
}

/// Splits the `{first}-{last}` part of a segment path into its two canonical numbers.
#[must_use]
pub fn canonical_range(part: &str) -> Option<(u64, u64)> {
    let (first, last) = part.split_once('-')?;
    Some((canonical_number(first)?, canonical_number(last)?))
}

/// What a `Range` header asks of an object of known size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeRequest {
    /// The whole object (no header, or one this surface ignores).
    Full,
    /// Bytes `start..=end`.
    Partial {
        /// First byte.
        start: u64,
        /// Last byte, inclusive.
        end: u64,
    },
    /// The range lies outside the object (`416`).
    Unsatisfiable,
}

/// Reads a `Range` header against an object of `total` bytes.
///
/// One `bytes=a-b`, `bytes=a-` or `bytes=-n` range is honoured. A malformed header, a unit
/// other than bytes and a list of several ranges are ignored (the whole object is served), as
/// HTTP allows.
#[must_use]
pub fn parse_range(header: Option<&str>, total: u64) -> RangeRequest {
    let Some(header) = header else {
        return RangeRequest::Full;
    };
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeRequest::Full;
    };
    if spec.contains(',') {
        return RangeRequest::Full;
    }
    let Some((from, to)) = spec.split_once('-') else {
        return RangeRequest::Full;
    };
    let number = |text: &str| -> Option<u64> {
        let text = text.trim();
        (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
            .then(|| text.parse().ok())
            .flatten()
    };
    if total == 0 {
        return RangeRequest::Unsatisfiable;
    }
    match (number(from), number(to)) {
        (Some(start), Some(end)) if start <= end => {
            if start >= total {
                RangeRequest::Unsatisfiable
            } else {
                RangeRequest::Partial {
                    start,
                    end: end.min(total - 1),
                }
            }
        }
        (Some(start), None) if to.trim().is_empty() => {
            if start >= total {
                RangeRequest::Unsatisfiable
            } else {
                RangeRequest::Partial {
                    start,
                    end: total - 1,
                }
            }
        }
        (None, Some(suffix)) if from.trim().is_empty() => {
            if suffix == 0 {
                RangeRequest::Unsatisfiable
            } else {
                RangeRequest::Partial {
                    start: total.saturating_sub(suffix),
                    end: total - 1,
                }
            }
        }
        _ => RangeRequest::Full,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_are_canonical_or_nothing() {
        assert_eq!(canonical_number("0"), Some(0));
        assert_eq!(canonical_number("17"), Some(17));
        for bad in [
            "",
            "01",
            "00000000000000000001",
            "..",
            "1.5",
            "-1",
            "+1",
            "1/2",
            "%2e",
            "18446744073709551616",
        ] {
            assert_eq!(canonical_number(bad), None, "{bad}");
        }
        assert_eq!(canonical_range("1000-1999"), Some((1000, 1999)));
        assert_eq!(canonical_range("1000"), None);
        assert_eq!(canonical_range("1-2-3"), None);
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 100), RangeRequest::Full);
        assert_eq!(
            parse_range(Some("bytes=0-9"), 100),
            RangeRequest::Partial { start: 0, end: 9 }
        );
        assert_eq!(
            parse_range(Some("bytes=90-"), 100),
            RangeRequest::Partial { start: 90, end: 99 }
        );
        assert_eq!(
            parse_range(Some("bytes=-10"), 100),
            RangeRequest::Partial { start: 90, end: 99 }
        );
        assert_eq!(
            parse_range(Some("bytes=95-500"), 100),
            RangeRequest::Partial { start: 95, end: 99 }
        );
        assert_eq!(
            parse_range(Some("bytes=100-"), 100),
            RangeRequest::Unsatisfiable
        );
        assert_eq!(parse_range(Some("bytes=0-1,5-6"), 100), RangeRequest::Full);
        assert_eq!(parse_range(Some("items=0-1"), 100), RangeRequest::Full);
        assert_eq!(parse_range(Some("bytes=9-3"), 100), RangeRequest::Full);
        assert_eq!(
            parse_range(Some("bytes=0-0"), 0),
            RangeRequest::Unsatisfiable
        );
    }
}
