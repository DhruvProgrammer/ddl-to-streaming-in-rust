//! Media identification.
//!
//! A `.mp4` URL is a claim, not evidence. We triangulate Content-Type, file
//! extension, and container magic bytes, in that order of decreasing trust,
//! and we refuse to hand a `<video>` element something no browser can decode.

use std::sync::OnceLock;

/// Containers we can serve straight to a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    Mp4,
    Mov,
    WebM,
    Ogg,
    MpegTs,
    Hls,
    Flv,
    Avi,
    Wav,
    Unknown,
}

impl Container {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::WebM => "webm",
            Self::Ogg => "ogg",
            Self::MpegTs => "mpegts",
            Self::Hls => "hls",
            Self::Flv => "flv",
            Self::Avi => "avi",
            Self::Wav => "wav",
            Self::Unknown => "unknown",
        }
    }

    /// Media type to send downstream.
    pub const fn media_type(self) -> &'static str {
        match self {
            Self::Mp4 => "video/mp4",
            Self::Mov => "video/quicktime",
            Self::WebM => "video/webm",
            Self::Ogg => "video/ogg",
            Self::MpegTs => "video/mp2t",
            Self::Hls => "application/vnd.apple.mpegurl",
            Self::Flv => "video/x-flv",
            Self::Avi => "video/x-msvideo",
            Self::Wav => "audio/wav",
            Self::Unknown => "application/octet-stream",
        }
    }

    /// Whether `<video src>` in a mainstream browser can play this directly.
    pub const fn browser_native(self) -> bool {
        matches!(
            self,
            Self::Mp4 | Self::WebM | Self::Ogg | Self::Mov | Self::Hls
        )
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    ContentType,
    MagicBytes,
    Extension,
    Unknown,
}

impl Evidence {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContentType => "content-type",
            Self::MagicBytes => "magic-bytes",
            Self::Extension => "extension",
            Self::Unknown => "unknown",
        }
    }
}

/// Maps a Content-Type header to a container. `None` means unrecognised.
pub fn from_content_type(raw: &str) -> Option<Container> {
    let base = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let c = match base.as_str() {
        "video/mp4" | "audio/mp4" | "application/mp4" | "video/x-m4v" => Container::Mp4,
        "video/quicktime" => Container::Mov,
        "video/webm" | "audio/webm" => Container::WebM,
        "video/ogg" | "audio/ogg" | "application/ogg" => Container::Ogg,
        "video/mp2t" | "video/mpeg" | "application/x-mpeg" => Container::MpegTs,
        "application/vnd.apple.mpegurl"
        | "application/x-mpegurl"
        | "audio/mpegurl"
        | "audio/x-mpegurl" => Container::Hls,
        "video/x-flv" | "application/x-flv" => Container::Flv,
        "video/x-msvideo" => Container::Avi,
        "audio/wav" | "audio/x-wav" | "audio/wave" => Container::Wav,
        _ => return None,
    };
    Some(c)
}

/// Maps a URL path extension to a container.
pub fn from_extension(path: &str) -> Option<Container> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    let c = match ext.as_str() {
        "mp4" | "m4v" | "m4a" | "mp4v" => Container::Mp4,
        "mov" | "qt" => Container::Mov,
        "webm" => Container::WebM,
        "ogv" | "ogg" | "oga" => Container::Ogg,
        "ts" | "m2ts" | "mts" => Container::MpegTs,
        "m3u8" => Container::Hls,
        "flv" => Container::Flv,
        "avi" => Container::Avi,
        "wav" => Container::Wav,
        _ => return None,
    };
    Some(c)
}

/// Container signature detection. Needs at most 64 bytes.
pub fn sniff(head: &[u8]) -> Container {
    if head.len() >= 4 && &head[..4] == b"OggS" {
        return Container::Ogg;
    }
    if head.len() >= 4 && &head[..4] == b"FLV\x01" {
        return Container::Flv;
    }
    if head.len() >= 4 && &head[..4] == b"\x1a\x45\xdf\xa3" {
        return Container::WebM;
    }
    if head.len() >= 4 && &head[..4] == b"RIFF" && head.len() >= 12 {
        match &head[8..12] {
            b"AVI " => return Container::Avi,
            b"WAVE" => return Container::Wav,
            _ => {}
        }
    }
    if head.len() >= 8 && &head[4..8] == b"ftyp" {
        let brand = &head[8..12.min(head.len())];
        return match brand {
            b"qt  " => Container::Mov,
            _ => Container::Mp4,
        };
    }
    // HLS manifests are plain text.
    if head.starts_with(b"#EXTM3U") {
        return Container::Hls;
    }
    // MPEG-TS: 188-byte packets starting with the 0x47 sync byte. Check
    // offsets 0, 188 and 376 to avoid a false positive on arbitrary data.
    if head.len() >= 189 && head[0] == 0x47 && head[188] == 0x47 {
        return Container::MpegTs;
    }
    if head.len() >= 377 && head[0] == 0x47 && head[188] == 0x47 && head[376] == 0x47 {
        return Container::MpegTs;
    }
    // MPEG audio frame sync (0xFF Ex/Fx) is not reliably distinguishable at
    // 64 bytes; leave it unclassified rather than guess.
    Container::Unknown
}

/// Path of a URL, extension only — never the query string.
pub fn url_path(url: &url::Url) -> &str {
    url.path()
}

fn finalize(container: Container, source: Evidence) -> MediaInfo {
    let needs_remux = !container.browser_native();
    let remux_reason = needs_remux.then(|| match container {
        Container::MpegTs => {
            "MPEG-TS is a broadcast container; browsers need it repackaged into MP4 or WebM."
                .to_owned()
        }
        Container::Flv => "FLV is not a browser-playable container.".to_owned(),
        Container::Avi => "AVI is not a browser-playable container.".to_owned(),
        Container::Wav => "This audio-only WAV is not exposed for inline playback.".to_owned(),
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

/// Content-Type is authoritative when recognised; magic bytes beat a
/// missing or generic type; extension is the last resort — but only when the
/// origin did not tell us something specific and non-media.
pub fn identify(content_type: Option<&str>, path: &str, head: Option<&[u8]>) -> MediaInfo {
    if let Some(raw) = content_type {
        let base = raw
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let generic = base.is_empty()
            || matches!(
                base.as_str(),
                "application/octet-stream"
                    | "binary/octet-stream"
                    | "application/download"
                    | "application/force-download"
                    | "application/x-download"
            );
        if !generic {
            if let Some(c) = from_content_type(raw) {
                return finalize(c, Evidence::ContentType);
            }
            if !base.starts_with("video/") && !base.starts_with("audio/") {
                // The origin declared something that is not media at all — an
                // HTML error page, a JSON blob, a login form. Never let the
                // file extension override that.
                return finalize(Container::Unknown, Evidence::ContentType);
            }
            // An unfamiliar video/* or audio/* type: pass it through, but flag
            // that the browser may refuse it.
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
    if let Some(head) = head {
        let c = sniff(head);
        if c != Container::Unknown {
            return finalize(c, Evidence::MagicBytes);
        }
    }
    if let Some(c) = from_extension(path) {
        return finalize(c, Evidence::Extension);
    }
    if let Some(raw) = content_type {
        let base = raw
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        if base.starts_with("video/") || base.starts_with("audio/") {
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
    finalize(Container::Unknown, Evidence::Unknown)
}

static GENERIC_TYPES: OnceLock<Vec<String>> = OnceLock::new();

/// Generic download types we never trust and always re-derive.
pub fn is_generic_content_type(raw: &str) -> bool {
    let base = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    GENERIC_TYPES
        .get_or_init(|| {
            vec![
                "application/octet-stream".to_owned(),
                "binary/octet-stream".to_owned(),
                "application/download".to_owned(),
                "application/force-download".to_owned(),
                "application/x-download".to_owned(),
                "".to_owned(),
            ]
        })
        .iter()
        .any(|g| g == &base)
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

    #[test]
    fn content_type_wins_when_recognised() {
        let i = identify(Some("video/webm"), "/x.mp4", Some(&mp4_head()));
        assert_eq!(i.container, Container::WebM);
        assert_eq!(i.source, Evidence::ContentType);
    }

    #[test]
    fn generic_type_falls_through_to_magic() {
        for generic in [
            "application/octet-stream",
            "binary/octet-stream",
            "application/download",
            "",
        ] {
            let i = identify(Some(generic), "/x.mp4", Some(&mp4_head()));
            assert_eq!(i.container, Container::Mp4, "{generic:?}");
            assert_eq!(i.source, Evidence::MagicBytes);
        }
    }

    #[test]
    fn magic_bytes_beat_lying_extension() {
        // URL says .mp4, bytes say WebM.
        let mut webm = vec![0x1a, 0x45, 0xdf, 0xa3];
        webm.extend_from_slice(&[0u8; 60]);
        let i = identify(Some("application/octet-stream"), "/movie.mp4", Some(&webm));
        assert_eq!(i.container, Container::WebM);
        assert_eq!(i.source, Evidence::MagicBytes);
    }

    #[test]
    fn extension_is_the_last_resort() {
        let i = identify(Some("application/octet-stream"), "/movie.mp4", None);
        assert_eq!(i.container, Container::Mp4);
        assert_eq!(i.source, Evidence::Extension);
        let i = identify(None, "/movie.webm", None);
        assert_eq!(i.container, Container::WebM);
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
    fn ts_sync_detection_needs_two_packets() {
        let mut ts = vec![0x47u8; 189];
        ts[0] = 0x47;
        ts[188] = 0x47;
        assert_eq!(sniff(&ts), Container::MpegTs);
        // A single stray 0x47 must not be mistaken for MPEG-TS.
        let mut not_ts = vec![0u8; 300];
        not_ts[7] = 0x47;
        assert_eq!(sniff(&not_ts), Container::Unknown);
        assert_eq!(sniff(&[]), Container::Unknown);
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n"), Container::Unknown);
    }

    #[test]
    fn hls_manifests_are_detected() {
        let m3u8 = b"#EXTM3U\n#EXT-X-VERSION:3\n";
        assert_eq!(sniff(m3u8), Container::Hls);
        let i = identify(None, "/live/stream", Some(m3u8));
        assert_eq!(i.container, Container::Hls);
        assert!(!i.needs_remux);
    }

    #[test]
    fn non_browser_containers_are_flagged_for_remux() {
        let mut ts = vec![0x47u8; 189];
        ts[188] = 0x47;
        let i = identify(Some("video/mp2t"), "/x.ts", Some(&ts));
        assert!(i.needs_remux);
        assert!(i.remux_reason.unwrap().contains("MP4"));

        let i = identify(Some("video/x-flv"), "/x.flv", None);
        assert!(i.needs_remux);

        // Playable containers must not be flagged.
        for c in [
            Container::Mp4,
            Container::WebM,
            Container::Ogg,
            Container::Mov,
        ] {
            assert!(c.browser_native(), "{c:?} should be native");
        }
        assert!(!identify(Some("video/mp4"), "/a.mp4", None).needs_remux);
    }

    #[test]
    fn unknown_vendor_video_type_is_not_played_blindly() {
        let i = identify(Some("video/x-totally-unknown"), "/a.bin", None);
        assert_eq!(i.media_type, "video/x-totally-unknown");
        assert!(i.needs_remux);
    }

    #[test]
    fn html_error_page_is_never_mistaken_for_media() {
        let page = b"<!DOCTYPE html><html><body>404 not found</body></html>";
        let i = identify(Some("text/html; charset=utf-8"), "/a.mp4", Some(page));
        assert_eq!(i.container, Container::Unknown);
        assert_eq!(i.source, Evidence::ContentType);
        assert_eq!(i.media_type, "application/octet-stream");
        assert!(i.needs_remux);
    }

    #[test]
    fn signature_table_is_complete() {
        let cases: Vec<(Container, &[u8])> = vec![
            (Container::Ogg, b"OggS\x00\x02"),
            (Container::Flv, b"FLV\x01\x05"),
            (Container::WebM, b"\x1a\x45\xdf\xa3\x01"),
            (Container::Avi, b"RIFF\x20\x00\x00\x00AVI LIST"),
            (Container::Wav, b"RIFF\x20\x00\x00\x00WAVEfmt "),
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
