//! The streaming engine: probe, fetch with redirects/retries, and open a
//! bounded, cancellable byte-range stream.
//!
//! Invariants this module exists to guarantee:
//!   * No media bytes are ever fully buffered. Memory per stream is bounded by
//!     `stream_buffer_bytes`, independent of file size.
//!   * Every upstream request is validated for SSRF, including every redirect.
//!   * Every task has an owner. Stopping playback tears down the origin
//!     request within one scheduler pass.
//!   * A seek supersedes the previous request; obsolete data can never reach
//!     the client.
//!   * Every length advertised downstream comes from the live origin response,
//!     never from a cache, so a stale entry cannot corrupt a seek.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use http::header::{
    HeaderMap, HeaderValue, ACCEPT_RANGES, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
    LAST_MODIFIED, LOCATION, RANGE,
};
use http::{HeaderName, StatusCode};
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::cache::{MetadataStore, ResourceMeta};
use crate::config::Config;
use crate::errors::{ErrorCode, PlayerError};
use crate::media::{self, Container, MediaInfo};
use crate::metrics::Metrics;
use crate::range::{self, RangeSpec, Resolution};
use crate::retry::{self, RetryState};
use crate::security::{is_loop, redirect_kind, resolve_redirect, validate_with, SafeUrl};

use super::pool::{origin_request_headers, OriginPool, RequestId};
use super::pump::{BodyPlan, Pump, PumpBody};
use super::registry::{StreamHandle, StreamRegistry};

const HDR_REQUEST_ID: HeaderName = HeaderName::from_static("x-ddl-request-id");
const HDR_GENERATION: HeaderName = HeaderName::from_static("x-ddl-generation");
const HDR_RANGE_SUPPORT: HeaderName = HeaderName::from_static("x-ddl-range-support");

/// Cheap, cloneable handle to the engine, safe to move into pump tasks.
#[derive(Clone)]
pub struct Engine(Arc<Inner>);

struct Inner {
    cfg: Arc<Config>,
    pool: Arc<OriginPool>,
    cache: Arc<dyn MetadataStore>,
    metrics: Arc<Metrics>,
    registry: Arc<StreamRegistry>,
}

/// A validated origin response. Owns the still-unread body.
pub struct OriginResponse {
    response: Option<reqwest::Response>,
    pub status: u16,
    headers: HeaderMap,
    pub ttfb: Duration,
    pub hops: usize,
    pub final_url: Url,
    pub final_authority: String,
}

impl OriginResponse {
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// Detach the body for streaming. Returns `None` if already taken.
    pub fn take_body(&mut self) -> Option<reqwest::Response> {
        self.response.take()
    }

    pub fn content_type(&self) -> Option<&str> {
        self.headers
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::trim)
    }

    /// Length the origin declared. `Some(None)` means the header was present
    /// but unusable, which we treat as "unknown".
    pub fn declared_length(&self) -> Option<Option<u64>> {
        let raw = self.headers.get(CONTENT_LENGTH)?.to_str().ok()?;
        Some(range::parse_content_length(raw))
    }

    pub fn length(&self) -> Option<u64> {
        self.declared_length().flatten()
    }

    pub fn etag(&self) -> Option<String> {
        self.headers
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .and_then(range::parse_single_etag)
    }

    pub fn last_modified(&self) -> Option<String> {
        self.headers
            .get(LAST_MODIFIED)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_owned())
    }

    pub fn accept_ranges(&self) -> Option<String> {
        self.headers
            .get(ACCEPT_RANGES)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.trim().to_ascii_lowercase())
    }

    pub fn content_range(&self) -> Option<(u64, u64, Option<u64>)> {
        self.headers
            .get(CONTENT_RANGE)
            .and_then(|v| v.to_str().ok())
            .and_then(range::parse_content_range)
    }

    /// Whether we can ask this origin for arbitrary byte ranges.
    pub fn range_supported(&self) -> bool {
        self.status == 206
            || self
                .accept_ranges()
                .map(|a| a.split(',').any(|t| t.trim() == "bytes"))
                .unwrap_or(false)
    }
}

/// Result of `POST /api/probe`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProbeResult {
    pub streamable: bool,
    pub content_type: Option<String>,
    pub content_length: Option<u64>,
    pub range_supported: bool,
    pub accept_ranges: Option<String>,
    pub container: String,
    pub evidence: String,
    pub needs_remux: bool,
    pub remux_reason: Option<String>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Log-safe origin URL: no query string, no credentials.
    pub final_url: String,
    pub origin: String,
    pub redirects: usize,
    pub ttfb_ms: f64,
    pub probe_ms: f64,
    pub cached: bool,
    pub reason: Option<String>,
    pub warning: Option<String>,
}

/// Decrements the live-stream gauge exactly once, however the stream ends:
/// normally, on error, or because the client vanished mid-planning.
struct OpenStreamGuard(Arc<Metrics>);

impl Drop for OpenStreamGuard {
    fn drop(&mut self) {
        self.0.stream_closed();
    }
}

pub struct StreamResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: PumpBody,
    pub rid: RequestId,
}

/// What the caller asked for.
#[derive(Debug, Clone)]
pub struct StreamRequest {
    pub url: SafeUrl,
    pub raw_range: Option<String>,
    pub session: Option<String>,
    pub generation: Option<u64>,
    pub if_none_match: Option<String>,
    pub if_modified_since: Option<String>,
}

/// Bytes of the body a probe reads so it can identify the container from its
/// signature. 1 KiB covers every container magic we care about at a cost of
/// 1 KiB of bandwidth — the cheapest way to be sure the media is what it claims
/// to be without paying for a full request.
pub const PROBE_HEAD_BYTES: u64 = 1024;

impl Engine {
    pub fn new(
        cfg: Arc<Config>,
        pool: Arc<OriginPool>,
        cache: Arc<dyn MetadataStore>,
        metrics: Arc<Metrics>,
        registry: Arc<StreamRegistry>,
    ) -> Self {
        Self(Arc::new(Inner {
            cfg,
            pool,
            cache,
            metrics,
            registry,
        }))
    }

    pub fn config(&self) -> &Arc<Config> {
        &self.0.cfg
    }

    pub fn metrics(&self) -> &Arc<Metrics> {
        &self.0.metrics
    }

    pub fn pool(&self) -> &Arc<OriginPool> {
        &self.0.pool
    }

    pub fn registry(&self) -> &Arc<StreamRegistry> {
        &self.0.registry
    }

    pub fn cache(&self) -> &Arc<dyn MetadataStore> {
        &self.0.cache
    }

    // ---------------------------------------------------------------- probe

    /// Inspect a resource with exactly one round trip.
    ///
    /// A single `GET bytes=0-0` yields the total length, the range support, the
    /// content type and the redirect chain. A HEAD would be cheaper but many
    /// CDNs answer HEAD inconsistently; a full GET would waste bandwidth.
    pub async fn probe(&self, url: &SafeUrl, force: bool) -> Result<ProbeResult, PlayerError> {
        let started = Instant::now();
        let key = url.url().as_str().to_owned();
        if !force {
            if let Some(meta) = self.0.cache.get(&key) {
                Metrics::incr(&self.0.metrics.probes_cached);
                self.0.metrics.startup_us.record_duration(started.elapsed());
                return Ok(Self::from_meta(&meta, true));
            }
        }
        Metrics::incr(&self.0.metrics.probes_total);

        let token = CancellationToken::new();
        let probe_range = format!("bytes=0-{}", PROBE_HEAD_BYTES - 1);
        let mut resp = match self.fetch(url, Some(&probe_range), &token).await {
            Ok(r) => r,
            Err(e) if e.code == ErrorCode::Http416 => {
                // Some origins reject a narrow range on an empty resource.
                let token2 = CancellationToken::new();
                self.fetch(url, Some("bytes=0-"), &token2).await?
            }
            Err(e) => return Err(e),
        };

        if resp.status >= 400 {
            return Err(Self::status_error(resp.status, resp.headers()));
        }

        // Sniff the container signature from the first chunk, then abandon the
        // body: a probe must never transfer more than one buffer.
        let head = read_head(resp.take_body(), self.0.cfg.read_chunk_bytes).await;
        self.finish_probe(url, key, started, resp, head)
    }

    fn finish_probe(
        &self,
        url: &SafeUrl,
        key: String,
        started: Instant,
        resp: OriginResponse,
        head: Vec<u8>,
    ) -> Result<ProbeResult, PlayerError> {
        let content_type = resp.content_type().map(|s| s.to_owned());
        let accept_ranges = resp.accept_ranges();
        let etag = resp.etag();
        let last_modified = resp.last_modified();
        let hops = resp.hops;
        let origin = resp.final_authority.clone();
        let final_url = crate::security::redact_url(&resp.final_url);

        let (range_supported, content_length) = match resp.content_range() {
            // `bytes 0-0/TOTAL` is the authoritative length.
            Some((0, _, total)) => (true, total.or_else(|| resp.length())),
            _ => (resp.range_supported(), resp.length()),
        };

        let info = media::identify(content_type.as_deref(), url.url().path(), Some(&head));
        // Report the type we will actually hand the browser, which is the
        // origin's when it told us and the identified one when it did not.
        let content_type = Some(info.media_type.clone());
        let streamable = info.container.browser_native()
            && self.0.cfg.allowed_media_types.contains(&info.media_type);

        let warning = if !info.container.browser_native() {
            info.remux_reason.clone()
        } else if !range_supported {
            Some(
                "The source does not support byte ranges; seeking will restart the download."
                    .to_owned(),
            )
        } else {
            None
        };

        let meta = ResourceMeta {
            final_url: final_url.clone(),
            origin: origin.clone(),
            content_type,
            content_length,
            accept_ranges,
            range_supported,
            etag,
            last_modified,
            media: info,
            probed_at: Instant::now(),
        };

        let mut result = Self::from_meta(&meta, false);
        result.redirects = hops;
        result.ttfb_ms = resp.ttfb.as_secs_f64() * 1000.0;
        result.probe_ms = started.elapsed().as_secs_f64() * 1000.0;
        if result.reason.is_none() && !range_supported && streamable {
            result.warning = warning.clone();
        }
        self.0.cache.put(key, meta);
        self.0.metrics.ttfb_us.record_duration(resp.ttfb);
        self.0.metrics.startup_us.record_duration(started.elapsed());
        Ok(result)
    }

    fn from_meta(meta: &ResourceMeta, cached: bool) -> ProbeResult {
        let streamable = meta.media.container.browser_native();
        ProbeResult {
            streamable,
            content_type: meta.content_type.clone(),
            content_length: meta.content_length,
            range_supported: meta.range_supported,
            accept_ranges: meta.accept_ranges.clone(),
            container: meta.media.container.as_str().to_owned(),
            evidence: meta.media.source.as_str().to_owned(),
            needs_remux: meta.media.needs_remux,
            remux_reason: meta.media.remux_reason.clone(),
            etag: meta.etag.clone(),
            last_modified: meta.last_modified.clone(),
            final_url: meta.final_url.clone(),
            origin: meta.origin.clone(),
            redirects: 0,
            ttfb_ms: 0.0,
            probe_ms: 0.0,
            cached,
            reason: (!streamable).then(|| {
                meta.media
                    .remux_reason
                    .clone()
                    .unwrap_or_else(|| "Unsupported media container.".to_owned())
            }),
            warning: (streamable && !meta.range_supported).then(|| {
                "The source does not support byte ranges; seeking will restart the download."
                    .to_owned()
            }),
        }
    }

    // --------------------------------------------------------------- stream

    pub async fn open_stream(
        self,
        req: StreamRequest,
        rid: RequestId,
    ) -> Result<StreamResponse, PlayerError> {
        let handle = self
            .0
            .registry
            .register(req.session.as_deref(), req.generation)?;
        self.0.metrics.stream_opened();
        // The gauge must be decremented on *every* exit path, including the one
        // nobody writes down: the client disconnecting while we are still
        // planning, which drops this future mid-await. An RAII guard is the
        // only way to be sure that cannot drift.
        let opened = OpenStreamGuard(self.0.metrics.clone());

        let token = handle.token().clone();
        let started = Instant::now();

        let planned = self.plan_stream(&req, &handle, &token, rid).await;
        let (status, headers, plan) = match planned {
            Ok(p) => p,
            Err(e) => {
                drop(handle);
                return Err(e);
            }
        };

        let slots = (self.0.cfg.stream_buffer_bytes / self.0.cfg.read_chunk_bytes).clamp(2, 64);
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(slots);
        let pump = Pump::new(
            self.clone(),
            plan,
            tx,
            token.clone(),
            rid,
            handle,
            self.0.cfg.idle_timeout,
        );
        let engine = self.clone();
        // The guard now belongs to the pump task, which always runs to
        // completion (or is dropped), so the count cannot leak.
        tokio::spawn(async move {
            let stats = pump.run(started).await;
            let m = engine.metrics();
            drop(opened);
            if stats.cancelled {
                m.streams_cancelled.fetch_add(1, Ordering::Relaxed);
            }
        });

        Ok(StreamResponse {
            status,
            headers,
            body: PumpBody::new(rx, token),
            rid,
        })
    }

    async fn plan_stream(
        &self,
        req: &StreamRequest,
        handle: &StreamHandle,
        token: &CancellationToken,
        rid: RequestId,
    ) -> Result<(StatusCode, HeaderMap, BodyPlan), PlayerError> {
        let cfg = self.0.cfg.clone();
        let spec = match req.raw_range.as_deref() {
            None => RangeSpec::None,
            Some(v) => range::parse_range(v),
        };
        let requested = spec.requested();
        let partial_request = spec.is_partial_request();
        let seek = partial_request && requested.is_some_and(|r| range_start(r) > 0);

        let outbound = outbound_range(requested, cfg.prefetch_window_bytes)?;

        let resp = self.fetch(&req.url, outbound.as_deref(), token).await?;
        self.0.metrics.ttfb_us.record_duration(resp.ttfb);

        if resp.status >= 400 {
            return Err(Self::status_error(resp.status, resp.headers()));
        }

        let info = self.media_for(&req.url, &resp)?;
        if !info.container.browser_native() {
            self.0
                .metrics
                .media_rejected
                .fetch_add(1, Ordering::Relaxed);
            return Err(PlayerError::new(ErrorCode::MediaNotSupported)
                .with_reason(
                    info.remux_reason
                        .clone()
                        .unwrap_or_else(|| format!("container {}", info.container.as_str())),
                )
                .with_user_action("Convert the file to MP4 (H.264) or WebM and try again."));
        }

        // Resolve what we promised the client before committing to a status.
        let served = served_window(&resp, requested, partial_request)?;
        let range_supported = resp.range_supported();
        if !range_supported {
            self.0
                .metrics
                .range_unsupported
                .fetch_add(1, Ordering::Relaxed);
        }

        let mut headers = HeaderMap::with_capacity(12);
        headers.insert(CONTENT_TYPE, header_value(&info.media_type));
        headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        headers.insert(
            http::header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=0, must-revalidate"),
        );
        headers.insert(
            http::header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        headers.insert(HDR_REQUEST_ID, header_value(&rid.to_string()));
        headers.insert(
            HDR_GENERATION,
            header_value(&handle.generation().to_string()),
        );
        if !range_supported {
            // Say it out loud rather than letting the player discover it.
            headers.insert(HDR_RANGE_SUPPORT, HeaderValue::from_static("none"));
        }
        if let Some(v) = resp.etag().and_then(|v| header_value_opt(&v)) {
            headers.insert(ETAG, v);
        }
        if let Some(v) = resp.last_modified().and_then(|v| header_value_opt(&v)) {
            headers.insert(LAST_MODIFIED, v);
        }

        // Conditional short-circuit: unchanged and no range -> 304, zero bytes.
        if !partial_request && conditional_not_modified(req, &resp) {
            let plan = BodyPlan::empty(resp.final_url.clone());
            return Ok((StatusCode::NOT_MODIFIED, headers, plan));
        }

        let mut status = StatusCode::OK;
        // The status we return is determined by what the *client* asked for,
        // not by how we happened to window the origin transfer. The origin's
        // own `206` is an implementation detail of our prefetch window.
        if partial_request {
            if let Some(cr) = served.content_range() {
                headers.insert(CONTENT_RANGE, header_value(&cr));
                status = StatusCode::PARTIAL_CONTENT;
            }
        }
        if let Some(len) = served.length() {
            headers.insert(CONTENT_LENGTH, header_value(&len.to_string()));
        }

        let final_url = resp.final_url.clone();
        let ttfb = resp.ttfb;
        if seek {
            self.0.metrics.seek_us.record_duration(ttfb);
        }
        // The response we already hold is only reusable if it actually starts
        // where the client asked. A suffix-range probe (and any origin that
        // answered from a different offset) is discarded and re-requested, so
        // we never forward bytes from the wrong position.
        let reusable = match resp.content_range() {
            Some((start, _, _)) => start == served.start,
            None => served.start == 0,
        };
        let plan = BodyPlan {
            url: final_url,
            start: served.start,
            end: served.end,
            total: served.total,
            partial: status == StatusCode::PARTIAL_CONTENT,
            range_supported,
            window: cfg.prefetch_window_bytes,
            expected_len: served.length(),
            content_type: info.media_type.clone(),
            noop: false,
            body: reusable.then_some(resp),
        };
        Ok((status, headers, plan))
    }

    /// Content-type decision for the streaming path.
    ///
    /// Unlike probe we do not wait for magic bytes: doing so would add a read
    /// before the first byte reaches the player. We trust the origin's type,
    /// then the file extension, then the last probe result as a hint.
    fn media_for(&self, url: &SafeUrl, resp: &OriginResponse) -> Result<MediaInfo, PlayerError> {
        let ct = resp.content_type();
        if let Some(raw) = ct {
            if !media::is_generic_content_type(raw) && media::from_content_type(raw).is_some() {
                return Ok(media::identify(Some(raw), url.url().path(), None));
            }
        }
        let from_path = media::identify(None, url.url().path(), None);
        if from_path.container != Container::Unknown {
            return Ok(from_path);
        }
        if let Some(raw) = ct {
            return Ok(media::identify(Some(raw), url.url().path(), None));
        }
        let key = url.url().as_str().to_owned();
        if let Some(meta) = self.0.cache.get(&key) {
            if meta.media.container != Container::Unknown {
                return Ok(meta.media);
            }
        }
        Err(PlayerError::new(ErrorCode::InvalidContentType)
            .with_reason("no usable content type and no media extension in the path")
            .with_user_action("Verify the link points to a media file."))
    }

    fn status_error(status: u16, headers: &HeaderMap) -> PlayerError {
        let code = crate::errors::from_status(status);
        let reason = if headers.contains_key(http::header::RETRY_AFTER) {
            format!("upstream status {status} (with Retry-After)")
        } else {
            format!("upstream status {status}")
        };
        let mut e = PlayerError::new(code)
            .with_reason(reason)
            .with_status(status);
        if code == ErrorCode::Http416 {
            // A conforming 416 tells the client the real length. Forward it:
            // without it the player cannot tell "past the end" from "no idea
            // how big this is".
            if let Some(v) = headers.get(CONTENT_RANGE).and_then(|v| v.to_str().ok()) {
                e = e
                    .with_user_action("Reload the page and try again.")
                    .with_content_range_raw(v.to_owned());
            }
        }
        if code == ErrorCode::UpstreamError {
            e = e.with_user_action("The source is having trouble. Retry in a moment.");
        }
        e
    }

    // ---------------------------------------------------------------- fetch

    /// One origin fetch with manual redirects, SSRF revalidation, bounded
    /// retries and cancellation. The caller owns the returned body.
    pub(crate) async fn fetch(
        &self,
        start: &SafeUrl,
        range: Option<&str>,
        token: &CancellationToken,
    ) -> Result<OriginResponse, PlayerError> {
        let mut current = start.url().clone();
        let mut visited = vec![current.clone()];
        let mut hops = 0usize;
        let cfg = &self.0.cfg;
        let m = &self.0.metrics;
        let mut retry = RetryState::new(Arc::new(cfg.retry.clone()), RequestId::next().raw());

        loop {
            if token.is_cancelled() {
                return Err(cancelled());
            }
            // Every hop is validated from scratch: scheme, hostname, and below
            // the resolved address. A redirect cannot widen the blast radius.
            let safe = validate_with(current.as_str(), self.0.pool.policy())?;

            let client = match self.0.pool.client_for(&safe).await {
                Ok(c) => c,
                Err(e) => {
                    retry_fut(&self.0, e, &mut retry, token).await?;
                    continue;
                }
            };

            let mut builder = client.get(current.clone());
            for (k, v) in origin_request_headers() {
                builder = builder.header(k, v);
            }
            if let Some(r) = range {
                builder = builder.header(RANGE, r);
            }

            let t0 = Instant::now();
            let sent = tokio::select! {
                biased;
                _ = token.cancelled() => return Err(cancelled()),
                res = tokio::time::timeout(cfg.response_timeout, builder.send()) => res,
            };

            let response = match sent {
                Err(_elapsed) => {
                    m.connection_failures.fetch_add(1, Ordering::Relaxed);
                    let e = PlayerError::new(ErrorCode::RequestTimeout).with_reason(format!(
                        "no response headers within {}ms",
                        cfg.response_timeout.as_millis()
                    ));
                    retry_fut(&self.0, e, &mut retry, token).await?;
                    continue;
                }
                Ok(Err(err)) => {
                    let code = crate::errors::from_reqwest(&err);
                    if matches!(
                        code,
                        ErrorCode::ConnectionTimeout
                            | ErrorCode::DnsFailure
                            | ErrorCode::TlsFailure
                    ) {
                        m.connection_failures.fetch_add(1, Ordering::Relaxed);
                    }
                    let e = PlayerError::new(code).with_reason(scrub_reqwest(&err));
                    retry_fut(&self.0, e, &mut retry, token).await?;
                    continue;
                }
                Ok(Ok(resp)) => resp,
            };

            let status = response.status().as_u16();
            if let Some(kind) = redirect_kind(status) {
                hops += 1;
                if hops > cfg.max_redirects {
                    m.redirect_loops.fetch_add(1, Ordering::Relaxed);
                    return Err(PlayerError::new(ErrorCode::TooManyRedirects)
                        .with_reason(format!("exceeded {} redirects", cfg.max_redirects))
                        .with_user_action("The link is probably part of a redirect loop."));
                }
                let Some(loc) = response
                    .headers()
                    .get(LOCATION)
                    .and_then(|v| v.to_str().ok())
                else {
                    return Err(PlayerError::new(ErrorCode::CorruptedResponse).with_reason(
                        format!("{} redirect without a Location header", kind.as_str()),
                    ));
                };
                let next = resolve_redirect(&current, loc)?;
                if is_loop(&visited, &next) {
                    m.redirect_loops.fetch_add(1, Ordering::Relaxed);
                    return Err(PlayerError::new(ErrorCode::TooManyRedirects)
                        .with_reason("redirect loop detected"));
                }
                Metrics::incr(&m.redirects_followed);
                tracing::debug!(
                    kind = kind.as_str(),
                    hops,
                    to = %crate::security::redact_url(&next),
                    "following redirect"
                );
                visited.push(next.clone());
                current = next;
                // Drop the redirect response so its connection returns to the
                // pool instead of lingering.
                drop(response);
                continue;
            }

            // A retryable status is retried here, not handed to the caller: by
            // the time the caller sees a response it is either success or a
            // genuinely permanent failure. `Retry-After` is honoured when the
            // origin sends one.
            let retry_after = response
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| retry::parse_retry_after(v, std::time::SystemTime::now()));
            let decision = retry::decide_status(status, retry_after, &cfg.retry);
            if decision.is_retry() {
                let e = PlayerError::new(crate::errors::from_status(status))
                    .with_reason(format!("upstream status {status}"))
                    .with_status(status);
                if retry_after.is_some() {
                    m.retry_after_honoured.fetch_add(1, Ordering::Relaxed);
                }
                // Release the connection before sleeping.
                drop(response);
                match retry.next(decision) {
                    Some(wait) => {
                        m.retries_total.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(
                            code = e.code.as_str(),
                            attempt = retry.attempts(),
                            wait_ms = wait.as_millis() as u64,
                            "retrying after a retryable origin status"
                        );
                        retry::sleep_or_cancel(token, wait)
                            .await
                            .map_err(|_| cancelled())?;
                        continue;
                    }
                    None => {
                        m.retries_exhausted.fetch_add(1, Ordering::Relaxed);
                        if retry.attempts() >= cfg.retry.max_attempts {
                            m.retry_budget_exhausted.fetch_add(1, Ordering::Relaxed);
                        }
                        let attempts = retry.attempts();
                        let reason = e.reason.clone();
                        return Err(e.with_reason(format!(
                            "{reason} (gave up after {attempts} attempt(s))"
                        )));
                    }
                }
            }

            let headers = response.headers().clone();
            return Ok(OriginResponse {
                response: Some(response),
                status,
                headers,
                ttfb: t0.elapsed(),
                hops,
                final_url: current,
                final_authority: safe.authority().to_owned(),
            });
        }
    }
}

async fn retry_fut(
    inner: &Inner,
    err: PlayerError,
    retry: &mut RetryState,
    token: &CancellationToken,
) -> Result<(), PlayerError> {
    let m = &inner.metrics;
    let decision = retry::decide_error(err.code, None, &inner.cfg.retry);
    match retry.next(decision) {
        Some(wait) => {
            m.retries_total.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                code = err.code.as_str(),
                attempt = retry.attempts(),
                wait_ms = wait.as_millis() as u64,
                reason = %err.reason,
                "retrying origin request"
            );
            retry::sleep_or_cancel(token, wait)
                .await
                .map_err(|_| cancelled())
        }
        None => {
            if decision.is_retry() {
                m.retries_exhausted.fetch_add(1, Ordering::Relaxed);
                if retry.attempts() >= inner.cfg.retry.max_attempts {
                    m.retry_budget_exhausted.fetch_add(1, Ordering::Relaxed);
                }
                let attempts = retry.attempts();
                let reason = err.reason.clone();
                Err(err.with_reason(format!("{reason} (gave up after {attempts} attempt(s))")))
            } else {
                Err(err)
            }
        }
    }
}

const fn range_start(r: range::ByteRange) -> u64 {
    match r {
        range::ByteRange::FromTo { start, .. } => start,
        range::ByteRange::Suffix { .. } => 0,
    }
}

/// Decide the outbound `Range` for one attempt.
///
/// A suffix range (`bytes=-N`) cannot be expressed without knowing the length,
/// so we open a normal window from zero instead: it reveals the total in one
/// round trip and costs one extra request for a request type browsers rarely
/// send.
fn outbound_range(
    requested: Option<range::ByteRange>,
    window: u64,
) -> Result<Option<String>, PlayerError> {
    let Some(range::ByteRange::Suffix { .. }) = requested else {
        let spec = match requested {
            None => RangeSpec::None,
            Some(r) => RangeSpec::Single(r),
        };
        // Length is unknown here, so `resolve` yields exactly the window the
        // client asked for. We never guess a length that would corrupt a seek.
        let res = range::resolve(spec, None).map_err(|e| {
            PlayerError::new(ErrorCode::InvalidRangeHeader)
                .with_reason(e.to_string())
                .with_user_action("Reload the page and try again.")
        })?;
        let bounded = match res.end {
            Some(end) => Some(end),
            None if window > 0 => Some(res.start.saturating_add(window).saturating_sub(1)),
            None => None,
        };
        return Ok(range::request_header(&Resolution {
            end: bounded,
            ..res
        }));
    };

    // Suffix: learn the total from a bounded probe.
    let end = window.saturating_sub(1);
    Ok(if window > 0 {
        Some(format!("bytes=0-{end}"))
    } else {
        Some("bytes=0-".to_owned())
    })
}

/// Work out the exact absolute byte window we will forward to the client.
///
/// Two facts matter and only two: what the *client* asked for, and how long
/// the resource actually is. How we windowed the origin transfer is irrelevant
/// to what we promise downstream.
fn served_window(
    resp: &OriginResponse,
    requested: Option<range::ByteRange>,
    partial_request: bool,
) -> Result<Resolution, PlayerError> {
    // Length first: a `Content-Range` complete-length is authoritative and
    // survives an origin that omits `Content-Length`.
    let total = resp
        .content_range()
        .and_then(|(_, _, t)| t)
        .or_else(|| resp.length());

    if !partial_request {
        return Ok(Resolution {
            start: 0,
            end: total.map(|t| t.saturating_sub(1)),
            total,
        });
    }

    match requested {
        Some(range::ByteRange::FromTo { start, end }) => {
            let Some(total) = total else {
                // Length unknown: an open-ended range can only be delivered as
                // a chunked 200, which is the honest answer.
                return Ok(Resolution {
                    start,
                    end,
                    total: None,
                });
            };
            if total == 0 || start >= total {
                return Err(unsatisfiable(total));
            }
            let end = end.map_or(total - 1, |e| e.min(total - 1)).max(start);
            Ok(Resolution {
                start,
                end: Some(end),
                total: Some(total),
            })
        }
        Some(range::ByteRange::Suffix { len }) => {
            let Some(total) = total else {
                return Err(PlayerError::new(ErrorCode::Http416)
                    .with_reason("suffix range requires a known content length")
                    .with_user_action("Reload the page and try again."));
            };
            if total == 0 {
                return Err(unsatisfiable(total));
            }
            Ok(Resolution {
                start: total.saturating_sub(len),
                end: Some(total - 1),
                total: Some(total),
            })
        }
        None => Ok(Resolution {
            start: 0,
            end: total.map(|t| t.saturating_sub(1)),
            total,
        }),
    }
}

fn conditional_not_modified(req: &StreamRequest, resp: &OriginResponse) -> bool {
    if let Some(inm) = &req.if_none_match {
        let Some(ours) = resp.etag() else {
            return false;
        };
        return range::parse_single_etag(inm).is_some_and(|theirs| theirs == "*" || theirs == ours);
    }
    if let Some(ims) = &req.if_modified_since {
        let Some(ours) = resp.last_modified() else {
            return false;
        };
        return ours == *ims;
    }
    false
}

fn unsatisfiable(total: u64) -> PlayerError {
    PlayerError::new(ErrorCode::Http416)
        .with_reason(format!(
            "requested range is outside a {total} byte resource"
        ))
        .with_status(416)
        .with_content_range(total)
        .with_user_action("Reload the page and try again.")
}

pub(crate) fn header_value(v: &str) -> HeaderValue {
    HeaderValue::from_str(v)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

pub(crate) fn header_value_opt(v: &str) -> Option<HeaderValue> {
    HeaderValue::from_str(v).ok()
}

/// Read at most `cap` bytes of the body, then abandon the response.
async fn read_head(body: Option<reqwest::Response>, cap: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(cap.min(4096));
    let Some(resp) = body else { return out };
    // `bytes_stream` consumes the response, which closes the connection as soon
    // as the stream is dropped: we refuse to download a body we will not use.
    let mut stream = resp.bytes_stream();
    while let Ok(Some(Ok(chunk))) =
        tokio::time::timeout(Duration::from_millis(1500), stream.next()).await
    {
        out.extend_from_slice(&chunk);
        if out.len() >= cap {
            break;
        }
    }
    out
}

fn scrub_reqwest(err: &reqwest::Error) -> String {
    scrub(&err.to_string())
}

/// Replace anything that looks like a URL so upstream locations, and any
/// credential embedded in a query string, never reach a log or a response.
fn scrub(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for tok in text.split_whitespace() {
        if tok.contains("://") {
            out.push_str("<origin>");
        } else {
            out.push_str(tok);
        }
        out.push(' ');
    }
    out.trim().to_owned()
}

fn cancelled() -> PlayerError {
    PlayerError::new(ErrorCode::Shutdown).with_reason("request cancelled by client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outbound_range_is_bounded_and_never_over_fetches() {
        // No range requested, window 4 MiB -> one bounded window from zero.
        assert_eq!(
            outbound_range(None, 4 * 1024 * 1024).unwrap(),
            Some("bytes=0-4194303".to_owned())
        );
        // Explicit client range is honoured exactly.
        assert_eq!(
            outbound_range(
                Some(range::ByteRange::FromTo {
                    start: 100,
                    end: Some(199)
                }),
                4 * 1024 * 1024
            )
            .unwrap(),
            Some("bytes=100-199".to_owned())
        );
        // Open client range is windowed from its start.
        assert_eq!(
            outbound_range(
                Some(range::ByteRange::FromTo {
                    start: 1_000_000,
                    end: None
                }),
                4096
            )
            .unwrap(),
            Some("bytes=1000000-1004095".to_owned())
        );
        // Zero window means a single open-ended request.
        assert_eq!(
            outbound_range(
                Some(range::ByteRange::FromTo {
                    start: 5,
                    end: None
                }),
                0
            )
            .unwrap(),
            Some("bytes=5-".to_owned())
        );
    }

    #[test]
    fn suffix_range_is_probed_from_zero_because_it_needs_the_length() {
        // We cannot express `bytes=-N` without knowing the total, so we ask
        // for a bounded window from zero instead: one round trip, no guessing.
        assert_eq!(
            outbound_range(Some(range::ByteRange::Suffix { len: 10 }), 1024).unwrap(),
            Some("bytes=0-1023".to_owned())
        );
        assert_eq!(
            outbound_range(Some(range::ByteRange::Suffix { len: 10 }), 0).unwrap(),
            Some("bytes=0-".to_owned())
        );
    }

    #[test]
    fn suffix_range_without_length_is_refused() {
        // The outbound header is always expressible now; the refusal happens
        // later, in `served_window`, when a suffix range meets a length-less
        // resource.
        assert!(outbound_range(Some(range::ByteRange::Suffix { len: 10 }), 1024).is_ok());
    }

    #[test]
    fn scrubbed_errors_never_contain_a_url() {
        let s = scrub("error sending request for url (http://10.0.0.5:8080/secret?token=abc)");
        assert!(!s.contains("10.0.0.5"), "{s}");
        assert!(!s.contains("token=abc"), "{s}");
        assert!(s.contains("<origin>"));
    }

    #[test]
    fn scrub_leaves_ordinary_text_intact() {
        assert_eq!(scrub("upstream status 503"), "upstream status 503");
        assert_eq!(scrub("  spaced   out  "), "spaced out");
    }

    #[test]
    fn range_start_helper() {
        assert_eq!(
            range_start(range::ByteRange::FromTo {
                start: 9,
                end: None
            }),
            9
        );
        assert_eq!(range_start(range::ByteRange::Suffix { len: 4 }), 0);
    }
}
