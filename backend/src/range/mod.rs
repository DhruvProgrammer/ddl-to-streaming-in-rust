//! Byte-range parsing and resolution.
//!
//! Two directions matter:
//!   * inbound  — the browser's `Range` request header (what the player wants)
//!   * outbound — the `Range` header we ask the origin for
//!
//! Everything here is pure, allocation-light and exhaustively unit tested.

use std::fmt;

/// `bytes=0-` / `bytes=0-1023` / `bytes=-1024`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteRange {
    /// Inclusive start, inclusive end. `end == None` means "to the end".
    FromTo { start: u64, end: Option<u64> },
    /// Last `len` bytes.
    Suffix { len: u64 },
}

/// Outcome of parsing a client `Range` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeSpec {
    /// No header present: serve the whole representation.
    None,
    /// Header present but malformed / unknown unit. Per RFC 9110 §14.2 we
    /// ignore it and serve the whole representation.
    Ignored,
    /// A single usable range.
    Single(ByteRange),
    /// Multiple ranges requested. We deliberately serve only the first as a
    /// simple `206` (allowed by RFC 9110 §14.2) rather than emitting
    /// `multipart/byteranges`, which no `<video>` element can use.
    FirstOfMany(ByteRange),
}

impl RangeSpec {
    /// The range we will actually serve, if any.
    pub const fn requested(self) -> Option<ByteRange> {
        match self {
            Self::Single(r) | Self::FirstOfMany(r) => Some(r),
            Self::None | Self::Ignored => None,
        }
    }

    /// Whether the client explicitly asked for partial content.
    pub const fn is_partial_request(self) -> bool {
        matches!(self, Self::Single(_) | Self::FirstOfMany(_))
    }
}

/// Why a well-formed range cannot be satisfied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsatisfiable {
    /// Client asked for bytes at or past the end of a known-size resource.
    StartBeyondEnd,
    /// `bytes=-N` against a resource of unknown length.
    SuffixWithoutLength,
    /// Zero-length representation.
    EmptyResource,
}

impl fmt::Display for Unsatisfiable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StartBeyondEnd => {
                f.write_str("requested start is at or beyond the end of the resource")
            }
            Self::SuffixWithoutLength => {
                f.write_str("suffix range requested but resource length is unknown")
            }
            Self::EmptyResource => f.write_str("resource has zero length"),
        }
    }
}

/// A concrete, servable slice of the origin representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub start: u64,
    /// Inclusive end. `None` only when the origin length is unknown and the
    /// range was open-ended; such a response is emitted as `200` chunked.
    pub end: Option<u64>,
    pub total: Option<u64>,
}

impl Resolution {
    /// Bytes we will transmit, when the length is knowable up front.
    pub const fn length(&self) -> Option<u64> {
        match self.end {
            Some(e) if e >= self.start => Some(e - self.start + 1),
            Some(_) => Some(0),
            None => None,
        }
    }

    pub const fn covers_whole(&self, total: Option<u64>) -> bool {
        self.start == 0 && matches!((self.end, total), (Some(e), Some(t)) if e + 1 >= t)
    }

    /// `Content-Range: bytes 0-1048575/734003200`, only when complete.
    pub fn content_range(&self) -> Option<String> {
        let end = self.end?;
        let total = self.total?;
        Some(format!("bytes {}-{end}/{total}", self.start))
    }

    /// `Content-Range: bytes */734003200` for a `416`.
    pub fn unsatisfied_header(total: Option<u64>) -> Option<String> {
        Some(format!("bytes */{}", total?))
    }
}

/// Parse a single HTTP entity-tag (weak tags allowed, lists are not).
pub fn parse_single_etag(raw: &str) -> Option<String> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    // `If-None-Match` may be a list; we only ever match one strong/weak tag.
    if t.starts_with('*') {
        return Some("*".to_owned());
    }
    let first = t.split(',').next()?.trim();
    let inner = first.strip_prefix("W/").unwrap_or(first);
    if inner.len() >= 2 && inner.starts_with('"') && inner.ends_with('"') {
        Some(first.to_owned())
    } else {
        None
    }
}

/// Parse a `Content-Range` response header.
pub fn parse_content_range(raw: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = raw.trim();
    if rest.len() < 5 || !rest[..5].eq_ignore_ascii_case("bytes") {
        return None;
    }
    let rest = rest[5..].trim_start();
    let (range_part, complete_part) = rest.split_once('/')?;
    let (s, e) = range_part.trim().split_once('-')?;
    let start: u64 = s.trim().parse().ok()?;
    let end: u64 = e.trim().parse().ok()?;
    if end < start {
        return None;
    }
    let complete_part = complete_part.trim();
    let total = if complete_part == "*" {
        None
    } else {
        let t: u64 = complete_part.parse().ok()?;
        if t < end + 1 {
            return None;
        }
        Some(t)
    };
    Some((start, end, total))
}

/// Parse a `Content-Length`. Rejects empty, non-numeric, multi-valued and
/// absurd values rather than trusting them.
pub fn parse_content_length(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() || raw.contains(',') {
        return None;
    }
    raw.parse().ok()
}

/// Parse a client `Range` header value.
pub fn parse_range(raw: &str) -> RangeSpec {
    let raw = raw.trim();
    let Some(spec) = raw
        .strip_prefix("bytes=")
        .or_else(|| raw.strip_prefix("BYTES="))
    else {
        // Unknown range unit: ignore per RFC, never error.
        return RangeSpec::Ignored;
    };
    let mut first: Option<ByteRange> = None;
    for part in spec.split(',') {
        let Some(r) = parse_single(part) else {
            return RangeSpec::Ignored;
        };
        if first.is_none() {
            first = Some(r);
        }
    }
    match first {
        None => RangeSpec::Ignored,
        Some(r) if spec.contains(',') => RangeSpec::FirstOfMany(r),
        Some(r) => RangeSpec::Single(r),
    }
}

fn parse_single(part: &str) -> Option<ByteRange> {
    let part = part.trim();
    if part.is_empty() {
        return None;
    }
    let (s, e) = part.split_once('-')?;
    let (s, e) = (s.trim(), e.trim());
    match (s.is_empty(), e.is_empty()) {
        (true, true) => None,
        // bytes=-N
        (true, false) => {
            let len: u64 = e.parse().ok()?;
            if len == 0 {
                return None;
            }
            Some(ByteRange::Suffix { len })
        }
        // bytes=N-
        (false, true) => Some(ByteRange::FromTo {
            start: s.parse().ok()?,
            end: None,
        }),
        // bytes=N-M
        (false, false) => {
            let start: u64 = s.parse().ok()?;
            let end: u64 = e.parse().ok()?;
            if end < start {
                return None;
            }
            Some(ByteRange::FromTo {
                start,
                end: Some(end),
            })
        }
    }
}

/// Turn a requested range plus the known origin length into a servable slice.
pub fn resolve(spec: RangeSpec, total: Option<u64>) -> Result<Resolution, Unsatisfiable> {
    let Some(range) = spec.requested() else {
        // Whole representation. With a known length we know the end.
        return Ok(Resolution {
            start: 0,
            end: total.map(|t| t.saturating_sub(1)),
            total,
        });
    };

    match (range, total) {
        (ByteRange::FromTo { start, end }, Some(total)) => {
            if total == 0 {
                return Err(Unsatisfiable::EmptyResource);
            }
            if start >= total {
                return Err(Unsatisfiable::StartBeyondEnd);
            }
            let last = total - 1;
            let end = end.map_or(last, |e| e.min(last));
            Ok(Resolution {
                start,
                end: Some(end),
                total: Some(total),
            })
        }
        (ByteRange::FromTo { start, end }, None) => {
            // Unknown origin length: an explicit end still gives us a
            // Content-Length; an open end means chunked 200.
            Ok(Resolution {
                start,
                end,
                total: None,
            })
        }
        (ByteRange::Suffix { len }, Some(total)) => {
            if total == 0 {
                return Err(Unsatisfiable::EmptyResource);
            }
            let start = total.saturating_sub(len);
            Ok(Resolution {
                start,
                end: Some(total - 1),
                total: Some(total),
            })
        }
        (ByteRange::Suffix { .. }, None) => Err(Unsatisfiable::SuffixWithoutLength),
    }
}

/// Format the outbound `Range` header value we ask the origin for.
pub fn request_header(resolution: &Resolution) -> Option<String> {
    match resolution.end {
        Some(end) if resolution.start == 0 && resolution.total == Some(end + 1) => {
            // Whole object: sending a Range would only risk a 206 instead of 200.
            None
        }
        Some(end) => Some(format!("bytes={}-{}", resolution.start, end)),
        None => Some(format!("bytes={}-", resolution.start)),
    }
}

/// Clamp a requested absolute offset into a servable window.
///
/// Used by the seek engine, which works in absolute byte offsets rather than
/// headers. Returns `None` when the offset cannot be served.
pub fn open_ended_at(start: u64, total: Option<u64>) -> Option<Resolution> {
    let end = total.and_then(|t| t.checked_sub(1));
    if end == Some(0) && start > 0 {
        return None;
    }
    if let Some(t) = total {
        if start >= t {
            return None;
        }
    }
    Some(Resolution { start, end, total })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_three_forms() {
        assert_eq!(
            parse_range("bytes=0-1048575"),
            RangeSpec::Single(ByteRange::FromTo {
                start: 0,
                end: Some(1_048_575)
            })
        );
        assert_eq!(
            parse_range("bytes=0-"),
            RangeSpec::Single(ByteRange::FromTo {
                start: 0,
                end: None
            })
        );
        assert_eq!(
            parse_range("bytes=-2048"),
            RangeSpec::Single(ByteRange::Suffix { len: 2048 })
        );
    }

    #[test]
    fn ignores_malformed_and_unknown_units() {
        for bad in [
            "",
            "items=0-10",
            "bytes=",
            "bytes=abc-def",
            "bytes=5-1",
            "bytes=-0",
            "bytes=--5",
            "bytes=10",
            "bytes=1-2-3",
        ] {
            assert_eq!(parse_range(bad), RangeSpec::Ignored, "{bad:?}");
        }
    }

    #[test]
    fn keeps_only_first_of_many() {
        let spec = parse_range("bytes=0-99, 200-299, -10");
        assert_eq!(
            spec,
            RangeSpec::FirstOfMany(ByteRange::FromTo {
                start: 0,
                end: Some(99)
            })
        );
        assert!(spec.is_partial_request());
    }

    #[test]
    fn content_range_round_trip() {
        assert_eq!(
            parse_content_range("bytes 0-1048575/734003200"),
            Some((0, 1_048_575, Some(734_003_200)))
        );
        assert_eq!(parse_content_range("bytes 0-99/*"), Some((0, 99, None)));
        assert_eq!(
            parse_content_range("BYTES 10-20/21"),
            Some((10, 20, Some(21)))
        );
    }

    #[test]
    fn rejects_hostile_content_range() {
        for bad in [
            "bytes 0-99/50",
            "bytes 99-10/200",
            "bytes 0-abc/200",
            "bytes */200",
            "0-99/200",
            "bytes 0-99/-1",
            "",
            "bytes 0-99/200,",
        ] {
            assert!(parse_content_range(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn content_length_is_strict() {
        assert_eq!(parse_content_length("0"), Some(0));
        assert_eq!(parse_content_length(" 12345 "), Some(12_345));
        assert_eq!(parse_content_length(""), None);
        assert_eq!(parse_content_length("abc"), None);
        assert_eq!(parse_content_length("1, 2"), None);
        assert_eq!(parse_content_length("-5"), None);
        assert_eq!(parse_content_length("18446744073709551616"), None);
    }

    #[test]
    fn single_etag_only() {
        assert_eq!(parse_single_etag("\"abc\"").as_deref(), Some("\"abc\""));
        assert_eq!(parse_single_etag("W/\"abc\"").as_deref(), Some("W/\"abc\""));
        assert_eq!(parse_single_etag("*").as_deref(), Some("*"));
        assert_eq!(parse_single_etag("\"a\", \"b\"").as_deref(), Some("\"a\""));
        assert_eq!(parse_single_etag("abc"), None);
        assert_eq!(parse_single_etag("\"unterminated"), None);
        assert_eq!(parse_single_etag(""), None);
    }

    #[test]
    fn resolution_clamps_to_total() {
        let r = resolve(
            RangeSpec::Single(ByteRange::FromTo {
                start: 10,
                end: Some(500),
            }),
            Some(100),
        )
        .unwrap();
        assert_eq!(
            r,
            Resolution {
                start: 10,
                end: Some(99),
                total: Some(100)
            }
        );
        assert_eq!(r.length(), Some(90));
        assert_eq!(r.content_range().as_deref(), Some("bytes 10-99/100"));
    }

    #[test]
    fn open_ended_resolution_uses_total() {
        let r = resolve(
            RangeSpec::Single(ByteRange::FromTo {
                start: 500,
                end: None,
            }),
            Some(1000),
        )
        .unwrap();
        assert_eq!(
            r,
            Resolution {
                start: 500,
                end: Some(999),
                total: Some(1000)
            }
        );
        assert_eq!(r.length(), Some(500));
    }

    #[test]
    fn suffix_resolution() {
        let r = resolve(
            RangeSpec::Single(ByteRange::Suffix { len: 100 }),
            Some(1000),
        )
        .unwrap();
        assert_eq!(
            r,
            Resolution {
                start: 900,
                end: Some(999),
                total: Some(1000)
            }
        );
        // suffix longer than the resource clamps to the whole thing
        let r = resolve(
            RangeSpec::Single(ByteRange::Suffix { len: 5000 }),
            Some(1000),
        )
        .unwrap();
        assert_eq!(r.start, 0);
        assert_eq!(r.length(), Some(1000));
    }

    #[test]
    fn unsatisfiable_cases() {
        assert_eq!(
            resolve(
                RangeSpec::Single(ByteRange::FromTo {
                    start: 1000,
                    end: None
                }),
                Some(1000)
            ),
            Err(Unsatisfiable::StartBeyondEnd)
        );
        assert_eq!(
            resolve(RangeSpec::Single(ByteRange::Suffix { len: 10 }), None),
            Err(Unsatisfiable::SuffixWithoutLength)
        );
        assert_eq!(
            resolve(
                RangeSpec::Single(ByteRange::FromTo {
                    start: 0,
                    end: None
                }),
                Some(0)
            ),
            Err(Unsatisfiable::EmptyResource)
        );
    }

    #[test]
    fn unknown_length_open_range_is_chunked() {
        let r = resolve(
            RangeSpec::Single(ByteRange::FromTo {
                start: 4096,
                end: None,
            }),
            None,
        )
        .unwrap();
        assert_eq!(r.start, 4096);
        assert_eq!(r.end, None);
        assert_eq!(r.length(), None);
        assert!(r.content_range().is_none());
    }

    #[test]
    fn whole_representation_short_circuits_outbound_range() {
        let r = resolve(RangeSpec::None, Some(1000)).unwrap();
        assert!(request_header(&r).is_none());
        // A client asking for `bytes=0-` against a resource whose length we
        // already know covers the whole object, so we send no Range at all and
        // take the origin's plain 200.
        let r = resolve(
            RangeSpec::Single(ByteRange::FromTo {
                start: 0,
                end: None,
            }),
            Some(1000),
        )
        .unwrap();
        assert!(request_header(&r).is_none());
        // A genuine partial slice still goes out as a range.
        let r = resolve(
            RangeSpec::Single(ByteRange::FromTo {
                start: 10,
                end: None,
            }),
            Some(1000),
        )
        .unwrap();
        assert_eq!(request_header(&r).as_deref(), Some("bytes=10-999"));
    }

    #[test]
    fn unsatisfied_header_for_416() {
        assert_eq!(
            Resolution::unsatisfied_header(Some(42)).as_deref(),
            Some("bytes */42")
        );
        assert!(Resolution::unsatisfied_header(None).is_none());
    }

    #[test]
    fn open_ended_at_guards() {
        assert!(open_ended_at(10, Some(10)).is_none());
        assert_eq!(
            open_ended_at(10, Some(100)),
            Some(Resolution {
                start: 10,
                end: Some(99),
                total: Some(100)
            })
        );
        // unknown total: always allowed
        assert_eq!(
            open_ended_at(u64::MAX - 1, None).map(|r| r.start),
            Some(u64::MAX - 1)
        );
    }

    #[test]
    fn ranges_never_overflow_at_u64_limits() {
        let r = resolve(
            RangeSpec::Single(ByteRange::FromTo {
                start: u64::MAX - 10,
                end: Some(u64::MAX),
            }),
            Some(u64::MAX),
        )
        .unwrap();
        assert_eq!(r.end, Some(u64::MAX - 1));
        assert_eq!(r.length(), Some(10));
    }
}
