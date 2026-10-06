//! Runtime configuration. Every limit that bounds resource usage lives here,
//! and every value is overridable by environment variable so operators can tune
//! the system without a rebuild.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

use crate::retry::RetryPolicy;

/// Buffering and resource ceilings.
#[derive(Debug, Clone)]
pub struct Config {
    pub bind: SocketAddr,
    /// Directory of built frontend assets served at `/`. `None` disables it.
    pub static_dir: Option<String>,

    /// Maximum simultaneously active upstream streams.
    pub max_concurrent_streams: usize,
    /// Idle keep-alive connections retained per origin.
    pub max_idle_connections_per_host: usize,
    /// TCP connect timeout.
    pub connect_timeout: Duration,
    /// Time allowed for the origin to produce response headers (TTFB).
    pub response_timeout: Duration,
    /// Maximum gap between two body chunks before we treat the origin as gone.
    pub idle_timeout: Duration,
    /// Overall DNS resolution timeout.
    pub dns_timeout: Duration,

    pub max_redirects: usize,
    pub retry: RetryPolicy,

    /// Bytes held in memory per stream between origin and client. This is the
    /// entire per-stream memory budget; media never accumulates here.
    pub stream_buffer_bytes: usize,
    /// Size of each chunk read from the origin socket.
    pub read_chunk_bytes: usize,
    /// How far ahead of the requested offset we ask the origin for data.
    pub prefetch_window_bytes: u64,

    pub cache_capacity: usize,
    pub cache_ttl: Duration,
    pub dns_cache_ttl: Duration,

    pub max_request_body_bytes: usize,
    pub max_url_len: usize,

    /// Content types we are willing to hand to a `<video>` element.
    pub allowed_media_types: Vec<String>,
    /// Hard ceiling on any single buffered metadata entry.
    pub max_meta_bytes: usize,

    pub log_json: bool,
    pub access_log: bool,
    /// Escape hatch for self-hosting and tests: permit requests to private,
    /// loopback and link-local addresses. **Re-opens SSRF.** Off by default.
    pub allow_private_hosts: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 8787),
            static_dir: Some("frontend/dist".to_owned()),
            max_concurrent_streams: 256,
            max_idle_connections_per_host: 8,
            connect_timeout: Duration::from_secs(5),
            response_timeout: Duration::from_secs(15),
            idle_timeout: Duration::from_secs(20),
            dns_timeout: Duration::from_secs(5),
            max_redirects: 5,
            retry: RetryPolicy::default(),
            // 512 KiB per stream: enough to absorb jitter, small enough that
            // 1000 concurrent streams stay well under a gigabyte.
            stream_buffer_bytes: 512 * 1024,
            read_chunk_bytes: 64 * 1024,
            prefetch_window_bytes: 4 * 1024 * 1024,
            cache_capacity: 4096,
            cache_ttl: Duration::from_secs(300),
            dns_cache_ttl: Duration::from_secs(30),
            max_request_body_bytes: 8 * 1024,
            max_url_len: 4096,
            allowed_media_types: default_media_types(),
            max_meta_bytes: 8 * 1024,
            log_json: false,
            access_log: true,
            allow_private_hosts: false,
        }
    }
}

/// Media types every mainstream browser can play via a bare `<video src>`.
fn default_media_types() -> Vec<String> {
    // Exactly the types a browser-playable container reports. An entry that no
    // container can produce can never match, so it reads as a containment
    // boundary while silently not being one.
    [
        "video/mp4",
        "video/webm",
        "video/ogg",
        "video/quicktime",
        "audio/mpeg",
        "audio/wav",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect()
}

impl Config {
    /// Read overrides from the environment, falling back to defaults.
    pub fn from_env() -> Self {
        let mut c = Config::default();

        if let Some(v) = env_string("DDL_BIND") {
            match v.parse::<SocketAddr>() {
                Ok(s) => c.bind = s,
                Err(_) => tracing::warn!(value = %v, "ignoring unparseable DDL_BIND"),
            }
        }
        if let Some(v) = env_string("DDL_STATIC_DIR") {
            c.static_dir = if v.is_empty() { None } else { Some(v) };
        }
        c.max_concurrent_streams =
            env_usize("DDL_MAX_CONCURRENT_STREAMS", c.max_concurrent_streams);
        c.max_idle_connections_per_host =
            env_usize("DDL_MAX_IDLE_CONNECTIONS", c.max_idle_connections_per_host);
        c.max_redirects = env_usize("DDL_MAX_REDIRECTS", c.max_redirects);
        c.cache_capacity = env_usize("DDL_CACHE_CAPACITY", c.cache_capacity);
        c.max_request_body_bytes = env_usize("DDL_MAX_REQUEST_BODY", c.max_request_body_bytes);
        c.max_url_len = env_usize("DDL_MAX_URL_LEN", c.max_url_len);

        c.connect_timeout = env_millis("DDL_CONNECT_TIMEOUT_MS", c.connect_timeout);
        c.response_timeout = env_millis("DDL_RESPONSE_TIMEOUT_MS", c.response_timeout);
        c.idle_timeout = env_millis("DDL_IDLE_TIMEOUT_MS", c.idle_timeout);
        c.dns_timeout = env_millis("DDL_DNS_TIMEOUT_MS", c.dns_timeout);
        c.cache_ttl = env_millis("DDL_CACHE_TTL_MS", c.cache_ttl);
        c.dns_cache_ttl = env_millis("DDL_DNS_CACHE_TTL_MS", c.dns_cache_ttl);

        c.stream_buffer_bytes = env_usize("DDL_STREAM_BUFFER_BYTES", c.stream_buffer_bytes)
            .clamp(16 * 1024, 16 * 1024 * 1024);
        c.read_chunk_bytes = env_usize("DDL_READ_CHUNK_BYTES", c.read_chunk_bytes)
            .clamp(4 * 1024, c.stream_buffer_bytes);
        c.prefetch_window_bytes = env_usize(
            "DDL_PREFETCH_WINDOW_BYTES",
            c.prefetch_window_bytes as usize,
        ) as u64;

        c.retry.max_attempts = env_usize("DDL_MAX_RETRIES", c.retry.max_attempts as usize) as u32;
        c.retry.base_delay = env_millis("DDL_RETRY_BASE_MS", c.retry.base_delay);
        c.retry.max_delay = env_millis("DDL_RETRY_MAX_MS", c.retry.max_delay);

        c.log_json = env_flag("DDL_LOG_JSON", c.log_json);
        c.access_log = env_flag("DDL_ACCESS_LOG", c.access_log);
        c.allow_private_hosts = env_flag("DDL_ALLOW_PRIVATE_HOSTS", c.allow_private_hosts);

        if let Some(v) = env_string("DDL_ALLOWED_MEDIA_TYPES") {
            let parsed: Vec<String> = v
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.to_ascii_lowercase())
                .collect();
            if !parsed.is_empty() {
                c.allowed_media_types = parsed;
            }
        }

        c
    }

    /// Total in-memory bytes a full deployment can hold for stream buffers.
    pub const fn max_total_buffer_bytes(&self) -> usize {
        self.stream_buffer_bytes
            .saturating_mul(self.max_concurrent_streams)
    }
}

fn env_string(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn env_usize(key: &str, default: usize) -> usize {
    match env_string(key).and_then(|v| v.parse().ok()) {
        Some(v) if v > 0 => v,
        Some(_) => {
            tracing::warn!(key, "value must be > 0, using default");
            default
        }
        None => default,
    }
}

fn env_millis(key: &str, default: Duration) -> Duration {
    match env_string(key).and_then(|v| v.parse::<u64>().ok()) {
        Some(v) if v > 0 => Duration::from_millis(v),
        Some(_) => default,
        None => default,
    }
}

fn env_flag(key: &str, default: bool) -> bool {
    match env_string(key) {
        Some(v) => matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on"),
        None => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_bounded_and_sane() {
        let c = Config::default();
        assert!(c.max_concurrent_streams > 0);
        assert!(c.read_chunk_bytes <= c.stream_buffer_bytes);
        assert!(c.max_url_len >= 512);
        assert!(c.retry.max_attempts >= 1);
        assert!(!c.allowed_media_types.is_empty());
        assert!(c
            .allowed_media_types
            .iter()
            .all(|t| t == &t.to_ascii_lowercase()));
    }

    #[test]
    fn total_buffer_is_derivable() {
        let c = Config::default();
        assert_eq!(c.max_total_buffer_bytes(), 512 * 1024 * 256);
        // 1000 concurrent streams must not imply gigabytes of RAM.
        let c = Config {
            max_concurrent_streams: 1000,
            ..c
        };
        assert_eq!(c.max_total_buffer_bytes(), 512 * 1024 * 1000);
    }

    #[test]
    fn media_types_include_browser_playable_containers() {
        let t = Config::default().allowed_media_types;
        for want in ["video/mp4", "video/webm", "audio/mpeg"] {
            assert!(t.iter().any(|x| x == want), "missing {want}");
        }
    }

    /// Every allow-list entry must be one the identifier can actually produce.
    ///
    /// An unreachable entry is worse than a missing one: it reads as a
    /// containment boundary and silently is not one. `DDL_ALLOWED_MEDIA_TYPES`
    /// is advertised in the README, so it has to mean something.
    #[test]
    fn every_allowed_media_type_is_reachable_from_an_identifiable_container() {
        use crate::media::Container;
        let producible: Vec<&str> = [
            Container::Mp4,
            Container::Mov,
            Container::WebM,
            Container::Matroska,
            Container::Ogg,
            Container::Mp3,
            Container::Wav,
            Container::MpegTs,
            Container::Hls,
            Container::Flv,
            Container::Avi,
            Container::Html,
            Container::Json,
            Container::Compressed,
            Container::Empty,
            Container::Unknown,
        ]
        .iter()
        .map(|c| c.media_type())
        .collect();
        for entry in Config::default().allowed_media_types {
            assert!(
                producible.contains(&entry.as_str()),
                "allowed_media_types contains {entry:?}, which no container can \
                 report — it can never match and so is not a real boundary"
            );
        }
    }

    /// A type on the allow-list must also be one a browser can decode, or the
    /// list promises more than the product can keep.
    #[test]
    fn every_allowed_media_type_is_browser_playable() {
        use crate::media::Container;
        for entry in Config::default().allowed_media_types {
            let playable = [
                Container::Mp4,
                Container::Mov,
                Container::WebM,
                Container::Ogg,
                Container::Mp3,
                Container::Wav,
            ]
            .iter()
            .any(|c| c.media_type() == entry);
            assert!(
                playable,
                "{entry:?} is allowed but no browser-playable container reports it"
            );
        }
    }
}
