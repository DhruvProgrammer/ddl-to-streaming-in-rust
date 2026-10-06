//! Media identification.
//!
//! A URL is a claim; the bytes are evidence. We read the response body first
//! and let it decide, because a valid DDL is routinely extensionless —
//! `/download/37334`, `/get?id=12345`, `/file?token=…` — and the extension is
//! both the least reliable signal and the one most likely to be a lie. The file
//! extension is consulted only when we have no body and no usable headers, and
//! it can never rescue a body that was read and did not look like media.

use std::sync::OnceLock;

/// Containers we recognise. `Unknown` means "we looked and it was not media",
/// which is a different statement from "we could not tell".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Mov,
    WebM,
    Matroska,
    Ogg,
    Mp3,
    Wav,
    MpegTs,
    Hls,
    Flv,
    Avi,
    /// The origin returned a web page: an expired link, a login wall, an error.
    Html,
    /// The origin returned a JSON error envelope behind a 200.
    Json,
    /// The body arrived content-encoded, so its bytes are not the media.
    Compressed,
    /// The origin returned no body at all.
    Empty,
    Unknown,
}

impl Container {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::WebM => "webm",
            Self::Matroska => "matroska",
            Self::Ogg => "ogg",
            Self::Mp3 => "mp3",
            Self::Wav => "wav",
            Self::MpegTs => "mpegts",
            Self::Hls => "hls",
            Self::Flv => "flv",
            Self::Avi => "avi",
            Self::Html => "html",
            Self::Json => "json",
            Self::Compressed => "compressed",
            Self::Empty => "empty",
            Self::Unknown => "unknown",
        }
    }

    /// Media type to send downstream. For the pseudo-containers this is the
    /// type we actually received, not something to pass off as media.
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Mp4 => "video/mp4",
            Self::Mov => "video/quicktime",
            Self::WebM => "video/webm",
            Self::Matroska => "video/x-matroska",
            Self::Ogg => "video/ogg",
            Self::Mp3 => "audio/mpeg",
            Self::Wav => "audio/wav",
            Self::MpegTs => "video/mp2t",
            Self::Hls => "application/vnd.apple.mpegurl",
            Self::Flv => "video/x-flv",
            Self::Avi => "video/x-msvideo",
            Self::Html => "text/html",
            Self::Json => "application/json",
            Self::Compressed | Self::Empty | Self::Unknown => "application/octet-stream",
        }
    }

    /// Whether `<video src>` in a mainstream browser can play this directly.
    ///
    /// HLS is excluded deliberately: serving the manifest is easy, but the
    /// segment requests that follow would go straight to the origin, bypassing
    /// the proxy — which breaks signed and temporary links outright. Reporting
    /// it as playable and then failing is worse than saying so up front.
    pub const fn browser_native(self) -> bool {
        matches!(
            self,
            Self::Mp4 | Self::WebM | Self::Ogg | Self::Mov | Self::Mp3 | Self::Wav
        )
    }

    /// True when this verdict came from reading the body, so it is decisive.
    pub const fn is_definitive(self) -> bool {
        !matches!(self, Self::Unknown | Self::Empty)
    }
}

/// What we believe the resource is, and how we learned it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaInfo {
    pub container: Container,
    pub media_type: String,
    pub source: Evidence,
    /// Set when the resource is valid media but not browser-playable as-is.
    pub needs_remux: bool,
    pub remux_reason: Option<String>,
}

/// How we know. Ordered strongest-first; the string form is what the probe
/// reports, so it should name the evidence a person could check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    MagicBytes,
    ContentType,
    ContentDisposition,
    Extension,
    Unknown,
}

impl Evidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MagicBytes => "magic-bytes",
            Self::ContentType => "content-type",
            Self::ContentDisposition => "content-disposition",
            Self::Extension => "extension",
            Self::Unknown => "unknown",
        }
    }
}

/// Maps a Content-Type header to a container. `None` means unrecognised.
pub fn from_content_type(raw: &str) -> Option<Container> {
    let base = base_type(raw);
    let c = match base.as_str() {
        "video/mp4" | "audio/mp4" | "application/mp4" | "video/x-m4v" | "audio/x-m4a" => {
            Container::Mp4
        }
        "video/quicktime" => Container::Mov,
        "video/webm" | "audio/webm" => Container::WebM,
        "video/x-matroska" | "audio/x-matroska" | "application/x-matroska" => Container::Matroska,
        "video/ogg" | "audio/ogg" | "application/ogg" => Container::Ogg,
        "audio/mpeg" | "audio/mp3" | "audio/x-mpeg" | "audio/x-mp3" => Container::Mp3,
        "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => Container::Wav,
        "video/mp2t" | "video/mpeg" | "application/x-mpeg" => Container::MpegTs,
        "application/vnd.apple.mpegurl"
        | "application/x-mpegurl"
        | "audio/mpegurl"
        | "audio/x-mpegurl" => Container::Hls,
        "video/x-flv" | "application/x-flv" => Container::Flv,
        "video/x-msvideo" => Container::Avi,
        "text/html" | "application/xhtml+xml" => Container::Html,
        "application/json" | "text/json" => Container::Json,
        _ => return None,
    };
    Some(c)
}

/// Maps a filename to a container. Used for the URL path *and* for a
/// `Content-Disposition` filename, which is origin-supplied evidence rather
/// than something the user typed.
pub fn from_filename(name: &str) -> Option<Container> {
    let ext = name.rsplit('.').next()?.to_ascii_lowercase();
    let c = match ext.as_str() {
        "mp4" | "m4v" | "m4a" | "mp4v" => Container::Mp4,
        "mov" | "qt" => Container::Mov,
        "webm" => Container::WebM,
        "mkv" => Container::Matroska,
        "ogv" | "ogg" | "oga" => Container::Ogg,
        "mp3" => Container::Mp3,
        "ts" | "m2ts" | "mts" => Container::MpegTs,
        "m3u8" => Container::Hls,
        "flv" => Container::Flv,
        "avi" => Container::Avi,
        "wav" => Container::Wav,
        _ => return None,
    };
    Some(c)
}

/// Maps a URL path to a container. Last resort only — see the module docs.
pub fn from_extension(path: &str) -> Option<Container> {
    from_filename(path)
}

/// The path of a URL, and nothing else.
///
/// A signed link routinely carries its token in the query string, so anything
/// that derives a container from a URL must go through here and must never see
/// `?id=`, `?token=` or `?sig=`.
pub fn url_path(url: &url::Url) -> &str {
    url.path()
}

/// Lowercased media type with parameters stripped.
fn base_type(raw: &str) -> String {
    raw.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// Container signature detection from the first bytes of the body.
///
/// This is the only evidence strong enough to override a declared type, in
/// either direction: an origin that claims `video/mp4` over an HTML error page
/// is lying, and so is one that claims `text/html` over a real MP4.
pub fn sniff(head: &[u8]) -> Container {
    if head.is_empty() {
        return Container::Empty;
    }

    // Fixed signatures first: unambiguous and cheap.
    if head.starts_with(b"OggS") {
        return Container::Ogg;
    }
    if head.starts_with(b"FLV\x01") {
        return Container::Flv;
    }
    if head.starts_with(b"\x1a\x45\xdf\xa3") {
        return sniff_ebml(head);
    }
    if head.len() >= 12 && head.starts_with(b"RIFF") {
        return match &head[8..12] {
            b"AVI " => Container::Avi,
            b"WAVE" => Container::Wav,
            _ => Container::Unknown,
        };
    }
    if head.starts_with(b"ID3") {
        return Container::Mp3;
    }

    // Text-ish payloads: an expired link or a login wall behind a 200.
    if let Some(c) = sniff_text(head) {
        return c;
    }

    // MPEG audio frame sync, only where a layer/version byte confirms it.
    if head.len() >= 2 && head[0] == 0xff && (head[1] & 0xe0) == 0xe0 && (head[1] & 0x06) != 0x00 {
        return Container::Mp3;
    }

    // MPEG-TS: 188-byte packets starting with the 0x47 sync byte. Two syncs
    // are required so a stray 0x47 in arbitrary data cannot trigger it.
    if head.len() >= 189 && head[0] == 0x47 && head[188] == 0x47 {
        return Container::MpegTs;
    }

    sniff_isobmff(head)
}

/// EBML is shared by WebM and Matroska; `DocType` is what separates them.
/// Reporting a Matroska file as `video/webm` is a lie the browser acts on.
fn sniff_ebml(head: &[u8]) -> Container {
    // DocType is a short string element, so it appears verbatim in the head.
    if find(head, b"matroska").is_some() {
        return Container::Matroska;
    }
    if find(head, b"webm").is_some() {
        return Container::WebM;
    }
    // EBML header present but DocType beyond our window: WebM is the subset
    // browsers can play, so it is the safer of the two to claim.
    Container::WebM
}

/// HTML, JSON or gzip, or `None` if the bytes are not recognisably one.
fn sniff_text(head: &[u8]) -> Option<Container> {
    // Content-encoded bodies: we asked for identity and were refused. The bytes
    // are not the media, so nothing further can be concluded from them.
    if head.starts_with(&[0x1f, 0x8b]) {
        return Some(Container::Compressed);
    }
    if head.starts_with(b"PK\x03\x04") {
        return None;
    }

    let window = &head[..head.len().min(512)];
    let trimmed = skip_ws_and_bom(window);
    let lower = trimmed.to_ascii_lowercase();
    if lower.starts_with(b"<!doctype html")
        || lower.starts_with(b"<html")
        || lower.starts_with(b"<head")
        || lower.starts_with(b"<?xml")
        || lower.starts_with(b"<script")
    {
        return Some(Container::Html);
    }
    if trimmed.starts_with(b"{") || trimmed.starts_with(b"[") {
        // Only trust this if it parses as a JSON object/array opener rather
        // than being the first byte of some binary format.
        if looks_like_json(trimmed) {
            return Some(Container::Json);
        }
    }
    if window.starts_with(b"#EXTM3U") {
        return Some(Container::Hls);
    }
    None
}

fn skip_ws_and_bom(b: &[u8]) -> &[u8] {
    let mut i = 0;
    // UTF-8 BOM.
    if b.len() >= 3 && b[..3] == [0xef, 0xbb, 0xbf] {
        i = 3;
    }
    while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n' | 0x0b | 0x0c) {
        i += 1;
    }
    &b[i..]
}

/// Cheap structural check: balanced-ish braces/quotes, no NUL bytes. A binary
/// blob that happens to start with `{` will not survive this.
fn looks_like_json(b: &[u8]) -> bool {
    let end = b.len().min(4096);
    let window = &b[..end];
    if window.contains(&0u8) {
        return false;
    }
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for &c in window {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth -= 1;
                if depth == 0 {
                    return true;
                }
                if depth < 0 {
                    return false;
                }
            }
            // Anything outside a string that cannot appear in JSON text.
            0x00..=0x08 | 0x0e..=0x1f => return false,
            _ => {}
        }
    }
    false
}

/// ISO base media file format: walk the box chain looking for `ftyp`/`styp`.
///
/// A conforming file may open with `free`, `skip`, `wide`, `moov` or `uuid`, so
/// testing offset 4 alone misses real files — and misses them exactly when the
/// URL is extensionless and there is nothing else to fall back on.
fn sniff_isobmff(head: &[u8]) -> Container {
    let mut off = 0usize;
    // Bounded: enough for any real file's preamble, and never a scan loop.
    for _ in 0..8 {
        if head.len() < off + 8 {
            return Container::Unknown;
        }
        let size = u32::from_be_bytes([head[off], head[off + 1], head[off + 2], head[off + 3]]);
        let btype = &head[off + 4..off + 8];

        match btype {
            b"ftyp" | b"styp" => {
                // The brand must be complete before we claim a container; a
                // truncated header is not evidence of anything.
                if head.len() < off + 12 {
                    return Container::Unknown;
                }
                return if &head[off + 8..off + 12] == b"qt  " {
                    Container::Mov
                } else {
                    Container::Mp4
                };
            }
            // Boxes that legitimately precede `ftyp`.
            b"moov" | b"moof" | b"mdat" | b"free" | b"skip" | b"wide" | b"uuid" => {}
            _ => return Container::Unknown,
        }

        let step = match size {
            // 0 means "to end of file".
            0 => return Container::Unknown,
            // 1 means the real size is a 64-bit field that follows the type.
            1 => {
                if head.len() < off + 16 {
                    return Container::Unknown;
                }
                let mut w = [0u8; 8];
                w.copy_from_slice(&head[off + 8..off + 16]);
                u64::from_be_bytes(w)
            }
            n => n as u64,
        };
        // Any size below one header cannot advance us; refuse rather than spin.
        if step < 8 {
            return Container::Unknown;
        }
        off += step as usize;
        if off >= head.len() {
            return Container::Unknown;
        }
    }
    Container::Unknown
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

static GENERIC_TYPES: OnceLock<Vec<String>> = OnceLock::new();

/// Generic download types we never trust and always re-derive from the body.
pub fn is_generic_content_type(raw: &str) -> bool {
    GENERIC_TYPES
        .get_or_init(|| {
            [
                "application/octet-stream",
                "binary/octet-stream",
                "application/download",
                "application/force-download",
                "application/x-download",
                "",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
        })
        .iter()
        .any(|g| *g == base_type(raw))
}

fn finalize(container: Container, source: Evidence) -> MediaInfo {
    let needs_remux = !container.browser_native();
    let remux_reason = needs_remux.then(|| match container {
        Container::Matroska => {
            "Matroska is not directly playable; remux to MP4 or WebM first.".to_owned()
        }
        Container::MpegTs => {
            "MPEG-TS is a broadcast container; browsers need it repackaged into MP4 or WebM."
                .to_owned()
        }
        Container::Flv => "FLV is not a browser-playable container.".to_owned(),
        Container::Avi => "AVI is not a browser-playable container.".to_owned(),
        Container::Hls => {
            "This is an HLS manifest. Its segments would be fetched from the origin \
             directly, outside the proxy, so it cannot be played here."
                .to_owned()
        }
        Container::Html => {
            "The origin returned a web page rather than a video. The link may have \
             expired, or may require signing in."
                .to_owned()
        }
        Container::Json => {
            "The origin returned a JSON response rather than a video. The link may \
             have expired."
                .to_owned()
        }
        Container::Compressed => {
            "The origin compressed the response, so it cannot be inspected or seeked. \
             This link cannot be streamed as-is."
                .to_owned()
        }
        Container::Empty => "The origin returned an empty body; there is no video here.".to_owned(),
        _ => "The detected container is not directly playable in this browser.".to_owned(),
    });
    MediaInfo {
        container,
        media_type: container.media_type().to_owned(),
        source,
        needs_remux,
        remux_reason,
    }
}

/// Identify a resource from the strongest evidence available.
///
/// `head` is the body prefix, if one was read. Its presence changes everything:
/// bytes are ground truth, so when they were read and did not look like media,
/// the URL is not allowed to overrule them. `filename` is a
/// `Content-Disposition` filename — origin-supplied, unlike the URL, and
/// therefore evidence rather than a claim.
pub fn identify(
    content_type: Option<&str>,
    path: &str,
    head: Option<&[u8]>,
    filename: Option<&str>,
) -> MediaInfo {
    // 1. Bytes, when we have them. Decisive in both directions.
    let read_body = head.is_some();
    if let Some(head) = head {
        if !head.is_empty() {
            let c = sniff(head);
            if c.is_definitive() {
                return finalize(c, Evidence::MagicBytes);
            }
        }
    }

    // 2. A Content-Type we recognise. Reaching here means the body either was
    //    not read, or was read and was inconclusive — so the origin's word is
    //    the best evidence left. A decisive body already returned above.
    if let Some(raw) = content_type {
        if !is_generic_content_type(raw) {
            if let Some(c) = from_content_type(raw) {
                return finalize(c, Evidence::ContentType);
            }
            let base = base_type(raw);
            if !base.starts_with("video/") && !base.starts_with("audio/") {
                // The origin declared something that is not media at all.
                return finalize(Container::Unknown, Evidence::ContentType);
            }
            // An unfamiliar media type. Pass it through, but do not claim the
            // browser will manage it.
            return MediaInfo {
                container: Container::Unknown,
                media_type: base,
                source: Evidence::ContentType,
                needs_remux: true,
                remux_reason: Some(
                    "Unrecognised media type; the browser may refuse to play it.".to_owned(),
                ),
            };
        }
    }

    // 3. A Content-Disposition filename: the origin telling us what it is
    //    serving, which is evidence rather than something the user typed.
    if let Some(name) = filename {
        if let Some(c) = from_filename(name) {
            return finalize(c, Evidence::ContentDisposition);
        }
    }

    // 4. The URL — last, and only when no body was read. A body that was read
    //    and did not identify as media is a negative result, and the extension
    //    must not rescue it.
    if !read_body {
        if let Some(c) = from_extension(path) {
            return finalize(c, Evidence::Extension);
        }
    }

    if head.is_some() {
        // We have body evidence and it was inconclusive or empty. Say which.
        if head.map_or(false, <[u8]>::is_empty) {
            return finalize(Container::Empty, Evidence::MagicBytes);
        }
        return finalize(Container::Unknown, Evidence::MagicBytes);
    }
    finalize(Container::Unknown, Evidence::Unknown)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mp4_head() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&24u32.to_be_bytes());
        v.extend_from_slice(b"ftypisom");
        v.extend_from_slice(&[0u8; 32]);
        v
    }

    fn box_head(ty: &[u8; 4], then: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&8u32.to_be_bytes());
        v.extend_from_slice(ty);
        v.extend_from_slice(then);
        v.resize(64, 0);
        v
    }

    const HTML: &[u8] = b"<!DOCTYPE html><html><body>404 not found</body></html>";

    #[test]
    fn declared_type_loses_to_decisive_bytes() {
        // The origin claims WebM and serves an MP4. The bytes know better.
        let i = identify(Some("video/webm"), "/x", Some(&mp4_head()), None);
        assert_eq!(i.container, Container::Mp4);
        assert_eq!(i.source, Evidence::MagicBytes);
    }

    #[test]
    fn generic_type_falls_through_to_magic() {
        for generic in [
            "application/octet-stream",
            "binary/octet-stream",
            "application/download",
            "",
        ] {
            let i = identify(Some(generic), "/x", Some(&mp4_head()), None);
            assert_eq!(i.container, Container::Mp4, "{generic:?}");
            assert_eq!(i.source, Evidence::MagicBytes);
        }
    }

    #[test]
    fn declared_type_is_used_when_the_body_says_nothing() {
        // A recognised type plus an unidentifiable body: keep the origin's word.
        let i = identify(Some("video/mp4"), "/x", Some(&[0u8; 6]), None);
        assert_eq!(i.container, Container::Mp4);
        assert_eq!(i.source, Evidence::ContentType);
    }

    #[test]
    fn magic_bytes_beat_lying_extension() {
        let mut webm = vec![0x1a, 0x45, 0xdf, 0xa3];
        webm.extend_from_slice(b"webm");
        webm.extend_from_slice(&[0u8; 60]);
        let i = identify(Some("application/octet-stream"), "/movie.mp4", Some(&webm), None);
        assert_eq!(i.container, Container::WebM);
        assert_eq!(i.source, Evidence::MagicBytes);
    }

    #[test]
    fn extension_is_the_last_resort_and_only_without_a_body() {
        let i = identify(Some("application/octet-stream"), "/movie.mp4", None, None);
        assert_eq!(i.container, Container::Mp4);
        assert_eq!(i.source, Evidence::Extension);
        let i = identify(None, "/movie.webm", None, None);
        assert_eq!(i.container, Container::WebM);
    }

    #[test]
    fn a_body_never_lets_the_extension_rescue_it() {
        // The headline case: HTML behind a .mp4 path must not become MP4.
        for ct in [Some("application/octet-stream"), None] {
            let i = identify(ct, "/movie.mp4", Some(HTML), None);
            assert_eq!(i.container, Container::Html, "{ct:?}");
            assert!(!i.container.browser_native());
        }
        // An empty body is a negative result too.
        let i = identify(Some("application/octet-stream"), "/movie.mp4", Some(&[]), None);
        assert_eq!(i.container, Container::Empty);
        assert!(!i.container.browser_native());
        // A truncated body is not evidence of media.
        let i = identify(Some("application/octet-stream"), "/movie.mp4", Some(&[0, 0, 0x18]), None);
        assert_eq!(i.container, Container::Unknown);
        assert!(!i.container.browser_native());
    }

    #[test]
    fn a_lying_type_cannot_rescue_an_html_body() {
        // `video/mp4` over an error page is the common expired-link shape.
        let i = identify(Some("video/mp4"), "/download/37334", Some(HTML), None);
        assert_eq!(i.container, Container::Html);
        assert!(!i.container.browser_native());
    }

    #[test]
    fn content_disposition_filename_is_evidence() {
        let i = identify(
            Some("application/octet-stream"),
            "/download/A_SLu4ViVaRA1e51eV8yNiRGIQpB4_D25-okgLU5OxlphDIfGg",
            None,
            Some("Movie.2024.1080p.BluRay.mp4"),
        );
        assert_eq!(i.container, Container::Mp4);
        assert_eq!(i.source, Evidence::ContentDisposition);
    }

    #[test]
    fn bytes_beat_a_content_disposition_filename() {
        let i = identify(
            Some("application/octet-stream"),
            "/d/1",
            Some(&mp4_head()),
            Some("clip.webm"),
        );
        assert_eq!(i.container, Container::Mp4);
        assert_eq!(i.source, Evidence::MagicBytes);
    }

    #[test]
    fn mp4_quicktime_brand_is_distinguished() {
        let mut v = Vec::new();
        v.extend_from_slice(&16u32.to_be_bytes());
        v.extend_from_slice(b"ftypqt  ");
        assert_eq!(sniff(&v), Container::Mov);
        assert_eq!(sniff(&mp4_head()), Container::Mp4);
    }

    #[test]
    fn ftyp_is_found_after_a_legal_preamble() {
        // free / skip / wide / moov may all precede ftyp.
        for ty in [b"free", b"skip", b"wide", b"moov"] {
            let head = box_head(ty, &{
                let mut v = 24u32.to_be_bytes().to_vec();
                v.extend_from_slice(b"ftypisom");
                v
            });
            assert_eq!(sniff(&head), Container::Mp4, "{ty:?}");
            let i = identify(Some("application/octet-stream"), "/d/37334", Some(&head), None);
            assert!(i.container.browser_native(), "{ty:?}");
        }
    }

    #[test]
    fn a_truncated_ftyp_header_is_not_an_mp4() {
        // 8 bytes: size + type, but no brand to read.
        let mut eight = 24u32.to_be_bytes().to_vec();
        eight.extend_from_slice(b"ftyp");
        assert_eq!(sniff(&eight), Container::Unknown);
        let mut eleven = 24u32.to_be_bytes().to_vec();
        eleven.extend_from_slice(b"ftypiso");
        assert_eq!(sniff(&eleven), Container::Unknown);
    }

    #[test]
    fn matroska_and_webm_are_different_containers() {
        let mut mkv = vec![0x1a, 0x45, 0xdf, 0xa3];
        mkv.extend_from_slice(b"matroska");
        assert_eq!(sniff(&mkv), Container::Matroska);
        let mut webm = vec![0x1a, 0x45, 0xdf, 0xa3];
        webm.extend_from_slice(b"webm");
        assert_eq!(sniff(&webm), Container::WebM);
        // No mainstream browser plays Matroska, so it must not be claimed.
        assert!(!Container::Matroska.browser_native());
        assert!(from_content_type("video/x-matroska").is_some());
    }

    #[test]
    fn hls_is_not_claimed_as_playable() {
        let m3u8 = b"#EXTM3U\n#EXT-X-VERSION:3\n";
        assert_eq!(sniff(m3u8), Container::Hls);
        let i = identify(None, "/live/stream", Some(m3u8), None);
        assert_eq!(i.container, Container::Hls);
        assert!(!i.container.browser_native());
        assert!(i.needs_remux);
    }

    #[test]
    fn ts_sync_detection_needs_two_packets() {
        let mut ts = vec![0x47u8; 189];
        ts[0] = 0x47;
        ts[188] = 0x47;
        assert_eq!(sniff(&ts), Container::MpegTs);
        let mut not_ts = vec![0u8; 300];
        not_ts[7] = 0x47;
        assert_eq!(sniff(&not_ts), Container::Unknown);
        assert_eq!(sniff(&[]), Container::Empty);
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n"), Container::Unknown);
    }

    #[test]
    fn non_browser_containers_are_flagged_for_remux() {
        let mut ts = vec![0x47u8; 189];
        ts[188] = 0x47;
        let i = identify(Some("video/mp2t"), "/x.ts", Some(&ts), None);
        assert!(i.needs_remux);
        assert!(i.remux_reason.unwrap().contains("MP4"));

        let i = identify(Some("video/x-flv"), "/x.flv", None, None);
        assert!(i.needs_remux);

        for c in [
            Container::Mp4,
            Container::WebM,
            Container::Ogg,
            Container::Mov,
            Container::Mp3,
            Container::Wav,
        ] {
            assert!(c.browser_native(), "{c:?} should be native");
        }
        assert!(!identify(Some("video/mp4"), "/a.mp4", None, None).needs_remux);
    }

    #[test]
    fn unknown_vendor_video_type_is_not_played_blindly() {
        let i = identify(Some("video/x-totally-unknown"), "/a.bin", None, None);
        assert_eq!(i.media_type, "video/x-totally-unknown");
        assert!(i.needs_remux);
    }

    #[test]
    fn html_error_page_is_never_mistaken_for_media() {
        let i = identify(Some("text/html; charset=utf-8"), "/a.mp4", Some(HTML), None);
        assert_eq!(i.container, Container::Html);
        assert!(!i.container.browser_native());
        assert_eq!(i.media_type, "text/html");
        let r = i.remux_reason.unwrap().to_lowercase();
        assert!(r.contains("web page"), "{r}");
    }

    #[test]
    fn a_gzipped_body_is_named_as_such() {
        let mut gz = vec![0x1f, 0x8b, 0x08, 0x00];
        gz.extend_from_slice(&[0u8; 32]);
        let i = identify(Some("application/octet-stream"), "/g/1", Some(&gz), None);
        assert_eq!(i.container, Container::Compressed);
        let r = i.remux_reason.unwrap().to_lowercase();
        assert!(r.contains("compress"), "{r}");
    }

    #[test]
    fn a_json_error_envelope_is_named_as_such() {
        let body = br#"{"error":"link_expired","code":410}"#;
        let i = identify(Some("application/octet-stream"), "/f?token=x", Some(body), None);
        assert_eq!(i.container, Container::Json);
        assert!(!i.container.browser_native());
    }

    #[test]
    fn signature_table_is_complete() {
        let cases: Vec<(Container, &[u8])> = vec![
            (Container::Ogg, b"OggS\x00\x02"),
            (Container::Flv, b"FLV\x01\x05"),
            (Container::Avi, b"RIFF\x20\x00\x00\x00AVI LIST"),
            (Container::Wav, b"RIFF\x20\x00\x00\x00WAVEfmt "),
            (Container::Mp3, b"ID3\x04\x00\x00\x00\x00\x00\x00"),
        ];
        for (want, bytes) in cases {
            assert_eq!(sniff(bytes), want, "{want:?}");
        }
    }

    #[test]
    fn generic_type_helper() {
        assert!(is_generic_content_type("application/octet-stream"));
        assert!(is_generic_content_type(
            "APPLICATION/Octet-Stream; charset=binary"
        ));
        assert!(!is_generic_content_type("video/mp4"));
    }
}
