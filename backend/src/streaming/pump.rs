//! The byte pump.
//!
//! One task per active stream, and exactly one. It owns:
//!   * the concurrency permit,
//!   * the cancellation token,
//!   * the origin responses,
//!   * the bounded channel to the client.
//!
//! It exits when the client's range is satisfied, when the origin ends, when
//! the client goes away, or when a newer seek supersedes it. There is no path
//! that leaves it running.
//!
//! Chunks are forwarded as `Bytes` and never copied: a full pass over a 20 GB
//! file moves exactly as many bytes as the wire did.

use std::pin::Pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::errors::{ErrorCode, PlayerError};
use crate::retry::{self, RetryState};
use crate::security::validate_with;

use super::engine::{Engine, OriginResponse};
use super::pool::RequestId;
use super::registry::StreamHandle;

type ByteStream = Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>;

/// Exactly what the client was told, and exactly which bytes we may send.
pub struct BodyPlan {
    /// URL after the redirect chain.
    pub url: Url,
    /// Absolute offset of the first byte we may forward.
    pub start: u64,
    /// Inclusive last byte we may forward. `None` means "until origin EOF".
    pub end: Option<u64>,
    /// Total resource length when known.
    pub total: Option<u64>,
    /// Whether we promised the client a `206`.
    pub partial: bool,
    /// Whether the origin honours byte ranges at all.
    pub range_supported: bool,
    /// Upper bound on a single origin response, keeping connections reusable.
    pub window: u64,
    /// Length we told the client via `Content-Length`, if any.
    pub expected_len: Option<u64>,
    pub content_type: String,
    /// `304 Not Modified`: there is nothing to do.
    pub noop: bool,
    /// First origin response, already fetched while building the headers.
    pub body: Option<OriginResponse>,
}

impl BodyPlan {
    pub fn empty(url: Url) -> Self {
        Self {
            url,
            start: 0,
            end: Some(0),
            total: Some(0),
            partial: false,
            range_supported: false,
            window: 0,
            expected_len: None,
            content_type: "application/octet-stream".to_owned(),
            noop: true,
            body: None,
        }
    }
}

/// Terminal state of a stream, for metrics and logs.
#[derive(Debug, Clone, Copy, Default)]
pub struct PumpStats {
    pub bytes: u64,
    pub origin_bytes: u64,
    pub windows: u64,
    pub resumes: u64,
    pub cancelled: bool,
    pub completed: bool,
}

/// The client-facing body. Dropping it cancels the entire stream.
pub struct PumpBody {
    rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
    token: CancellationToken,
}

impl PumpBody {
    pub fn new(
        rx: mpsc::Receiver<Result<Bytes, std::io::Error>>,
        token: CancellationToken,
    ) -> Self {
        Self { rx, token }
    }
}

impl Stream for PumpBody {
    type Item = Result<Bytes, std::io::Error>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl Drop for PumpBody {
    fn drop(&mut self) {
        // Client hung up: seek, pause, navigate away, close the tab.
        // Cancel before the pump can issue another origin request.
        self.token.cancel();
    }
}

pub struct Pump {
    engine: Engine,
    plan: BodyPlan,
    tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
    token: CancellationToken,
    rid: RequestId,
    /// Held for the whole pump lifetime: the concurrency permit is released
    /// exactly when upstream work ends, never before.
    _handle: StreamHandle,
    idle: Duration,
}

impl Pump {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        engine: Engine,
        plan: BodyPlan,
        tx: mpsc::Sender<Result<Bytes, std::io::Error>>,
        token: CancellationToken,
        rid: RequestId,
        handle: StreamHandle,
        idle: Duration,
    ) -> Self {
        Self {
            engine,
            plan,
            tx,
            token,
            rid,
            _handle: handle,
            idle,
        }
    }

    pub async fn run(mut self, started: Instant) -> PumpStats {
        let mut stats = PumpStats::default();
        if self.plan.noop {
            drop(self.tx);
            return stats;
        }

        let metrics = self.engine.metrics().clone();
        let target_end = self.plan.end;
        let single_pass = !self.plan.range_supported;
        let mut next = self.plan.start;
        let mut resume_state =
            RetryState::new(Arc::new(self.engine.config().retry.clone()), self.rid.raw());
        let mut pending: Option<OriginResponse> = self.plan.body.take();

        'outer: loop {
            if self.token.is_cancelled() {
                stats.cancelled = true;
                break;
            }
            if let Some(e) = target_end {
                if next > e {
                    stats.completed = true;
                    break;
                }
            }

            // Window selection: always bounded per origin response so the
            // connection is fully consumed and reusable, and never more than
            // the client asked for. Zero over-fetch. Only a range-less origin
            // gets one unrestricted read, because it cannot express a window.
            let want_end: Option<u64> = if single_pass {
                target_end
            } else {
                match (self.plan.total, self.plan.window) {
                    (Some(t), w) if w > 0 => {
                        let last = t.saturating_sub(1);
                        Some(next.saturating_add(w).saturating_sub(1).min(last))
                    }
                    _ => target_end,
                }
            };
            let outbound = match want_end {
                Some(e) => Some(format!("bytes={next}-{e}")),
                None if next == 0 => None,
                None => Some(format!("bytes={next}-")),
            };

            let resp = match pending.take() {
                Some(r) => r,
                None => {
                    let safe =
                        match validate_with(self.plan.url.as_str(), self.engine.pool().policy()) {
                            Ok(s) => s,
                            Err(e) => return self.finish_bad(e, stats).await,
                        };
                    let fetched = tokio::select! {
                        biased;
                        _ = self.token.cancelled() => {
                            stats.cancelled = true;
                            break 'outer;
                        }
                        r = self.engine.fetch(&safe, outbound.as_deref(), &self.token) => r,
                    };
                    match fetched {
                        Ok(r) => r,
                        Err(e) => {
                            metrics.connection_failures.fetch_add(1, Ordering::Relaxed);
                            metrics.origin_disconnects.fetch_add(1, Ordering::Relaxed);
                            if stats.windows == 0 {
                                return self.finish_bad(e, stats).await;
                            }
                            // Mid-stream: resume from `next` within budget.
                            let decision =
                                retry::decide_error(e.code, None, &self.engine.config().retry);
                            match resume_state.next(decision) {
                                Some(wait) => {
                                    metrics.retries_total.fetch_add(1, Ordering::Relaxed);
                                    if retry::sleep_or_cancel(&self.token, wait).await.is_err() {
                                        stats.cancelled = true;
                                        break;
                                    }
                                    stats.resumes += 1;
                                    metrics.origin_resumes.fetch_add(1, Ordering::Relaxed);
                                    continue;
                                }
                                None => {
                                    let _ = self
                                        .tx
                                        .send(Err(io_error("origin stream failed", e)))
                                        .await;
                                    break;
                                }
                            }
                        }
                    }
                }
            };

            let window_start = Instant::now();
            stats.windows += 1;

            if resp.status == 416 {
                let e = PlayerError::new(ErrorCode::Http416)
                    .with_reason("the requested range no longer exists at the origin");
                let _ = self
                    .tx
                    .send(Err(io_error("range no longer satisfiable", e)))
                    .await;
                break;
            }
            if resp.status >= 400 {
                let e = PlayerError::new(crate::errors::from_status(resp.status))
                    .with_reason(format!("origin status {} mid-stream", resp.status));
                let _ = self
                    .tx
                    .send(Err(io_error("origin error mid-stream", e)))
                    .await;
                break;
            }

            // Validate the origin's range claim before forwarding any byte.
            let declared = resp.declared_length().flatten();
            match resp.content_range() {
                Some((start, end, _)) => {
                    if start != next && self.plan.range_supported {
                        let e = PlayerError::new(ErrorCode::CorruptedResponse).with_reason(
                            format!("origin returned a range starting at {start}, expected {next}"),
                        );
                        let _ = self.tx.send(Err(io_error("bad content-range", e))).await;
                        break;
                    }
                    stats.origin_bytes += end.saturating_sub(start).saturating_add(1);
                }
                None => stats.origin_bytes += declared.unwrap_or(0),
            }

            // A range-less origin restarts from zero on every response, so the
            // bytes we already passed have to be read and thrown away.
            let mut skip = if self.plan.range_supported { 0 } else { next };
            let mut resp = resp;
            let mut stream: Option<ByteStream> = resp
                .take_body()
                .map(|r| Box::pin(r.bytes_stream()) as ByteStream);
            let expected = declared;
            let mut received: u64 = 0;
            let truncated;

            loop {
                let polled = tokio::select! {
                    biased;
                    _ = self.token.cancelled() => {
                        stats.cancelled = true;
                        break 'outer;
                    }
                    r = tokio::time::timeout(self.idle, next_chunk(&mut stream)) => r,
                };
                let item = match polled {
                    Err(_stalled) => {
                        metrics.origin_disconnects.fetch_add(1, Ordering::Relaxed);
                        let e = PlayerError::new(ErrorCode::NetworkInterrupted)
                            .with_reason(format!("origin stalled for {}ms", self.idle.as_millis()));
                        if self.plan.range_supported {
                            truncated = true;
                            break;
                        }
                        let _ = self.tx.send(Err(io_error("origin stalled", e))).await;
                        break 'outer;
                    }
                    Ok(None) => {
                        truncated = expected.is_some_and(|exp| received < exp);
                        break;
                    }
                    Ok(Some(Err(e))) => {
                        metrics.origin_disconnects.fetch_add(1, Ordering::Relaxed);
                        let code = crate::errors::from_reqwest(&e);
                        if self.plan.range_supported {
                            truncated = true;
                            break;
                        }
                        let _ = self
                            .tx
                            .send(Err(io_error("origin body error", PlayerError::new(code))))
                            .await;
                        break 'outer;
                    }
                    Ok(Some(Ok(chunk))) => chunk,
                };

                received += item.len() as u64;
                metrics.chunk_gap_us.record_duration(window_start.elapsed());

                let mut b = item;
                if skip > 0 {
                    if skip >= b.len() as u64 {
                        skip -= b.len() as u64;
                        continue;
                    }
                    b = b.slice(skip as usize..);
                    skip = 0;
                }
                if let Some(end) = target_end {
                    let room = end.saturating_sub(next).saturating_add(1) as usize;
                    if room < b.len() {
                        b = b.slice(0..room);
                    }
                }
                if b.is_empty() {
                    continue;
                }
                next += b.len() as u64;
                stats.bytes += b.len() as u64;

                if self.tx.send(Ok(b)).await.is_err() {
                    // Client is gone: stop now, do not fetch another window.
                    stats.cancelled = true;
                    break 'outer;
                }
            }

            if self.token.is_cancelled() {
                stats.cancelled = true;
                break;
            }
            if target_end.is_some_and(|e| next > e) {
                stats.completed = true;
                break;
            }
            if self.plan.total.is_some_and(|t| next >= t) {
                stats.completed = true;
                break;
            }
            if truncated && !self.plan.range_supported {
                let e = PlayerError::new(ErrorCode::NetworkInterrupted)
                    .with_reason("origin truncated a resource that cannot be ranged");
                let _ = self.tx.send(Err(io_error("origin truncated", e))).await;
                break;
            }
            if truncated {
                stats.resumes += 1;
                metrics.origin_resumes.fetch_add(1, Ordering::Relaxed);
                metrics.resume_us.record_duration(window_start.elapsed());
                tracing::debug!(
                    request_id = %self.rid,
                    offset = next,
                    "resuming origin stream after truncation"
                );
            }
            if single_pass {
                stats.completed = true;
                break;
            }
        }

        let elapsed = started.elapsed();
        metrics.stream_duration_us.record_duration(elapsed);
        metrics
            .bytes_from_origin
            .fetch_add(stats.origin_bytes, Ordering::Relaxed);
        metrics
            .bytes_to_client
            .fetch_add(stats.bytes, Ordering::Relaxed);

        tracing::debug!(
            request_id = %self.rid,
            bytes = stats.bytes,
            origin_bytes = stats.origin_bytes,
            windows = stats.windows,
            resumes = stats.resumes,
            cancelled = stats.cancelled,
            completed = stats.completed,
            duration_ms = elapsed.as_millis() as u64,
            "stream finished"
        );

        // Dropping the sender signals a clean end of body to hyper.
        drop(self.tx);
        stats
    }

    async fn finish_bad(self, e: PlayerError, mut stats: PumpStats) -> PumpStats {
        let metrics = self.engine.metrics().clone();
        metrics.record_error(e.code);
        tracing::warn!(
            request_id = %self.rid,
            code = e.code.as_str(),
            reason = %e.reason,
            "stream start failed"
        );
        let _ = self
            .tx
            .send(Err(io_error("stream could not start", e)))
            .await;
        drop(self.tx);
        stats.cancelled = true;
        stats
    }
}

/// One item from the origin stream, or `None` once it is exhausted.
async fn next_chunk(stream: &mut Option<ByteStream>) -> Option<reqwest::Result<Bytes>> {
    match stream.as_mut() {
        None => None,
        Some(s) => s.next().await,
    }
}

fn io_error(context: &str, e: PlayerError) -> std::io::Error {
    // This reaches the browser as a transport failure, not as body content.
    std::io::Error::other(format!("{context}: {} ({})", e.code, e.reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::NullStore;
    use crate::config::Config;
    use crate::metrics::Metrics;
    use crate::streaming::pool::OriginPool;
    use crate::streaming::registry::StreamRegistry;

    fn test_engine(max: usize) -> (Engine, Arc<StreamRegistry>) {
        let cfg = Arc::new(Config::default());
        let pool = Arc::new(OriginPool::new(&cfg));
        let metrics = Arc::new(Metrics::default());
        let registry = StreamRegistry::new(max);
        let engine = Engine::new(cfg, pool, Arc::new(NullStore), metrics, registry.clone());
        (engine, registry)
    }

    #[test]
    fn body_plan_drop_cancels_the_token() {
        let token = CancellationToken::new();
        let (_tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
        let body = PumpBody::new(rx, token.clone());
        assert!(!token.is_cancelled());
        drop(body);
        assert!(token.is_cancelled());
    }

    #[test]
    fn io_errors_do_not_leak_upstream_detail() {
        let e = io_error(
            "origin stalled",
            PlayerError::new(ErrorCode::NetworkInterrupted).with_reason("stalled 20000ms"),
        );
        let s = e.to_string();
        assert!(s.contains("NETWORK_INTERRUPTED"));
        assert!(!s.contains("http://"));
    }

    #[tokio::test]
    async fn noop_pump_sends_nothing_and_releases_its_permit() {
        let (engine, registry) = test_engine(1);
        assert_eq!(registry.stats().in_flight, 0);
        let handle = registry.register(None, None).unwrap();
        assert_eq!(registry.stats().in_flight, 1);
        let (tx, mut rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(1);
        let token = CancellationToken::new();
        let pump = Pump::new(
            engine,
            BodyPlan::empty(Url::parse("https://a.example/b.mp4").unwrap()),
            tx,
            token,
            RequestId::next(),
            handle,
            Duration::from_secs(1),
        );
        let stats = pump.run(Instant::now()).await;
        assert_eq!(stats.bytes, 0);
        assert!(rx.recv().await.is_none());
        assert_eq!(registry.stats().in_flight, 0, "permit must be released");
    }
}
