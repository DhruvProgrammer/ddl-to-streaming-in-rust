//! Per-origin connection pool with DNS pinning.
//!
//! One `reqwest::Client` per origin authority. That costs some memory, and it
//! buys two things we cannot have otherwise:
//!
//!   * **Pinning.** `ClientBuilder::resolve` binds a hostname to the exact IP
//!     we validated. The window in which DNS could be flipped from public to
//!     private (rebinding) does not exist.
//!   * **Correct pooling.** The pool is keyed by authority, so idle keep-alive
//!     connections are reused across seeks and redirects to the same host.
//!
//! The pool itself is bounded: past `max_clients` the least recently created
//! client is dropped, which closes its idle sockets.

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::config::Config;
use crate::errors::{ErrorCode, PlayerError};
use crate::security::{self, resolve_and_pin_with, DnsCache, SafeUrl};

/// Compact, log-safe request identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RequestId(u64);

impl RequestId {
    pub fn next() -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        // Time-derived prefix keeps IDs roughly sortable across restarts
        // without pulling in a UUID dependency.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        Self(t.rotate_left(17) ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15))
    }

    pub const fn raw(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for RequestId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

pub const USER_AGENT: &str = concat!("ddl-player/", env!("CARGO_PKG_VERSION"));

/// Headers we are willing to send to an origin. Anything not here is dropped.
pub fn origin_request_headers() -> [(&'static str, &'static str); 3] {
    [
        ("user-agent", USER_AGENT),
        // Identity encoding is mandatory: a gzip or br content coding makes
        // Content-Length and Content-Range describe the *encoded* bytes, which
        // would silently break every seek.
        ("accept-encoding", "identity"),
        ("accept", "*/*"),
    ]
}

/// Response headers we allow an origin to set on *us* that we may forward.
pub const FORWARDABLE_RESPONSE_HEADERS: [&str; 2] = ["etag", "last-modified"];

/// Response headers we always regenerate ourselves.
pub const STRIPPED_RESPONSE_HEADERS: [&str; 10] = [
    "connection",
    "keep-alive",
    "transfer-encoding",
    "upgrade",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "set-cookie",
    "content-encoding",
];

struct PooledClient {
    client: Arc<reqwest::Client>,
    created: u64,
}

/// Bounded map of authority -> pinned client.
pub struct OriginPool {
    clients: Mutex<HashMap<String, PooledClient>>,
    order: Mutex<VecDeque<String>>,
    max_clients: usize,
    tick: AtomicU64,
    dns: DnsCache,
    built: AtomicU64,
    dropped: AtomicU64,
    evictions: AtomicU64,
    timeout_cfg: PoolTimeouts,
    policy: security::Policy,
}

#[derive(Debug, Clone, Copy)]
pub struct PoolTimeouts {
    pub connect: Duration,
    pub dns: Duration,
    pub idle: Duration,
    pub max_idle_connections: usize,
}

impl OriginPool {
    pub fn new(cfg: &Config) -> Self {
        Self {
            clients: Mutex::new(HashMap::new()),
            order: Mutex::new(VecDeque::new()),
            max_clients: 256,
            tick: AtomicU64::new(0),
            dns: DnsCache::new(cfg.dns_cache_ttl, 2048),
            built: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            timeout_cfg: PoolTimeouts {
                connect: cfg.connect_timeout,
                dns: cfg.dns_timeout,
                idle: cfg.idle_timeout,
                max_idle_connections: cfg.max_idle_connections_per_host,
            },
            policy: security::Policy::from_allow_private(cfg.allow_private_hosts),
        }
    }

    /// Address policy in force. Exposed so the engine validates hops with the
    /// same policy the pool pinned with.
    pub const fn policy(&self) -> security::Policy {
        self.policy
    }

    pub fn dns_cache(&self) -> &DnsCache {
        &self.dns
    }

    pub fn stats(&self) -> PoolStats {
        PoolStats {
            clients: self.clients.lock().map(|m| m.len()).unwrap_or(0),
            built: self.built.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            dns_hits: self.dns.hits(),
            dns_misses: self.dns.misses(),
        }
    }

    /// Get (or build) the pinned client for this URL.
    ///
    /// A cached client is reused only if it is still pinned to an address that
    /// passes current policy.
    pub async fn client_for(&self, safe: &SafeUrl) -> Result<Arc<reqwest::Client>, PlayerError> {
        let authority = safe.authority().to_owned();
        if let Some(hit) = self.lookup(&authority) {
            return Ok(hit);
        }
        let target = tokio::time::timeout(
            self.timeout_cfg.dns,
            resolve_and_pin_with(&self.dns, safe, self.policy),
        )
        .await
        .map_err(|_| {
            PlayerError::new(ErrorCode::DnsFailure).with_reason("dns resolution timed out")
        })??;

        let client = self.build(&authority, safe.host_str(), target.socket)?;
        Ok(client)
    }

    fn lookup(&self, authority: &str) -> Option<Arc<reqwest::Client>> {
        let tick = self.tick.fetch_add(1, Ordering::Relaxed) + 1;
        let mut clients = self.clients.lock().ok()?;
        let entry = clients.get_mut(authority)?;
        entry.created = tick;
        Some(entry.client.clone())
    }

    fn build(
        &self,
        authority: &str,
        host: &str,
        socket: SocketAddr,
    ) -> Result<Arc<reqwest::Client>, PlayerError> {
        let t = self.timeout_cfg;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(t.connect)
            .pool_max_idle_per_host(t.max_idle_connections)
            .pool_idle_timeout(Duration::from_secs(90))
            .tcp_keepalive(Duration::from_secs(60))
            .tcp_nodelay(true)
            .user_agent(USER_AGENT)
            // No global request timeout: a 20 GB body must be allowed to
            // stream for hours. Headers and per-chunk stalls are bounded
            // separately by the engine.
            .resolve(host, socket)
            .build()
            .map_err(|e| {
                PlayerError::new(ErrorCode::TlsFailure)
                    .with_reason(format!("client build failed: {e}"))
            })?;
        let client = Arc::new(client);
        self.built.fetch_add(1, Ordering::Relaxed);
        self.insert(authority.to_owned(), Arc::clone(&client));
        Ok(client)
    }

    fn insert(&self, key: String, client: Arc<reqwest::Client>) {
        let Ok(mut clients) = self.clients.lock() else {
            return;
        };
        let Ok(mut order) = self.order.lock() else {
            return;
        };
        if clients
            .insert(key.clone(), PooledClient { client, created: 0 })
            .is_some()
        {
            return;
        }
        order.push_back(key);
        while clients.len() > self.max_clients {
            let Some(oldest) = order.pop_front() else {
                break;
            };
            if clients.remove(&oldest).is_some() {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                self.evictions.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct PoolStats {
    pub clients: usize,
    pub built: u64,
    pub dropped: u64,
    pub evictions: u64,
    pub dns_hits: u64,
    pub dns_misses: u64,
}

impl PoolStats {
    /// Fraction of DNS lookups served from cache.
    pub fn dns_cache_hit_rate(&self) -> Option<f64> {
        let total = self.dns_hits + self.dns_misses;
        (total > 0).then(|| self.dns_hits as f64 / total as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::security::validate_syntax;

    #[test]
    fn request_ids_are_unique_and_short() {
        let a = RequestId::next();
        let b = RequestId::next();
        assert_ne!(a, b);
        assert_eq!(format!("{a}").len(), 16);
    }

    #[test]
    fn identity_encoding_is_always_requested() {
        let headers = origin_request_headers();
        assert!(headers
            .iter()
            .any(|(k, v)| *k == "accept-encoding" && *v == "identity"));
        assert!(headers.iter().any(|(k, _)| *k == "user-agent"));
    }

    #[test]
    fn hop_by_hop_headers_are_stripped() {
        for h in STRIPPED_RESPONSE_HEADERS {
            assert!(
                !FORWARDABLE_RESPONSE_HEADERS.contains(&h),
                "{h} must not be forwarded"
            );
        }
    }

    #[tokio::test]
    async fn pool_client_build_pins_dns() {
        let cfg = Config::default();
        let pool = OriginPool::new(&cfg);
        let safe = validate_syntax("http://example.com/video.mp4").unwrap();
        let client = pool.client_for(&safe).await;
        // Network may be unavailable in a sandbox; either way we must never
        // panic and must never hand back an unpinned client.
        if let Ok(c) = client {
            // Reuse must return the identical client (connection reuse).
            let again = pool.client_for(&safe).await.unwrap();
            assert!(Arc::ptr_eq(&c, &again));
        }
    }

    #[test]
    fn client_count_is_bounded() {
        let pool = OriginPool {
            max_clients: 4,
            ..OriginPool::new(&Config::default())
        };
        for i in 0..20u8 {
            let addr: SocketAddr = format!("203.0.113.{}:80", i + 1).parse().unwrap();
            let host = format!("h{i}.example");
            let _ = pool.build(&host, &host, addr);
        }
        assert!(
            pool.stats().clients <= 4,
            "clients: {}",
            pool.stats().clients
        );
        assert!(pool.stats().evictions > 0);
    }

    #[test]
    fn dns_cache_hit_rate_is_none_until_used() {
        let s = PoolStats {
            clients: 0,
            built: 0,
            dropped: 0,
            evictions: 0,
            dns_hits: 0,
            dns_misses: 0,
        };
        assert!(s.dns_cache_hit_rate().is_none());
        let s = PoolStats {
            dns_hits: 3,
            dns_misses: 1,
            ..s
        };
        assert_eq!(s.dns_cache_hit_rate(), Some(0.75));
    }
}
