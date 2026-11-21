//! Byte ranges and conditional requests for `GET /api/v1/files/{id}` (RFC 9110, single range).
//!
//! [`parse_range`] turns a `Range: bytes=start-end` header into a [`ByteRange`] or reports it
//! unsatisfiable; [`etag_matches`] decides `If-None-Match`; [`ContentRange`] renders the
//! `Content-Range` header; and [`RangeReader`] streams exactly the requested window of a body
//! without buffering the whole file.

use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

use spot_files::store::Body;

/// An inclusive byte range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteRange {
    /// The first byte to send.
    pub start: u64,
    /// The last byte to send (inclusive).
    pub end: u64,
}

impl ByteRange {
    /// The number of bytes in the range.
    pub fn len(&self) -> u64 {
        self.end - self.start + 1
    }

    /// Whether the range is empty (never true for a well-formed range).
    pub fn is_empty(&self) -> bool {
        self.end < self.start
    }
}

/// What a `Range` header asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RangeOutcome {
    /// No usable range: serve the whole file with `200`.
    Full,
    /// A satisfiable range: serve `206` with these bytes.
    Partial(ByteRange),
    /// A syntactically valid but unsatisfiable range: answer `416`.
    Unsatisfiable,
}

/// Parses a single `Range` header against the file's `size`.
///
/// A missing prefix, a multi-range request or a malformed spec is ignored ([`RangeOutcome::Full`]),
/// per RFC 9110. A well-formed range that cannot be satisfied (its first byte is past the end,
/// or a zero-length suffix) is [`RangeOutcome::Unsatisfiable`].
pub fn parse_range(header: &str, size: u64) -> RangeOutcome {
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        return RangeOutcome::Full;
    };
    let spec = spec.trim();
    if spec.contains(',') {
        return RangeOutcome::Full; // only a single range is supported
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeOutcome::Full;
    };
    let first = first.trim();
    let last = last.trim();

    if first.is_empty() {
        // A suffix range: the last `last` bytes.
        let Ok(suffix) = last.parse::<u64>() else {
            return RangeOutcome::Full;
        };
        if suffix == 0 || size == 0 {
            return RangeOutcome::Unsatisfiable;
        }
        let start = size.saturating_sub(suffix);
        return RangeOutcome::Partial(ByteRange {
            start,
            end: size - 1,
        });
    }

    let Ok(start) = first.parse::<u64>() else {
        return RangeOutcome::Full;
    };
    if start >= size {
        return RangeOutcome::Unsatisfiable;
    }
    if last.is_empty() {
        return RangeOutcome::Partial(ByteRange {
            start,
            end: size - 1,
        });
    }
    let Ok(end) = last.parse::<u64>() else {
        return RangeOutcome::Full;
    };
    if end < start {
        return RangeOutcome::Full;
    }
    RangeOutcome::Partial(ByteRange {
        start,
        end: end.min(size - 1),
    })
}

/// Whether an `If-None-Match` header matches `etag` (a strong validator, quotes included).
pub fn etag_matches(header: &str, etag: &str) -> bool {
    header.split(',').any(|candidate| {
        let candidate = candidate.trim();
        let strong = candidate.strip_prefix("W/").unwrap_or(candidate);
        strong == etag || strong == "*"
    })
}

/// The `Content-Range` header value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ContentRange {
    /// First byte sent.
    pub start: u64,
    /// Last byte sent (inclusive).
    pub end: u64,
    /// The complete file's size.
    pub size: u64,
}

impl ContentRange {
    /// The `bytes start-end/size` header value.
    pub fn to_header(self) -> String {
        format!("bytes {}-{}/{}", self.start, self.end, self.size)
    }

    /// The `bytes */size` header value an unsatisfiable range answers with.
    pub fn unsatisfiable(size: u64) -> String {
        format!("bytes */{size}")
    }
}

/// A body that streams exactly `range` of the underlying body, discarding the bytes before
/// `range.start` and stopping after `range.end`. Nothing is buffered beyond one read window.
pub struct RangeReader<R> {
    /// The wrapped body.
    inner: R,
    /// Bytes still to discard before the window starts.
    skip: u64,
    /// Bytes still to emit.
    remaining: u64,
    /// A fixed scratch buffer, so a poll never allocates.
    scratch: Box<[u8]>,
}

impl<R> RangeReader<R> {
    /// Wraps `inner` to serve `range`.
    pub fn new(inner: R, range: ByteRange) -> Self {
        RangeReader {
            inner,
            skip: range.start,
            remaining: range.len(),
            scratch: vec![0u8; 64 * 1024].into_boxed_slice(),
        }
    }
}

impl<R: AsyncRead + Unpin> AsyncRead for RangeReader<R> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        while this.skip > 0 {
            let want = this.skip.min(this.scratch.len() as u64) as usize;
            let mut scratch = ReadBuf::new(&mut this.scratch[..want]);
            match Pin::new(&mut this.inner).poll_read(cx, &mut scratch) {
                Poll::Ready(Ok(())) => {
                    let n = scratch.filled().len();
                    if n == 0 {
                        return Poll::Ready(Ok(())); // the body ended before the window
                    }
                    this.skip -= n as u64;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        if this.remaining == 0 {
            return Poll::Ready(Ok(()));
        }
        let want = this.remaining.min(buf.remaining() as u64) as usize;
        let mut scratch = ReadBuf::new(&mut this.scratch[..want]);
        match Pin::new(&mut this.inner).poll_read(cx, &mut scratch) {
            Poll::Ready(Ok(())) => {
                let filled = scratch.filled();
                let n = filled.len();
                buf.put_slice(filled);
                this.remaining -= n as u64;
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Wraps a stored [`Body`] as the streaming body for `range`.
pub fn range_body(body: Body, range: ByteRange) -> Body {
    Box::pin(RangeReader::new(body, range))
}

#[cfg(test)]
mod tests {
    use super::{ByteRange, ContentRange, RangeOutcome, etag_matches, parse_range};
    use tokio::io::AsyncReadExt;

    #[test]
    fn parses_start_end_and_open_ranges() {
        assert_eq!(
            parse_range("bytes=0-9", 100),
            RangeOutcome::Partial(ByteRange { start: 0, end: 9 })
        );
        assert_eq!(
            parse_range("bytes=10-", 100),
            RangeOutcome::Partial(ByteRange { start: 10, end: 99 })
        );
        assert_eq!(
            parse_range("bytes=-10", 100),
            RangeOutcome::Partial(ByteRange { start: 90, end: 99 })
        );
        // An end past the file is clamped.
        assert_eq!(
            parse_range("bytes=90-1000", 100),
            RangeOutcome::Partial(ByteRange { start: 90, end: 99 })
        );
    }

    #[test]
    fn ignores_malformed_and_multi_ranges() {
        for header in ["", "items=0-1", "bytes=abc", "bytes=0-1,3-4", "bytes=5-2"] {
            assert_eq!(parse_range(header, 100), RangeOutcome::Full, "{header}");
        }
    }

    #[test]
    fn reports_unsatisfiable_ranges() {
        assert_eq!(parse_range("bytes=100-", 100), RangeOutcome::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", 100), RangeOutcome::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-", 0), RangeOutcome::Unsatisfiable);
        assert_eq!(parse_range("bytes=-5", 0), RangeOutcome::Unsatisfiable);
    }

    #[test]
    fn etags_compare_strong_and_wildcard() {
        assert!(etag_matches("\"abc\"", "\"abc\""));
        assert!(etag_matches("W/\"abc\", \"x\"", "\"abc\""));
        assert!(etag_matches("*", "\"abc\""));
        assert!(!etag_matches("\"def\"", "\"abc\""));
        assert!(!etag_matches("", "\"abc\""));
    }

    #[test]
    fn content_range_headers() {
        assert_eq!(
            ContentRange {
                start: 1,
                end: 2,
                size: 10
            }
            .to_header(),
            "bytes 1-2/10"
        );
        assert_eq!(ContentRange::unsatisfiable(10), "bytes */10");
    }

    #[tokio::test]
    async fn range_reader_streams_only_the_window() {
        let body = spot_files::store::body_from_vec((0u8..=255).collect());
        let range = ByteRange { start: 10, end: 19 };
        let mut reader = super::RangeReader::new(body, range);
        let mut out = Vec::new();
        reader
            .read_to_end(&mut out)
            .await
            .unwrap_or_else(|e| panic!("read: {e:?}"));
        assert_eq!(out, (10u8..=19).collect::<Vec<u8>>());

        // A suffix range.
        let body = spot_files::store::body_from_vec(vec![1, 2, 3, 4, 5]);
        let mut reader = super::RangeReader::new(body, ByteRange { start: 3, end: 4 });
        let mut out = Vec::new();
        reader
            .read_to_end(&mut out)
            .await
            .unwrap_or_else(|e| panic!("read: {e:?}"));
        assert_eq!(out, vec![4, 5]);
    }
}
