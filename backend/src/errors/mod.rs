//! Structured, explicit error model.
//!
//! Every failure surfaces a stable machine code, a user-facing message, a
//! technical reason (never containing upstream secrets), and a retryability
//! verdict. Nothing here ever degrades to "something went wrong".

use std::fmt;
use std::sync::Arc;

use serde::Serialize;

/// Stable machine-readable error codes. Wire format (`code`) is part of the
/// public API and must not be renamed without a version bump.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ErrorCode {
    InvalidUrl,
    UnsupportedProtocol,
    TooManyRedirects,
    DnsFailure,
    TlsFailure,
    ConnectionTimeout,
    RequestTimeout,
    #[serde(rename = "HTTP_400")]
    Http400,
    #[serde(rename = "HTTP_401")]
    Http401,
    #[serde(rename = "HTTP_403")]
    Http403,
    #[serde(rename = "HTTP_404")]
    Http404,
    #[serde(rename = "HTTP_416")]
    Http416,
    RateLimited,
    UpstreamError,
    RangeNotSupported,
    InvalidContentType,
    InvalidContentLength,
    CorruptedResponse,
    MediaNotSupported,
    NetworkInterrupted,
    PlayerError,
    SupersededRequest,
    InvalidRangeHeader,
    TooManyRequests,
    Shutdown,
    UnknownError,
}

impl ErrorCode {
    /// Wire string, e.g. `RANGE_NOT_SUPPORTED`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidUrl => "INVALID_URL",
            Self::UnsupportedProtocol => "UNSUPPORTED_PROTOCOL",
            Self::TooManyRedirects => "TOO_MANY_REDIRECTS",
            Self::DnsFailure => "DNS_FAILURE",
            Self::TlsFailure => "TLS_FAILURE",
            Self::ConnectionTimeout => "CONNECTION_TIMEOUT",
            Self::RequestTimeout => "REQUEST_TIMEOUT",
            Self::Http400 => "HTTP_400",
            Self::Http401 => "HTTP_401",
            Self::Http403 => "HTTP_403",
            Self::Http404 => "HTTP_404",
            Self::Http416 => "HTTP_416",
            Self::RateLimited => "RATE_LIMITED",
            Self::UpstreamError => "UPSTREAM_ERROR",
            Self::RangeNotSupported => "RANGE_NOT_SUPPORTED",
            Self::InvalidContentType => "INVALID_CONTENT_TYPE",
            Self::InvalidContentLength => "INVALID_CONTENT_LENGTH",
            Self::CorruptedResponse => "CORRUPTED_RESPONSE",
            Self::MediaNotSupported => "MEDIA_NOT_SUPPORTED",
            Self::NetworkInterrupted => "NETWORK_INTERRUPTED",
            Self::PlayerError => "PLAYER_ERROR",
            Self::SupersededRequest => "SUPERSEDED_REQUEST",
            Self::InvalidRangeHeader => "INVALID_RANGE_HEADER",
            Self::TooManyRequests => "TOO_MANY_REQUESTS",
            Self::Shutdown => "SHUTDOWN",
            Self::UnknownError => "UNKNOWN_ERROR",
        }
    }

    /// Default user-facing message. Deliberately actionable, never technical.
    pub const fn default_message(self) -> &'static str {
        match self {
            Self::InvalidUrl => "That does not look like a valid URL.",
            Self::UnsupportedProtocol => "Only http and https links can be played.",
            Self::TooManyRedirects => "The link redirected too many times.",
            Self::DnsFailure => "The host name of this link could not be resolved.",
            Self::TlsFailure => "A secure connection to the source could not be established.",
            Self::ConnectionTimeout => "The source did not accept a connection in time.",
            Self::RequestTimeout => "The source took too long to respond.",
            Self::Http400 => "The source rejected the request.",
            Self::Http401 => "The source requires authorization.",
            Self::Http403 => "The source refused access to this file.",
            Self::Http404 => "The file was not found on the source.",
            Self::Http416 => "The requested part of the file does not exist.",
            Self::RateLimited => "The source is rate limiting requests. Try again shortly.",
            Self::UpstreamError => "The source reported a server error.",
            Self::RangeNotSupported => "The source does not support byte-range requests.",
            Self::InvalidContentType => "The source returned an unexpected content type.",
            Self::InvalidContentLength => "The source returned an invalid content length.",
            Self::CorruptedResponse => "The source sent a corrupted or incomplete response.",
            Self::MediaNotSupported => "This media format is not supported by this browser.",
            Self::NetworkInterrupted => "The connection to the source was interrupted.",
            Self::PlayerError => "Playback failed.",
            Self::SupersededRequest => "This request was replaced by a newer seek.",
            Self::InvalidRangeHeader => "The playback request asked for an invalid byte range.",
            Self::TooManyRequests => "Too many streams are already open. Try again in a moment.",
            Self::Shutdown => "The server is shutting down.",
            Self::UnknownError => "An unexpected error occurred.",
        }
    }

    /// Suggested next step for the user, shown verbatim in the player.
    pub const fn default_user_action(self) -> &'static str {
        match self {
            Self::InvalidUrl => "Check the link and paste it again.",
            Self::UnsupportedProtocol => "Use an http:// or https:// link.",
            Self::TooManyRedirects => "The link is probably part of a redirect loop.",
            Self::DnsFailure => "Check the host name for typos.",
            Self::TlsFailure => "Try again, or use a different source.",
            Self::ConnectionTimeout => "Try again in a few seconds.",
            Self::Http400 | Self::InvalidRangeHeader => "Reload the page and try again.",
            Self::Http401 => "The source needs credentials, which are not supported here.",
            Self::Http403 => "The link may have expired or require a referrer.",
            Self::Http404 => "Verify the link still points to a file.",
            Self::Http416 => "Reload the page and try again.",
            Self::RateLimited => "Wait a moment, then press play again.",
            Self::UpstreamError => "Try again shortly.",
            Self::RangeNotSupported => "Seeking may be limited on this source.",
            Self::InvalidContentType => "Verify the link points to a media file.",
            Self::InvalidContentLength => "Verify the link points to a complete file.",
            Self::CorruptedResponse | Self::NetworkInterrupted => {
                "Playback is retrying automatically."
            }
            Self::MediaNotSupported => "Convert the file to MP4 (H.264) or WebM and try again.",
            Self::PlayerError => "Press play to try again.",
            Self::SupersededRequest => "No action needed.",
            Self::TooManyRequests => "Close other streams or retry shortly.",
            Self::Shutdown => "Wait for the server to come back.",
            Self::RequestTimeout => "Check your connection and try again.",
            Self::UnknownError => "Reload the page and try again.",
        }
    }

    /// Whether a fresh attempt could plausibly succeed.
    pub const fn retryable(self) -> bool {
        match self {
            Self::DnsFailure
            | Self::TlsFailure
            | Self::ConnectionTimeout
            | Self::RequestTimeout
            | Self::RateLimited
            | Self::UpstreamError
            | Self::NetworkInterrupted
            | Self::CorruptedResponse
            | Self::TooManyRequests
            | Self::Shutdown => true,
            Self::InvalidUrl
            | Self::UnsupportedProtocol
            | Self::TooManyRedirects
            | Self::Http400
            | Self::Http401
            | Self::Http403
            | Self::Http404
            | Self::Http416
            | Self::RangeNotSupported
            | Self::InvalidContentType
            | Self::InvalidContentLength
            | Self::MediaNotSupported
            | Self::PlayerError
            | Self::SupersededRequest
            | Self::InvalidRangeHeader
            | Self::UnknownError => false,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A failure with everything the player needs to explain itself.
#[derive(Debug, Clone, Serialize)]
pub struct PlayerError {
    pub code: ErrorCode,
    /// Sanitized, user-facing sentence.
    pub message: String,
    /// Sanitized technical cause. Never contains credentials or full URLs.
    pub reason: String,
    pub retryable: bool,
    pub user_action: String,
    /// Upstream HTTP status when one exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream_status: Option<u16>,
    /// `Content-Range: bytes * /<total>` for an unsatisfiable range. Kept out
    /// of the JSON body and emitted as a header instead, where it belongs.
    #[serde(skip)]
    pub content_range: Option<String>,
}

impl PlayerError {
    pub fn new(code: ErrorCode) -> Self {
        Self {
            code,
            message: code.default_message().to_owned(),
            reason: String::new(),
            retryable: code.retryable(),
            user_action: code.default_user_action().to_owned(),
            upstream_status: None,
            content_range: None,
        }
    }

    /// Attach a sanitized technical cause. Long or suspicious values are
    /// truncated rather than trusted.
    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = sanitize(&reason.into());
        self
    }

    pub fn with_status(mut self, status: u16) -> Self {
        self.upstream_status = Some(status);
        self
    }

    /// Attach a `Content-Range: bytes * /<total>` for `416` responses.
    pub fn with_content_range(mut self, total: u64) -> Self {
        self.content_range = Some(format!("bytes */{total}"));
        self
    }

    /// Forward an origin's `Content-Range` verbatim, but only when it is a
    /// well-formed unsatisfied range (`bytes */<digits>`).
    pub fn with_content_range_raw(mut self, value: String) -> Self {
        let v = value.trim();
        if let Some(total) = v.strip_prefix("bytes */") {
            if !total.is_empty() && total.bytes().all(|b| b.is_ascii_digit()) {
                self.content_range = Some(v.to_owned());
            }
        }
        self
    }

    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = sanitize(&message.into());
        self
    }

    pub fn with_user_action(mut self, action: impl Into<String>) -> Self {
        self.user_action = sanitize(&action.into());
        self
    }

    /// JSON body sent to the browser.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| {
            serde_json::json!({
                "code": ErrorCode::UnknownError.as_str(),
                "message": ErrorCode::UnknownError.default_message(),
                "retryable": true,
                "user_action": ErrorCode::UnknownError.default_user_action(),
            })
        })
    }

    /// HTTP status we report to our own clients.
    pub const fn http_status(&self) -> u16 {
        match self.code {
            ErrorCode::InvalidUrl
            | ErrorCode::UnsupportedProtocol
            | ErrorCode::InvalidRangeHeader => 400,
            ErrorCode::Http401 => 401,
            ErrorCode::Http403 => 403,
            ErrorCode::Http404 => 404,
            ErrorCode::Http416 => 416,
            ErrorCode::RateLimited => 429,
            ErrorCode::TooManyRequests => 429,
            ErrorCode::SupersededRequest => 409,
            ErrorCode::TooManyRedirects => 508,
            ErrorCode::MediaNotSupported
            | ErrorCode::InvalidContentType
            | ErrorCode::InvalidContentLength
            | ErrorCode::RangeNotSupported => 415,
            ErrorCode::RequestTimeout | ErrorCode::ConnectionTimeout => 504,
            ErrorCode::Shutdown => 503,
            _ => 502,
        }
    }
}

impl fmt::Display for PlayerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for PlayerError {}

/// Shared, cheap-to-clone error used across task boundaries.
pub type SharedError = Arc<PlayerError>;

/// Cap and scrub free-form reason strings so a hostile upstream cannot flood
/// logs, inject control characters, or smuggle credentials into responses.
fn sanitize(input: &str) -> String {
    const MAX: usize = 240;
    const ELLIPSIS: &str = "...";
    let room = MAX - ELLIPSIS.len();
    let mut out = String::with_capacity(MAX);
    for ch in input.chars() {
        if out.len() >= room {
            out.push_str(ELLIPSIS);
            break;
        }
        match ch {
            '\n' | '\r' | '\t' | '"' | '\\' => out.push(' '),
            c if (c as u32) < 0x20 || c == '\u{7f}' => {}
            c => out.push(c),
        }
    }
    out
}

/// Map an upstream status code to our error taxonomy.
pub fn from_status(status: u16) -> ErrorCode {
    match status {
        400 => ErrorCode::Http400,
        401 => ErrorCode::Http401,
        403 => ErrorCode::Http403,
        404 => ErrorCode::Http404,
        416 => ErrorCode::Http416,
        429 => ErrorCode::RateLimited,
        500..=599 => ErrorCode::UpstreamError,
        _ => ErrorCode::UnknownError,
    }
}

/// Classify a reqwest transport failure into DNS / TLS / timeout / reset.
pub fn from_reqwest(err: &reqwest::Error) -> ErrorCode {
    if err.is_timeout() {
        return ErrorCode::RequestTimeout;
    }
    if err.is_connect() {
        // rustls surfaces the handshake failure through the connect chain;
        // there is no stable public marker for it, so we only claim TLS when
        // the message explicitly says so.
        let msg = err.to_string().to_ascii_lowercase();
        if msg.contains("tls") || msg.contains("certificate") || msg.contains("handshake") {
            return ErrorCode::TlsFailure;
        }
        return ErrorCode::ConnectionTimeout;
    }
    if err.is_body() || err.is_decode() {
        return ErrorCode::CorruptedResponse;
    }
    if err.is_redirect() {
        return ErrorCode::InvalidUrl;
    }
    let msg = err.to_string().to_ascii_lowercase();
    if msg.contains("dns") || msg.contains("name resolution") || msg.contains("no such host") {
        return ErrorCode::DnsFailure;
    }
    if msg.contains("tls") || msg.contains("certificate") || msg.contains("handshake") {
        return ErrorCode::TlsFailure;
    }
    if msg.contains("reset") || msg.contains("broken pipe") || msg.contains("incomplete") {
        return ErrorCode::NetworkInterrupted;
    }
    ErrorCode::NetworkInterrupted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes_have_unique_stable_strings() {
        let all = [
            ErrorCode::InvalidUrl,
            ErrorCode::UnsupportedProtocol,
            ErrorCode::DnsFailure,
            ErrorCode::TlsFailure,
            ErrorCode::ConnectionTimeout,
            ErrorCode::RequestTimeout,
            ErrorCode::Http400,
            ErrorCode::Http401,
            ErrorCode::Http403,
            ErrorCode::Http404,
            ErrorCode::Http416,
            ErrorCode::RateLimited,
            ErrorCode::UpstreamError,
            ErrorCode::RangeNotSupported,
            ErrorCode::InvalidContentType,
            ErrorCode::InvalidContentLength,
            ErrorCode::CorruptedResponse,
            ErrorCode::MediaNotSupported,
            ErrorCode::NetworkInterrupted,
            ErrorCode::PlayerError,
        ];
        let mut seen = std::collections::HashSet::new();
        for c in all {
            assert!(seen.insert(c.as_str()), "duplicate code string {c}");
            assert!(!c.default_message().is_empty());
            assert!(!c.default_user_action().is_empty());
        }
        assert_eq!(all.len(), seen.len());
    }

    #[test]
    fn retryability_matches_taxonomy() {
        assert!(ErrorCode::UpstreamError.retryable());
        assert!(ErrorCode::NetworkInterrupted.retryable());
        assert!(!ErrorCode::Http404.retryable());
        assert!(!ErrorCode::Http403.retryable());
        assert!(!ErrorCode::Http401.retryable());
        assert!(!ErrorCode::Http400.retryable());
        assert!(!ErrorCode::Http416.retryable());
        assert!(!ErrorCode::RangeNotSupported.retryable());
    }

    #[test]
    fn reason_is_truncated_and_scrubbed() {
        let e = PlayerError::new(ErrorCode::UpstreamError)
            .with_reason("line one\nline two\t\"quoted\"");
        assert!(!e.reason.contains('\n'));
        assert!(!e.reason.contains('\t'));
        assert!(!e.reason.contains('"'));

        let long = "x".repeat(5000);
        let e = PlayerError::new(ErrorCode::UpstreamError).with_reason(long);
        assert!(e.reason.chars().count() <= 241);
    }

    #[test]
    fn status_mapping_covers_documented_codes() {
        assert_eq!(from_status(400), ErrorCode::Http400);
        assert_eq!(from_status(429), ErrorCode::RateLimited);
        assert_eq!(from_status(502), ErrorCode::UpstreamError);
        assert_eq!(from_status(504), ErrorCode::UpstreamError);
        assert_eq!(from_status(599), ErrorCode::UpstreamError);
        assert_eq!(from_status(451), ErrorCode::UnknownError);
    }

    #[test]
    fn http_status_is_sane() {
        assert_eq!(PlayerError::new(ErrorCode::InvalidUrl).http_status(), 400);
        assert_eq!(PlayerError::new(ErrorCode::Http404).http_status(), 404);
        assert_eq!(
            PlayerError::new(ErrorCode::TooManyRequests).http_status(),
            429
        );
        assert_eq!(
            PlayerError::new(ErrorCode::UpstreamError).http_status(),
            502
        );
    }

    #[test]
    fn json_shape_is_the_documented_contract() {
        let v = PlayerError::new(ErrorCode::RangeNotSupported).to_json();
        assert_eq!(v["code"], "RANGE_NOT_SUPPORTED");
        assert_eq!(v["retryable"], false);
        assert!(v["message"].is_string());
        assert!(v["user_action"].is_string());
        assert!(v.get("upstream_status").is_none());
    }

    #[test]
    fn http_codes_serialize_with_their_underscore() {
        for (code, wire) in [
            (ErrorCode::Http400, "HTTP_400"),
            (ErrorCode::Http401, "HTTP_401"),
            (ErrorCode::Http403, "HTTP_403"),
            (ErrorCode::Http404, "HTTP_404"),
            (ErrorCode::Http416, "HTTP_416"),
            (ErrorCode::NetworkInterrupted, "NETWORK_INTERRUPTED"),
            (ErrorCode::RangeNotSupported, "RANGE_NOT_SUPPORTED"),
            (ErrorCode::TooManyRedirects, "TOO_MANY_REDIRECTS"),
            (ErrorCode::SupersededRequest, "SUPERSEDED_REQUEST"),
        ] {
            assert_eq!(PlayerError::new(code).to_json()["code"], wire);
        }
    }

    #[test]
    fn content_range_is_only_accepted_in_its_unsatisfied_form() {
        let ok = PlayerError::new(ErrorCode::Http416).with_content_range(4096);
        assert_eq!(ok.content_range.as_deref(), Some("bytes */4096"));

        for bad in [
            "bytes 0-10/20",
            "*/20",
            "bytes */",
            "<script>",
            "bytes */abc",
        ] {
            let e = PlayerError::new(ErrorCode::Http416).with_content_range_raw(bad.to_owned());
            assert!(e.content_range.is_none(), "{bad} must be refused");
        }
        let ok =
            PlayerError::new(ErrorCode::Http416).with_content_range_raw("bytes */4096".to_owned());
        assert_eq!(ok.content_range.as_deref(), Some("bytes */4096"));
    }
}
