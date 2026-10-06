//! URL validation and SSRF defence.
//!
//! Threat model: the DDL is fully attacker-controlled. We must never be tricked
//! into fetching `127.0.0.1`, RFC1918 space, link-local cloud metadata, or an
//! internal-only name — including via redirects and including via DNS
//! rebinding (where a name resolves public during validation and private at
//! connect time).
//!
//! Defence strategy:
//!   1. Syntactic validation (scheme, host shape, length, control chars).
//!   2. Denylist of internal hostnames.
//!   3. Resolve the name ourselves, reject the request if *any* usable address
//!      is private (fail closed), and pin the chosen address.
//!   4. The pinned address is what the connection actually uses, so the
//!      rebinding window is closed entirely.
//!   5. Steps 1-4 run again for every redirect hop.

use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::net::lookup_host;
use url::{Host, Url};

use crate::errors::{ErrorCode, PlayerError};

/// Longest URL we accept. Bounds work per request and log size.
pub const MAX_URL_LEN: usize = 4096;

/// Host names that must never resolve, regardless of what DNS says.
const DENY_HOSTS: &[&str] = &[
    "localhost",
    "localhost.localdomain",
    "ip6-localhost",
    "ip6-loopback",
    "metadata",
    "metadata.google.internal",
    "instance-data",
    "instance-data.ec2.internal",
    // Bare internal names, with no dot to anchor a suffix check.
    "internal",
    "intranet",
    "corp",
    "lan",
    "local",
    "home",
    "private",
];

/// Suffixes reserved for local / internal name resolution.
const DENY_SUFFIXES: &[&str] = &[
    ".localhost",
    ".local",
    ".localdomain",
    ".internal",
    ".intranet",
    ".lan",
    ".home",
    ".home.arpa",
    ".corp",
    ".private",
    ".test",
    ".example",
    ".invalid",
];

/// Address policy for a deployment.
///
/// `allow_private` exists for self-hosted setups and for the test suite, where
/// the "remote DDL" is a process on the same machine. It is off by default and
/// logged loudly when on, because turning it on in production re-opens SSRF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Policy {
    pub allow_private: bool,
}

impl Policy {
    /// The production default: refuse every non-public address.
    pub const STRICT: Self = Self {
        allow_private: false,
    };

    /// Accept any address. Only for tests and deliberate self-hosting.
    pub const TRUSTED: Self = Self {
        allow_private: true,
    };

    pub fn from_allow_private(allow_private: bool) -> Self {
        Self { allow_private }
    }
}

/// A URL that passed syntactic validation.
#[derive(Debug, Clone)]
pub struct SafeUrl {
    url: Url,
    /// `host:port` — what we key pools and DNS cache on.
    authority: String,
}

impl SafeUrl {
    pub fn url(&self) -> &Url {
        &self.url
    }

    pub fn authority(&self) -> &str {
        &self.authority
    }

    pub fn host(&self) -> Host<&str> {
        self.url.host().expect("validated")
    }

    pub fn scheme(&self) -> &str {
        self.url.scheme()
    }

    /// Host without brackets, suitable for DNS pinning and HTTP `Host`.
    pub fn host_str(&self) -> &str {
        self.url.host_str().unwrap_or("")
    }

    pub fn is_tls(&self) -> bool {
        self.url.scheme() == "https"
    }

    /// URL with query string and userinfo removed — safe for logs.
    pub fn redacted(&self) -> String {
        redact_url(&self.url)
    }
}

/// Log-safe rendering: keeps scheme/host/path, drops query, fragment, userinfo.
pub fn redact_url(u: &Url) -> String {
    let mut out = format!("{}://{}", u.scheme(), u.host_str().unwrap_or("?"));
    if let Some(port) = u.port() {
        out.push(':');
        out.push_str(&port.to_string());
    }
    let path = u.path();
    if path != "/" && !path.is_empty() {
        out.push_str(path);
    }
    out
}

/// Step 1 + 2: syntax and hostname checks, no DNS, strict policy.
pub fn validate_syntax(raw: &str) -> Result<SafeUrl, PlayerError> {
    validate_with(raw, Policy::STRICT)
}

/// Step 1 + 2 under an explicit policy.
pub fn validate_with(raw: &str, policy: Policy) -> Result<SafeUrl, PlayerError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(PlayerError::new(ErrorCode::InvalidUrl).with_reason("empty url"));
    }
    if raw.len() > MAX_URL_LEN {
        return Err(PlayerError::new(ErrorCode::InvalidUrl)
            .with_reason(format!("url exceeds {MAX_URL_LEN} bytes")));
    }
    if raw.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}') {
        return Err(
            PlayerError::new(ErrorCode::InvalidUrl).with_reason("url contains control characters")
        );
    }

    let url = Url::parse(raw)
        .map_err(|e| PlayerError::new(ErrorCode::InvalidUrl).with_reason(format!("parse: {e}")))?;

    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(PlayerError::new(ErrorCode::UnsupportedProtocol)
                .with_reason(format!("scheme {other} is not permitted")))
        }
    }

    // Embedded credentials would leak into logs and upstream auth headers.
    if !url.username().is_empty() || url.password().is_some() {
        return Err(PlayerError::new(ErrorCode::InvalidUrl)
            .with_reason("credentials embedded in url are not accepted"));
    }

    let host = url
        .host()
        .ok_or_else(|| PlayerError::new(ErrorCode::InvalidUrl).with_reason("url has no host"))?;

    let hostname = match host {
        Host::Domain(d) => normalise_host(d),
        Host::Ipv4(ip) => {
            check_ip_with(IpAddr::V4(ip), policy)?;
            ip.to_string()
        }
        Host::Ipv6(ip) => {
            check_ip_with(IpAddr::V6(ip), policy)?;
            ip.to_string()
        }
    };

    if !hostname.is_empty() {
        // Name-based refusals apply under *every* policy. `DDL_ALLOW_PRIVATE_HOSTS`
        // relaxes address classes for self-hosting; it must never open the door to
        // names that can only ever mean "inside the network".
        check_host_name(&hostname)?;
    }

    let authority = match url.port() {
        Some(p) => format!("{hostname}:{p}"),
        None => format!(
            "{hostname}:{}",
            if url.scheme() == "https" { 443 } else { 80 }
        ),
    };

    Ok(SafeUrl { url, authority })
}

/// Lowercase a domain and strip its trailing dot.
///
/// `db.local.` and `db.local` name the same host, and every resolver accepts
/// both — but a suffix denylist tested with `ends_with(".local")` does not match
/// the first. Normalising once, here, means every later check and the DNS
/// lookup itself see the same name the policy was written about.
fn normalise_host(domain: &str) -> String {
    domain
        .to_ascii_lowercase()
        .trim_end_matches('.')
        .to_owned()
}

fn check_host_name(name: &str) -> Result<(), PlayerError> {
    if DENY_HOSTS.contains(&name) {
        return Err(blocked(name, "reserved hostname"));
    }
    if DENY_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return Err(blocked(name, "internal-only domain suffix"));
    }
    if name.len() > 253 {
        return Err(blocked(name, "hostname too long"));
    }
    Ok(())
}

fn blocked(_host: &str, why: &'static str) -> PlayerError {
    PlayerError::new(ErrorCode::InvalidUrl).with_reason(format!("blocked host: {why}"))
}

/// Step 3: address policy. Fails closed — unknown/special space is refused.
pub fn check_ip(ip: IpAddr) -> Result<(), PlayerError> {
    check_ip_with(ip, Policy::STRICT)
}

pub fn check_ip_with(ip: IpAddr, policy: Policy) -> Result<(), PlayerError> {
    if policy.allow_private {
        return Ok(());
    }
    let ok = match ip {
        IpAddr::V4(v4) => v4_allowed(v4),
        IpAddr::V6(v6) => v6_allowed(v6),
    };
    if ok {
        Ok(())
    } else {
        Err(PlayerError::new(ErrorCode::InvalidUrl)
            .with_reason(format!("blocked address class: {ip}"))
            .with_user_action("Only public internet links can be played."))
    }
}

fn v4_allowed(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    !(ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_documentation()
        || ip.is_multicast()
        || ip.is_unspecified()

        // 0.0.0.0/8 "this network"
        || o[0] == 0
        // 192.0.0.0/24 IETF protocol assignments
        || (o[0] == 192 && o[1] == 0 && o[2] == 0)
        // 192.0.2.0/24, 198.51.100.0/24, 203.0.113.0/24 documentation
        || (o[0] == 198 && (o[1] == 51 || o[1] == 100) && o[2] == 0)
        // 198.18.0.0/15 benchmarking
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19))
        // 240.0.0.0/4 reserved
        || o[0] >= 240
        // 100.64.0.0/10 CGNAT / carrier NAT
        || (o[0] == 100 && (o[1] & 0b1100_0000) == 0b0100_0000))
}

fn v6_allowed(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    // IPv4-mapped / IPv4-compatible / NAT64 / 6to4 all tunnel an IPv4
    // destination; apply the v4 policy to the embedded address.
    if let Some(v4) = embedded_v4(ip) {
        return v4_allowed(v4);
    }
    !(ip.is_loopback()
        || ip.is_unspecified()
        || ip.is_multicast()
        // fc00::/7 unique local
        || (s[0] & 0xfe00) == 0xfc00
        // fe80::/10 link local
        || (s[0] & 0xffc0) == 0xfe80
        // fec0::/10 site local (deprecated)
        || (s[0] & 0xffc0) == 0xfec0
        // 2001:db8::/32 documentation
        || s[0] == 0x2001 && s[1] == 0x0db8
        // 100::/64 discard-only
        || s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0
        // 2001::/23 IETF protocol assignments
        || (s[0] == 0x2001 && s[1] & 0xfe00 == 0)
        // 2002::/16 6to4 handled above via embedded_v4, but keep unknown
        // transition space out as well
        || s[0] == 0x2002)
}

fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let s = ip.segments();
    // ::ffff:a.b.c.d
    if s[..5] == [0, 0, 0, 0, 0] && s[5] == 0xffff {
        return Some(v4_from(s[6], s[7]));
    }
    // 64:ff9b::a.b.c.d — the well-known NAT64 prefix. Without this an attacker
    // could reach 127.0.0.1 through a translator that is perfectly happy to
    // loop internally.
    if s[0] == 0x0064 && s[1] == 0xff9b && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        return Some(v4_from(s[6], s[7]));
    }
    // ::a.b.c.d (deprecated but still routable in some stacks)
    if s[..6] == [0, 0, 0, 0, 0, 0] {
        return Some(v4_from(s[6], s[7]));
    }
    None
}

fn v4_from(hi: u16, lo: u16) -> Ipv4Addr {
    Ipv4Addr::new(
        (hi >> 8) as u8,
        (hi & 0xff) as u8,
        (lo >> 8) as u8,
        (lo & 0xff) as u8,
    )
}

/// Result of a successful resolve-and-validate.
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub socket: std::net::SocketAddr,
    pub address: IpAddr,
}

/// Short-lived DNS answer cache. Cuts a full round trip out of every
/// redirect hop and every seek that re-requests the same origin, which is the
/// single largest avoidable latency in this system.
#[derive(Debug)]
pub struct DnsCache {
    ttl: Duration,
    max: usize,
    map: Mutex<HashSet<String>>,
    entries: Mutex<std::collections::HashMap<String, (Instant, ResolvedTarget)>>,
    hits: AtomicU64,
    misses: AtomicU64,
}

impl DnsCache {
    pub fn new(ttl: Duration, max: usize) -> Self {
        Self {
            ttl,
            max,
            map: Mutex::new(HashSet::new()),
            entries: Mutex::new(std::collections::HashMap::new()),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    pub fn hits(&self) -> u64 {
        self.hits.load(Ordering::Relaxed)
    }

    pub fn misses(&self) -> u64 {
        self.misses.load(Ordering::Relaxed)
    }

    pub fn len(&self) -> usize {
        self.entries.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn get(&self, key: &str) -> Option<ResolvedTarget> {
        let entries = self.entries.lock().ok()?;
        if let Some((at, t)) = entries.get(key) {
            if at.elapsed() < self.ttl {
                self.hits.fetch_add(1, Ordering::Relaxed);
                return Some(t.clone());
            }
        }
        self.misses.fetch_add(1, Ordering::Relaxed);
        None
    }

    fn put(&self, key: String, value: ResolvedTarget) {
        if let Ok(mut entries) = self.entries.lock() {
            let mut seen = self.map.lock().ok();
            if entries.len() >= self.max {
                if let Some(seen) = seen.as_mut() {
                    seen.clear();
                }
                entries.clear();
            }
            entries.insert(key.clone(), (Instant::now(), value));
            if let Some(seen) = seen.as_mut() {
                seen.insert(key);
            }
        }
    }
}

impl Default for DnsCache {
    fn default() -> Self {
        Self::new(Duration::from_secs(30), 1024)
    }
}

/// Resolve, validate every answer, and return one pinned address.
pub async fn resolve_and_pin(
    cache: &DnsCache,
    safe: &SafeUrl,
) -> Result<ResolvedTarget, PlayerError> {
    resolve_and_pin_with(cache, safe, Policy::STRICT).await
}

pub async fn resolve_and_pin_with(
    cache: &DnsCache,
    safe: &SafeUrl,
    policy: Policy,
) -> Result<ResolvedTarget, PlayerError> {
    let authority = safe.authority().to_owned();
    if let Some(hit) = cache.get(&authority) {
        // Re-check: policy could have tightened between calls.
        check_ip_with(hit.address, policy)?;
        return Ok(hit);
    }

    let port = safe.url().port_or_known_default().ok_or_else(|| {
        PlayerError::new(ErrorCode::InvalidUrl).with_reason("no port and no known default")
    })?;

    let addrs: Vec<SocketAddr> = lookup_host((safe.host_str(), port))
        .await
        .map_err(|e| {
            PlayerError::new(ErrorCode::DnsFailure).with_reason(format!("resolve failed: {e}"))
        })?
        .collect();

    if addrs.is_empty() {
        return Err(PlayerError::new(ErrorCode::DnsFailure).with_reason("no addresses returned"));
    }

    // Fail closed: every answer must be publicly routable. A host that
    // resolves to both a public and a private address is refused outright.
    for a in &addrs {
        if let Err(e) = check_ip_with(a.ip(), policy) {
            return Err(e.with_reason("host resolves to a non-public address"));
        }
    }

    // Prefer IPv4: for media, a 6-tuple hole or a slow path is worse than a
    // working v4 endpoint, and most origins are dual-stack.
    let chosen = *addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .expect("non-empty");

    let target = ResolvedTarget {
        socket: chosen,
        address: chosen.ip(),
    };
    cache.put(authority, target.clone());
    Ok(target)
}

/// Apply a `Location` header, resolving it against the current URL.
pub fn resolve_redirect(from: &Url, location: &str) -> Result<Url, PlayerError> {
    if location.len() > MAX_URL_LEN {
        return Err(PlayerError::new(ErrorCode::InvalidUrl)
            .with_reason("redirect target exceeds url length limit"));
    }
    if location.chars().any(|c| (c as u32) < 0x20 || c == '\u{7f}') {
        return Err(PlayerError::new(ErrorCode::InvalidUrl)
            .with_reason("redirect target contains control characters"));
    }
    from.join(location).map_err(|e| {
        PlayerError::new(ErrorCode::InvalidUrl).with_reason(format!("bad location: {e}"))
    })
}

/// Whether a status is a redirect we follow, and whether method/body survive.
pub const fn redirect_kind(status: u16) -> Option<RedirectKind> {
    match status {
        301 | 308 => Some(RedirectKind::PermanentKeepMethod),
        302 | 307 => Some(RedirectKind::TemporaryKeepMethod),
        303 => Some(RedirectKind::SeeOther),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectKind {
    PermanentKeepMethod,
    TemporaryKeepMethod,
    SeeOther,
}

impl RedirectKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PermanentKeepMethod => "permanent",
            Self::TemporaryKeepMethod => "temporary",
            Self::SeeOther => "see-other",
        }
    }
}

/// Detect a redirect loop by URL identity.
pub fn is_loop(hops: &[Url], next: &Url) -> bool {
    hops.iter().any(|h| h == next)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_public_urls() {
        for good in [
            "https://example.com/video.mp4",
            "http://example.com:8080/a.mp4?x=1",
            "https://1.1.1.1/x.mp4",
            "https://[2606:4700:4700::1111]/x.mp4",
            "https://EXAMPLE.com/video.mp4",
        ] {
            assert!(validate_syntax(good).is_ok(), "{good}");
        }
    }

    #[test]
    fn rejects_bad_syntax() {
        for bad in [
            "",
            "   ",
            "not a url",
            "ftp://example.com/a.mp4",
            "file:///etc/passwd",
            "data:video/mp4;base64,AAAA",
            "javascript:alert(1)",
            "https://",
            "https://user:pass@example.com/a.mp4",
        ] {
            let e = validate_syntax(bad).unwrap_err();
            assert!(
                matches!(
                    e.code,
                    ErrorCode::InvalidUrl | ErrorCode::UnsupportedProtocol
                ),
                "{bad} -> {e}"
            );
        }
    }

    #[test]
    fn rejects_control_characters_and_length() {
        assert!(validate_syntax("https://example.com/a\nb.mp4").is_err());
        assert!(validate_syntax("https://example.com/a\r\nX: 1").is_err());
        let long = format!("https://example.com/{}", "a".repeat(MAX_URL_LEN));
        assert!(validate_syntax(&long).is_err());
    }

    #[test]
    fn rejects_internal_hostnames() {
        for bad in [
            "http://localhost/a.mp4",
            "http://localhost:9000/a.mp4",
            "http://app.localhost/a.mp4",
            "http://metadata.google.internal/latest/meta-data/",
            "http://db.internal/a.mp4",
            "http://printer.local/a.mp4",
            "http://x.home.arpa/a.mp4",
            "http://corp/a.mp4",
        ] {
            assert!(validate_syntax(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rejects_private_and_special_v4() {
        for bad in [
            "127.0.0.1",
            "127.1.2.3",
            "0.0.0.0",
            "0.1.2.3",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.1",
            "169.254.169.254",
            "169.254.0.1",
            "100.64.0.1",
            "192.0.0.1",
            "192.0.2.5",
            "198.18.0.1",
            "198.51.100.7",
            "203.0.113.9",
            "224.0.0.1",
            "239.255.255.250",
            "255.255.255.255",
            "240.0.0.1",
        ] {
            let u = format!("http://{bad}/a.mp4");
            assert!(validate_syntax(&u).is_err(), "{u} should be blocked");
        }
    }

    #[test]
    fn allows_public_v4_edges() {
        for good in [
            "1.0.0.1",
            "8.8.8.8",
            "9.255.255.255",
            "11.0.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.167.255.255",
            "192.169.0.1",
            "198.20.0.1",
            "223.255.255.255",
        ] {
            let u = format!("http://{good}/a.mp4");
            assert!(validate_syntax(&u).is_ok(), "{good} should be allowed");
        }
    }

    #[test]
    fn rejects_private_v6_and_tunnelled_v4() {
        for bad in [
            "::1",
            "::",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "::127.0.0.1",
            "2002:7f00:0001::1",
            "64:ff9b::7f00:1",
        ] {
            let u = format!("http://[{bad}]/a.mp4");
            assert!(validate_syntax(&u).is_err(), "{u} should be blocked");
        }
    }

    #[test]
    fn allows_public_v6() {
        for good in [
            "2606:4700:4700::1111",
            "2001:4860:4860::8888",
            "2a00:1450:4001:81f::200e",
        ] {
            let u = format!("https://[{good}]/a.mp4");
            assert!(validate_syntax(&u).is_ok(), "{good} should be allowed");
        }
    }

    #[test]
    fn ipv4_mapped_private_is_refused() {
        // The classic SSRF bypass: a v6 literal wrapping 127.0.0.1.
        assert!(check_ip(IpAddr::V6("::ffff:127.0.0.1".parse().unwrap())).is_err());
        assert!(check_ip(IpAddr::V6("::ffff:8.8.8.8".parse().unwrap())).is_ok());
    }

    #[test]
    fn redaction_removes_query_and_userinfo() {
        let u =
            Url::parse("https://user:pw@example.com:8443/a/b.mp4?token=secret&x=1#frag").unwrap();
        assert_eq!(redact_url(&u), "https://example.com:8443/a/b.mp4");
    }

    #[test]
    fn safe_url_redacted_never_leaks_query() {
        let s = validate_syntax("https://example.com/a.mp4?sig=deadbeef").unwrap();
        assert_eq!(s.redacted(), "https://example.com/a.mp4");
        assert_eq!(s.authority(), "example.com:443");
    }

    #[test]
    fn redirect_resolution() {
        let from = Url::parse("https://a.example/x/y.mp4").unwrap();
        assert_eq!(
            resolve_redirect(&from, "/z.mp4").unwrap().as_str(),
            "https://a.example/z.mp4"
        );
        assert_eq!(
            resolve_redirect(&from, "https://b.example/q")
                .unwrap()
                .as_str(),
            "https://b.example/q"
        );
        assert!(resolve_redirect(&from, "http://a.example/\n").is_err());
        assert!(resolve_redirect(&from, &format!("/{}", "x".repeat(MAX_URL_LEN))).is_err());
    }

    #[test]
    fn redirect_loop_detection() {
        let a = Url::parse("https://a.example/1").unwrap();
        let b = Url::parse("https://a.example/2").unwrap();
        let hops = vec![a.clone(), b.clone()];
        assert!(is_loop(&hops, &a));
        assert!(is_loop(&hops, &b));
        assert!(!is_loop(&hops, &Url::parse("https://a.example/3").unwrap()));
    }

    #[test]
    fn redirect_status_table() {
        assert_eq!(redirect_kind(301), Some(RedirectKind::PermanentKeepMethod));
        assert_eq!(redirect_kind(302), Some(RedirectKind::TemporaryKeepMethod));
        assert_eq!(redirect_kind(303), Some(RedirectKind::SeeOther));
        assert_eq!(redirect_kind(307), Some(RedirectKind::TemporaryKeepMethod));
        assert_eq!(redirect_kind(308), Some(RedirectKind::PermanentKeepMethod));
        assert_eq!(redirect_kind(200), None);
        assert_eq!(redirect_kind(304), None);
        assert_eq!(redirect_kind(399), None);
        assert_eq!(redirect_kind(419), None);
    }

    #[test]
    fn dns_cache_is_bounded_and_counts_hits() {
        let c = DnsCache::new(Duration::from_secs(60), 4);
        let t = ResolvedTarget {
            socket: "1.1.1.1:443".parse().unwrap(),
            address: IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
        };
        for i in 0..8u8 {
            c.put(format!("h{i}"), t.clone());
        }
        assert!(c.len() <= 4, "len was {}", c.len());

        assert!(c.get("absent").is_none());
        assert_eq!(c.misses(), 1);

        c.put("hot".into(), t.clone());
        assert!(c.get("hot").is_some());
        assert_eq!(c.hits(), 1);
    }

    #[test]
    fn dns_cache_expires() {
        let c = DnsCache::new(Duration::from_millis(30), 8);
        c.put(
            "h".into(),
            ResolvedTarget {
                socket: "1.1.1.1:443".parse().unwrap(),
                address: IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1)),
            },
        );
        std::thread::sleep(Duration::from_millis(60));
        assert!(c.get("h").is_none());
    }

    #[test]
    fn the_private_host_escape_hatch_relaxes_addresses_but_never_names() {
        // Self-hosting needs loopback and RFC1918 to work.
        for ok in [
            "http://127.0.0.1:9000/video.mp4",
            "http://10.1.2.3/video.mp4",
            "http://192.168.1.10:8080/video.mp4",
            "http://[::1]:9000/video.mp4",
        ] {
            assert!(
                validate_with(ok, Policy::TRUSTED).is_ok(),
                "{ok} must be allowed under the trusted policy"
            );
        }
        // It must not open the door to names that can only be internal.
        for bad in [
            "http://localhost:9000/video.mp4",
            "http://db.internal/video.mp4",
            "http://metadata.google.internal/x",
            "http://corp/video.mp4",
            "http://printer.local/video.mp4",
        ] {
            assert!(
                validate_with(bad, Policy::TRUSTED).is_err(),
                "{bad} must stay refused under the trusted policy"
            );
        }
        // And the strict policy still refuses the addresses.
        assert!(validate_with("http://127.0.0.1:9000/video.mp4", Policy::STRICT).is_err());
    }

    /// A trailing dot names the same host and every resolver accepts it, so a
    /// suffix denylist tested with a bare `ends_with` misses it. Under
    /// `DDL_ALLOW_PRIVATE_HOSTS` the address check is relaxed, which makes this
    /// the only thing standing between a request and an internal name.
    #[test]
    fn a_trailing_dot_does_not_evade_the_internal_name_denylist() {
        for bad in [
            "http://db.internal./video.mp4",
            "http://printer.local./video.mp4",
            "http://metadata.google.internal./x",
            "http://localhost./video.mp4",
            "http://nas.lan./video.mp4",
            // Repeated dots are the same trick with extra steps.
            "http://db.internal../video.mp4",
        ] {
            assert!(
                validate_with(bad, Policy::TRUSTED).is_err(),
                "{bad} must stay refused: a trailing dot does not make an \
                 internal name external"
            );
        }
    }

    /// Normalisation must not break an ordinary public name.
    #[test]
    fn a_trailing_dot_on_a_public_name_is_accepted() {
        assert!(validate_syntax("https://example.com./video.mp4").is_ok());
        assert_eq!(normalise_host("Example.COM."), "example.com");
    }

    #[tokio::test]
    async fn dns_failure_is_explicit() {
        let cache = DnsCache::default();
        // .invalid is reserved and must never resolve.
        let safe = match validate_syntax("http://definitely-not-a-real-host.invalid/a.mp4") {
            Ok(s) => s,
            Err(_) => return, // blocked syntactically: also correct
        };
        let err = resolve_and_pin(&cache, &safe).await.unwrap_err();
        assert_eq!(err.code, ErrorCode::DnsFailure);
    }
}
