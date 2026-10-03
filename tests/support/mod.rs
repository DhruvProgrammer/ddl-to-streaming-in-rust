//! A deliberately hostile origin server, written straight on TCP so we can
//! emit things no well-behaved HTTP library would produce: malformed
//! `Content-Range`, lying `Content-Length`, truncated bodies, abrupt
//! disconnects, headerless responses, slow transfers.
//!
//! Everything the integration and chaos suites need to make the proxy fail in
//! every way it claims to survive.

// Items here are `pub` because three separate test binaries share this module,
// but each of them only reaches it from inside its own crate.
#![allow(dead_code, unreachable_pub)]

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Fault modes the origin can be switched into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Behaviour {
    /// Well-behaved: honours `Range`, correct headers.
    Normal,
    /// Ignore the client's `Range` and always send `200` with the whole body.
    IgnoreRange,
    /// Advertise `Accept-Ranges: none`.
    NoAcceptRanges,
    /// `200` with a body but no `Content-Length` (close-delimited).
    NoContentLength,
    /// `206` whose `Content-Range` start does not match the request.
    BrokenContentRange,
    /// `Content-Length` larger than the body actually sent.
    WrongContentLength,
    /// Correct headers, then close the socket after `n` body bytes.
    TruncateAfter(usize),
    /// Correct headers, then stall forever.
    StallAfter(usize),
    /// Always answer with this status.
    Status(u16),
    /// `Retry-After: <secs>` plus a 503.
    RateLimited(u64),
    /// Answer `fail_times` times with `code`, then behave normally.
    Flaky { fail_times: u16, code: u16 },
    /// Redirect chain: `/hop/N` -> `/hop/N-1` -> ... -> `/media`.
    Redirect(u16),
    /// Respond with `Location: <internal url>` (SSRF attempt).
    RedirectToPrivate(String),
    /// Respond with a body that is not media at all.
    NotMedia,
    /// Omit the `Content-Type` entirely.
    OmitContentType,
    /// Return a single byte and then drop the connection with no data.
    ImmediateDisconnect,
    /// Reply with headers only, no body, no `Content-Length`.
    HeadersOnlyThenClose,
    /// Very large response headers (header-flood attempt).
    HeaderFlood(usize),
    /// Slow drip of the body.
    SlowBody(Duration),
}

/// Deterministic media payload: an MP4 `ftyp` header followed by a pattern the
/// tests can locate at any offset.
pub fn make_payload(size: usize) -> Arc<Vec<u8>> {
    // Building a 128 MiB pattern per request would dominate the test runtime
    // and confuse timeout-based assertions, so it is memoised per size.
    static CACHE: std::sync::OnceLock<Mutex<HashMap<usize, Arc<Vec<u8>>>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(hit) = cache.lock().ok().and_then(|m| m.get(&size).cloned()) {
        return hit;
    }
    let mut v = Vec::with_capacity(size);
    v.extend_from_slice(&32u32.to_be_bytes());
    v.extend_from_slice(b"ftypisom");
    v.extend_from_slice(&[0u8; 24]);
    let mut x: u32 = 0x1234_5678;
    while v.len() < size {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        v.extend_from_slice(&x.to_le_bytes());
    }
    v.truncate(size);
    let payload = Arc::new(v);
    if let Ok(mut c) = cache.lock() {
        c.insert(size, Arc::clone(&payload));
    }
    payload
}

/// Byte at `offset` inside a payload produced by [`make_payload`].
pub fn payload_byte(size: usize, offset: u64) -> u8 {
    let p = make_payload(size);
    p[offset as usize]
}

/// Handle to a running origin.
pub struct Origin {
    pub addr: SocketAddr,
    state: Arc<Mutex<State>>,
    pub(crate) shutdown: tokio::sync::watch::Sender<bool>,
    pub requests: Arc<AtomicU64>,
    pub bytes_served: Arc<AtomicU64>,
}

impl Origin {
    pub async fn start(default_size: usize) -> Origin {
        Self::start_with(default_size, Behaviour::Normal).await
    }

    pub async fn start_with(default_size: usize, initial: Behaviour) -> Origin {
        let state = Arc::new(Mutex::new(State {
            behaviour: initial,
            size: default_size,
            seen_ranges: Vec::new(),
            flaky_left: 0,
            flaky_armed: false,
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind origin");
        let addr = listener.local_addr().expect("origin addr");
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        let requests = Arc::new(AtomicU64::new(0));
        let bytes_served = Arc::new(AtomicU64::new(0));

        let st = state.clone();
        let reqs = requests.clone();
        let served = bytes_served.clone();
        tokio::spawn(async move {
            loop {
                let accept = tokio::select! {
                    r = listener.accept() => r,
                    _ = rx.changed() => break,
                };
                let Ok((sock, _)) = accept else { continue };
                if *rx.borrow() {
                    break;
                }
                let st = st.clone();
                let reqs = reqs.clone();
                let served = served.clone();
                tokio::spawn(async move {
                    let _ = handle(sock, st, reqs, served).await;
                });
            }
        });

        Origin {
            addr,
            state,
            shutdown: tx,
            requests,
            bytes_served,
        }
    }

    /// `http://127.0.0.1:<port>/media`
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn set_behaviour(&self, b: Behaviour) {
        if let Ok(mut s) = self.state.lock() {
            s.behaviour = b;
            s.flaky_armed = false;
            s.flaky_left = 0;
        }
    }

    pub fn behaviour(&self) -> Behaviour {
        self.state
            .lock()
            .map(|s| s.behaviour.clone())
            .unwrap_or(Behaviour::Normal)
    }

    pub fn set_size(&self, size: usize) {
        if let Ok(mut s) = self.state.lock() {
            s.size = size;
        }
    }

    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn bytes_served(&self) -> u64 {
        self.bytes_served.load(Ordering::Relaxed)
    }

    pub fn ranges_seen(&self) -> Vec<String> {
        self.state
            .lock()
            .map(|s| s.seen_ranges.clone())
            .unwrap_or_default()
    }

    pub fn reset(&self) {
        if let Ok(mut s) = self.state.lock() {
            s.seen_ranges.clear();
        }
        self.requests.store(0, Ordering::Relaxed);
        self.bytes_served.store(0, Ordering::Relaxed);
    }

    /// Stop accepting new connections. Takes `&self` so a test can shut the
    /// origin down without consuming the server it is inspecting.
    pub async fn stop(&self) {
        let _ = self.shutdown.send(true);
    }
}

struct State {
    behaviour: Behaviour,
    size: usize,
    seen_ranges: Vec<String>,
    /// Remaining failures for `Behaviour::Flaky`.
    flaky_left: u16,
    /// Whether the flaky counter has been armed since the last `set_behaviour`.
    flaky_armed: bool,
}

struct Request {
    path: String,
    range: Option<String>,
    headers: HashMap<String, String>,
}

async fn handle(
    mut sock: TcpStream,
    state: Arc<Mutex<State>>,
    requests: Arc<AtomicU64>,
    bytes_served: Arc<AtomicU64>,
) -> std::io::Result<()> {
    // Keep-alive: serve requests on this connection until the client leaves.
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    loop {
        let req = match read_request(&mut sock, &mut buf).await? {
            Some(r) => r,
            None => return Ok(()),
        };
        requests.fetch_add(1, Ordering::Relaxed);
        let (behaviour, size) = {
            let mut s = state.lock().expect("state");
            if let Some(r) = &req.range {
                s.seen_ranges.push(r.clone());
            }
            // A flaky origin burns one failure per request, then heals. The
            // counter is armed once per `set_behaviour`, so it cannot reset
            // itself on the request that drains it.
            let mut b = s.behaviour.clone();
            if let Behaviour::Flaky { fail_times, code } = &b {
                if !s.flaky_armed {
                    s.flaky_armed = true;
                    s.flaky_left = *fail_times;
                }
                if s.flaky_left > 0 {
                    s.flaky_left -= 1;
                    b = Behaviour::Flaky {
                        fail_times: 0,
                        code: *code,
                    };
                } else {
                    b = Behaviour::Normal;
                }
            }
            (b, s.size)
        };
        let keep_alive = respond(&mut sock, &req, &behaviour, size).await?;
        bytes_served.fetch_add(size as u64, Ordering::Relaxed);
        if !keep_alive {
            return Ok(());
        }
    }
}

async fn read_request(sock: &mut TcpStream, buf: &mut Vec<u8>) -> std::io::Result<Option<Request>> {
    loop {
        if let Some(pos) = find_headers_end(buf) {
            let head = String::from_utf8_lossy(&buf[..pos]).to_string();
            buf.drain(..pos + 4);
            let mut lines = head.split("\r\n");
            let Some(request_line) = lines.next() else {
                return Ok(None);
            };
            let mut parts = request_line.split_whitespace();
            let _method = parts.next().unwrap_or("GET");
            let path = parts.next().unwrap_or("/").to_owned();
            let mut headers = HashMap::new();
            for line in lines {
                if let Some((k, v)) = line.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
                }
            }
            return Ok(Some(Request {
                path,
                range: headers.get("range").cloned(),
                headers,
            }));
        }
        if buf.len() > 64 * 1024 {
            return Ok(None);
        }
        let mut chunk = [0u8; 2048];
        let n = sock.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Returns whether the connection may be reused.
async fn respond(
    sock: &mut TcpStream,
    req: &Request,
    behaviour: &Behaviour,
    size: usize,
) -> std::io::Result<bool> {
    match behaviour {
        Behaviour::Status(code) => {
            let body = format!("upstream said {code}");
            write_simple(
                sock,
                *code,
                "text/plain",
                body.as_bytes(),
                Some(&req.headers),
                true,
            )
            .await?;
            Ok(true)
        }
        Behaviour::RateLimited(secs) => {
            let head = format!(
                "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nRetry-After: {secs}\r\nConnection: keep-alive\r\n\r\n"
            );
            sock.write_all(head.as_bytes()).await?;
            Ok(true)
        }
        Behaviour::Flaky { code, .. } => {
            // How many failures are left is decided by the caller in `handle`.
            let head = format!(
                "HTTP/1.1 {code} Service Unavailable\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n"
            );
            sock.write_all(head.as_bytes()).await?;
            Ok(true)
        }
        Behaviour::Redirect(n) => {
            if !req.path.starts_with("/hop") {
                // Everything outside the redirect chain is served normally.
                return Box::pin(respond(sock, req, &Behaviour::Normal, size)).await;
            }
            // The depth comes from the path, so the chain really terminates.
            let depth: u16 = req.path.trim_start_matches("/hop/").parse().unwrap_or(*n);
            let next = if depth <= 1 {
                "/media".to_owned()
            } else {
                format!("/hop/{}", depth - 1)
            };
            let head = format!(
                "HTTP/1.1 302 Found\r\nLocation: {next}\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n"
            );
            sock.write_all(head.as_bytes()).await?;
            Ok(true)
        }
        Behaviour::RedirectToPrivate(target) => {
            let head = format!(
                "HTTP/1.1 302 Found\r\nLocation: {target}\r\nContent-Length: 0\r\nConnection: keep-alive\r\n\r\n"
            );
            sock.write_all(head.as_bytes()).await?;
            Ok(true)
        }
        Behaviour::ImmediateDisconnect => {
            // Headers promise a body, then the socket dies with nothing sent.
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nContent-Type: video/mp4\r\n\r\n"
            );
            sock.write_all(head.as_bytes()).await?;
            Ok(false)
        }
        Behaviour::HeadersOnlyThenClose => {
            let head = "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nConnection: close\r\n\r\n";
            sock.write_all(head.as_bytes()).await?;
            Ok(false)
        }
        Behaviour::HeaderFlood(n) => {
            let mut head = String::from("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n");
            for i in 0..*n {
                head.push_str(&format!("X-Pad-{i}: {}\r\n", "a".repeat(200)));
            }
            head.push_str("\r\n");
            sock.write_all(head.as_bytes()).await?;
            Ok(true)
        }
        Behaviour::NotMedia => {
            let body = b"<!DOCTYPE html><html><body>not media</body></html>";
            write_simple(
                sock,
                200,
                "text/html; charset=utf-8",
                body,
                Some(&req.headers),
                true,
            )
            .await?;
            Ok(true)
        }
        Behaviour::OmitContentType => serve_media(sock, req, size, true, false, false).await,
        Behaviour::NoAcceptRanges => serve_media(sock, req, size, true, false, false).await,
        Behaviour::NoContentLength => {
            // Close-delimited body, no Content-Length, Range ignored.
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nAccept-Ranges: none\r\nConnection: close\r\n\r\n")
                .await?;
            write_body(sock, &make_payload(size), 64 * 1024).await?;
            Ok(false)
        }
        Behaviour::IgnoreRange => {
            // Answers every request with the whole body and no range support,
            // exactly like a plain static file server behind a misconfigured CDN.
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nContent-Type: video/mp4\r\nAccept-Ranges: none\r\nConnection: keep-alive\r\n\r\n"
            );
            sock.write_all(head.as_bytes()).await?;
            write_body(sock, &make_payload(size), 64 * 1024).await?;
            Ok(true)
        }
        Behaviour::BrokenContentRange => {
            let head = format!(
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 999999999-999999999/{size}\r\nContent-Length: 1\r\nContent-Type: video/mp4\r\n\r\nX"
            );
            sock.write_all(head.as_bytes()).await?;
            Ok(true)
        }
        Behaviour::WrongContentLength => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: video/mp4\r\n\r\n",
                size * 4
            );
            sock.write_all(head.as_bytes()).await?;
            write_body(sock, &make_payload(size), 64 * 1024).await?;
            Ok(false)
        }
        Behaviour::TruncateAfter(n) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nContent-Type: video/mp4\r\nAccept-Ranges: bytes\r\n\r\n"
            );
            sock.write_all(head.as_bytes()).await?;
            write_body(sock, &make_payload(size), *n).await?;
            Ok(false)
        }
        Behaviour::StallAfter(n) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nContent-Type: video/mp4\r\nAccept-Ranges: bytes\r\n\r\n"
            );
            sock.write_all(head.as_bytes()).await?;
            write_body(sock, &make_payload(size), *n).await?;
            // Never send the rest and never close: this is the "origin stopped
            // mid-transfer" case that must be cut off by the idle timeout.
            std::future::pending::<std::io::Result<bool>>().await
        }
        Behaviour::SlowBody(delay) => {
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {size}\r\nContent-Type: video/mp4\r\nAccept-Ranges: bytes\r\n\r\n"
            );
            sock.write_all(head.as_bytes()).await?;
            let payload = make_payload(size);
            for chunk in payload.chunks(16 * 1024) {
                sock.write_all(chunk).await?;
                sock.flush().await?;
                tokio::time::sleep(*delay).await;
            }
            Ok(true)
        }
        Behaviour::Normal => serve_media(sock, req, size, false, true, true).await,
    }
}

async fn serve_media(
    sock: &mut TcpStream,
    req: &Request,
    size: usize,
    no_accept_ranges: bool,
    with_content_type: bool,
    honour_range: bool,
) -> std::io::Result<bool> {
    let size64 = size as u64;
    let mut head = String::new();
    match req
        .range
        .as_deref()
        .filter(|_| honour_range)
        .and_then(|r| parse_range(r, size64))
    {
        Some((start, end)) => {
            let len = end - start + 1;
            head.push_str("HTTP/1.1 206 Partial Content\r\n");
            head.push_str(&format!("Content-Range: bytes {start}-{end}/{size64}\r\n"));
            head.push_str(&format!("Content-Length: {len}\r\n"));
        }
        None => {
            head.push_str("HTTP/1.1 200 OK\r\n");
            head.push_str(&format!("Content-Length: {size64}\r\n"));
        }
    }
    if with_content_type {
        head.push_str("Content-Type: video/mp4\r\n");
    }
    if !no_accept_ranges {
        head.push_str("Accept-Ranges: bytes\r\n");
    }
    head.push_str("ETag: \"origin-v1\"\r\n");
    head.push_str("Last-Modified: Wed, 21 Oct 2026 07:28:00 GMT\r\n");
    head.push_str("Connection: keep-alive\r\n\r\n");
    sock.write_all(head.as_bytes()).await?;

    let payload = make_payload(size);
    let (from, to) = match req
        .range
        .as_deref()
        .filter(|_| honour_range)
        .and_then(|r| parse_range(r, size64))
    {
        Some((s, e)) => (s as usize, (e as usize + 1).min(size)),
        None => (0, size),
    };
    write_body(sock, &payload[from..to], 64 * 1024).await?;
    Ok(true)
}

/// Minimal `bytes=start-end` parser for the fake origin.
fn parse_range(h: &str, total: u64) -> Option<(u64, u64)> {
    let spec = h.strip_prefix("bytes=")?;
    let (s, e) = spec.split_once('-')?;
    let start: u64 = s.trim().parse().ok()?;
    let end: u64 = match e.trim().parse::<u64>() {
        Ok(v) => v.min(total.saturating_sub(1)),
        Err(_) => total.saturating_sub(1),
    };
    if total == 0 || start >= total || end < start {
        return None;
    }
    Some((start, end))
}

async fn write_simple(
    sock: &mut TcpStream,
    code: u16,
    ctype: &str,
    body: &[u8],
    _req_headers: Option<&HashMap<String, String>>,
    keep_alive: bool,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {code} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
        body.len(),
        if keep_alive { "keep-alive" } else { "close" }
    );
    sock.write_all(head.as_bytes()).await?;
    sock.write_all(body).await?;
    Ok(())
}

async fn write_body(sock: &mut TcpStream, body: &[u8], max: usize) -> std::io::Result<()> {
    for chunk in body.chunks(max) {
        if sock.write_all(chunk).await.is_err() {
            return Ok(());
        }
        if sock.flush().await.is_err() {
            return Ok(());
        }
    }
    Ok(())
}

/// Spin up the real application on an ephemeral port with test-friendly limits.
pub struct TestServer {
    pub base: String,
    pub stats_url: String,
    pub metrics: Arc<ddl_player::metrics::Metrics>,
    pub origin: Origin,
    pub task: tokio::task::JoinHandle<()>,
}

pub async fn start_server(origin: Origin) -> TestServer {
    let cfg = ddl_player::config::Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        static_dir: None,
        max_concurrent_streams: 64,
        // Small windows so multi-window behaviour is exercised in tests.
        stream_buffer_bytes: 128 * 1024,
        read_chunk_bytes: 16 * 1024,
        prefetch_window_bytes: 256 * 1024,
        connect_timeout: Duration::from_secs(2),
        response_timeout: Duration::from_secs(4),
        idle_timeout: Duration::from_millis(700),
        dns_timeout: Duration::from_secs(2),
        retry: ddl_player::retry::RetryPolicy {
            max_attempts: 3,
            base_delay: Duration::from_millis(20),
            max_delay: Duration::from_millis(120),
            jitter: 0.3,
            total_backoff_budget: Duration::from_millis(400),
            max_retry_after: Duration::from_millis(400),
        },
        allow_private_hosts: true,
        ..Default::default()
    };
    let app = ddl_player::build(cfg);
    let metrics = app.state.metrics.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router.clone();
    let shutdown = app.state.shutdown.clone();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    TestServer {
        base: format!("http://{addr}"),
        stats_url: format!("http://{addr}/api/stats"),
        metrics,
        origin,
        task,
    }
}

/// A deployment with the SSRF policy left on (the production default).
pub async fn start_server_strict() -> TestServer {
    let origin = Origin::start(64 * 1024).await;
    let cfg = ddl_player::config::Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        static_dir: None,
        max_concurrent_streams: 64,
        allow_private_hosts: false,
        ..Default::default()
    };
    serve_with(origin, cfg).await
}

/// A deployment with an explicit concurrency ceiling.
pub async fn start_server_with_concurrency(max: usize) -> TestServer {
    let origin = Origin::start(2 * 1024 * 1024).await;
    let cfg = ddl_player::config::Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        static_dir: None,
        max_concurrent_streams: max,
        allow_private_hosts: true,
        idle_timeout: Duration::from_millis(700),
        response_timeout: Duration::from_secs(4),
        retry: ddl_player::retry::RetryPolicy {
            max_attempts: 2,
            base_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(50),
            jitter: 0.2,
            total_backoff_budget: Duration::from_millis(120),
            max_retry_after: Duration::from_millis(120),
        },
        ..Default::default()
    };
    serve_with(origin, cfg).await
}

async fn serve_with(origin: Origin, cfg: ddl_player::config::Config) -> TestServer {
    let app = ddl_player::build(cfg);
    let metrics = app.state.metrics.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = app.router.clone();
    let shutdown = app.state.shutdown.clone();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await;
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    TestServer {
        base: format!("http://{addr}"),
        stats_url: format!("http://{addr}/api/stats"),
        metrics,
        origin,
        task,
    }
}

impl TestServer {
    /// `/api/stream?url=<encoded origin url for path>`
    pub fn stream_url(&self, origin_path: &str) -> String {
        format!(
            "{}/api/stream?url={}",
            self.base,
            url_encode(&self.origin.url(origin_path))
        )
    }

    pub async fn stats(&self) -> serde_json::Value {
        client()
            .get(&self.stats_url)
            .send()
            .await
            .expect("stats request")
            .json::<serde_json::Value>()
            .await
            .expect("stats json")
    }
}

/// Percent-encode a URL for use as a query-string value.
pub fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b':' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// A plain HTTP client for asserting on the proxy's behaviour.
pub fn client() -> reqwest::Client {
    client_with_timeout(Duration::from_secs(20))
}

/// The same, with a longer budget for tests that move a lot of bytes.
pub fn client_with_timeout(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(timeout)
        .build()
        .expect("client")
}
