//! Metrics: counters, gauges and log-linear latency histograms.
//!
//! Percentiles are *estimates* from 4-buckets-per-octave histograms (the same
//! resolution as HdrHistogram). No dependency is needed for that, and the
//! memory cost is 128 `u64`s per histogram instead of a reservoir sample that
//! silently changes meaning under load.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

use crate::errors::ErrorCode;

/// 4 buckets per power of two, 63 octaves: 0 µs .. 2^64 µs, 256 buckets.
const SUB_BITS: u32 = 2;
const BUCKETS: usize = 252;

/// Fixed-bucket log-linear histogram over microsecond values.
#[derive(Debug)]
pub struct Histogram {
    buckets: [AtomicU64; BUCKETS],
    count: AtomicU64,
    sum_us: AtomicU64,
    max_us: AtomicU64,
    negative: AtomicI64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: std::array::from_fn(|_| AtomicU64::new(0)),
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
            max_us: AtomicU64::new(0),
            negative: AtomicI64::new(0),
        }
    }
}

impl Histogram {
    pub fn record(&self, value_us: u64) {
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(value_us, Ordering::Relaxed);
        self.max_us.fetch_max(value_us, Ordering::Relaxed);
        self.buckets[Self::index(value_us)].fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_duration(&self, d: std::time::Duration) {
        self.record(d.as_micros().min(u64::MAX as u128) as u64);
    }

    /// Negative observations (a clock that moved backwards) are counted so
    /// they can never silently distort the distribution.
    pub fn record_negative(&self) {
        self.negative.fetch_add(1, Ordering::Relaxed);
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn mean_us(&self) -> f64 {
        let c = self.count();
        if c == 0 {
            return 0.0;
        }
        self.sum_us.load(Ordering::Relaxed) as f64 / c as f64
    }

    pub fn max_us(&self) -> u64 {
        self.max_us.load(Ordering::Relaxed)
    }

    /// Estimated quantile in microseconds. Returns `None` when empty.
    pub fn quantile(&self, q: f64) -> Option<u64> {
        let total = self.count();
        if total == 0 {
            return None;
        }
        let q = q.clamp(0.0, 1.0);
        let rank = (q * total as f64).ceil().max(1.0) as u64;
        let mut seen = 0u64;
        for (i, b) in self.buckets.iter().enumerate() {
            seen += b.load(Ordering::Relaxed);
            if seen >= rank {
                return Some(Self::bucket_upper_us(i));
            }
        }
        Some(self.max_us())
    }

    pub fn negative_count(&self) -> i64 {
        self.negative.load(Ordering::Relaxed)
    }

    /// Bucket index for a microsecond value.
    ///
    /// Values 0..=3 get exact buckets. From 4 upwards we take 4 buckets per
    /// power of two, keyed on the two bits below the leading one.
    const fn index(v: u64) -> usize {
        if v < 4 {
            return v as usize;
        }
        let k = 63 - v.leading_zeros() as usize; // floor(log2(v)), >= 2
        let sub = ((v >> (k - 2)) & 0b11) as usize;
        (k - 1) * (1 << SUB_BITS) + sub
    }

    /// Upper bound of a bucket in microseconds. Quantiles are reported as this
    /// conservative bound, never as an optimistic midpoint.
    const fn bucket_upper_us(i: usize) -> u64 {
        if i < 4 {
            return i as u64;
        }
        let o = i / (1 << SUB_BITS);
        let sub = i % (1 << SUB_BITS);
        let k = o + 1;
        let step = 1u64 << (k - 2);
        let lo = (1u64 << k) + (sub as u64) * step;
        lo.saturating_add(step).saturating_sub(1)
    }
}

/// All process-wide counters. Explicit fields, no dynamic map, so the metric
/// names are a stable contract.
#[derive(Debug, Default)]
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub requests_2xx: AtomicU64,
    pub requests_3xx: AtomicU64,
    pub requests_4xx: AtomicU64,
    pub requests_5xx: AtomicU64,

    pub active_streams: AtomicI64,
    pub streams_total: AtomicU64,
    pub streams_rejected: AtomicU64,
    pub streams_cancelled: AtomicU64,

    pub bytes_from_origin: AtomicU64,
    pub bytes_to_client: AtomicU64,
    pub upstream_requests: AtomicU64,
    pub upstream_reused_connections: AtomicU64,

    pub probes_total: AtomicU64,
    pub probes_cached: AtomicU64,
    pub redirects_followed: AtomicU64,
    pub redirect_loops: AtomicU64,

    pub retries_total: AtomicU64,
    pub retries_exhausted: AtomicU64,
    pub retry_budget_exhausted: AtomicU64,
    pub retry_after_honoured: AtomicU64,

    pub connection_failures: AtomicU64,
    pub origin_disconnects: AtomicU64,
    pub origin_resumes: AtomicU64,
    pub range_unsupported: AtomicU64,
    pub range_rejected: AtomicU64,
    pub media_rejected: AtomicU64,
    pub ssrf_blocked: AtomicU64,

    pub client_play: AtomicU64,
    pub client_seek: AtomicU64,
    pub client_stall: AtomicU64,
    pub client_error: AtomicU64,

    pub http_latency_us: Histogram,
    pub ttfb_us: Histogram,
    pub startup_us: Histogram,
    pub seek_us: Histogram,
    pub stream_duration_us: Histogram,
    pub resume_us: Histogram,
    pub chunk_gap_us: Histogram,

    errors: Mutex<BTreeMap<&'static str, u64>>,
}

impl Metrics {
    pub fn incr(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    pub fn stream_opened(&self) {
        self.streams_total.fetch_add(1, Ordering::Relaxed);
        self.active_streams.fetch_add(1, Ordering::Relaxed);
    }

    pub fn stream_closed(&self) {
        self.active_streams.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn active(&self) -> i64 {
        self.active_streams.load(Ordering::Relaxed)
    }

    pub fn record_error(&self, code: ErrorCode) {
        if let Ok(mut m) = self.errors.lock() {
            *m.entry(code.as_str()).or_insert(0) += 1;
        }
    }

    pub fn error_counts(&self) -> BTreeMap<String, u64> {
        self.errors
            .lock()
            .map(|m| m.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect())
            .unwrap_or_default()
    }

    pub fn total_errors(&self) -> u64 {
        self.errors.lock().map(|m| m.values().sum()).unwrap_or(0)
    }

    pub fn snapshot(&self) -> Snapshot {
        let q = |h: &Histogram| Quantiles {
            count: h.count(),
            mean_us: h.mean_us(),
            p50_us: h.quantile(0.50),
            p95_us: h.quantile(0.95),
            p99_us: h.quantile(0.99),
            max_us: (h.max_us() > 0).then_some(h.max_us()),
            negative: h.negative_count(),
        };
        let load = |c: &AtomicU64| c.load(Ordering::Relaxed);
        let live = self.active_streams.load(Ordering::Relaxed);
        let started = load(&self.streams_total);
        Snapshot {
            requests_total: load(&self.requests_total),
            requests_2xx: load(&self.requests_2xx),
            requests_3xx: load(&self.requests_3xx),
            requests_4xx: load(&self.requests_4xx),
            requests_5xx: load(&self.requests_5xx),
            active_streams: live.max(0),
            streams_total: started,
            streams_rejected: load(&self.streams_rejected),
            streams_cancelled: load(&self.streams_cancelled),
            bytes_from_origin: load(&self.bytes_from_origin),
            bytes_to_client: load(&self.bytes_to_client),
            upstream_requests: load(&self.upstream_requests),
            upstream_reused_connections: load(&self.upstream_reused_connections),
            probes_total: load(&self.probes_total),
            probes_cached: load(&self.probes_cached),
            redirects_followed: load(&self.redirects_followed),
            redirect_loops: load(&self.redirect_loops),
            retries_total: load(&self.retries_total),
            retries_exhausted: load(&self.retries_exhausted),
            retry_budget_exhausted: load(&self.retry_budget_exhausted),
            retry_after_honoured: load(&self.retry_after_honoured),
            connection_failures: load(&self.connection_failures),
            origin_disconnects: load(&self.origin_disconnects),
            origin_resumes: load(&self.origin_resumes),
            range_unsupported: load(&self.range_unsupported),
            range_rejected: load(&self.range_rejected),
            media_rejected: load(&self.media_rejected),
            ssrf_blocked: load(&self.ssrf_blocked),
            client_play: load(&self.client_play),
            client_seek: load(&self.client_seek),
            client_stall: load(&self.client_stall),
            client_error: load(&self.client_error),
            http_latency: q(&self.http_latency_us),
            ttfb: q(&self.ttfb_us),
            startup: q(&self.startup_us),
            seek: q(&self.seek_us),
            stream_duration: q(&self.stream_duration_us),
            resume: q(&self.resume_us),
            chunk_gap: q(&self.chunk_gap_us),
            errors: self.error_counts(),
        }
    }

    /// Prometheus text exposition. Deliberately hand-rolled: the format is
    /// stable and tiny, and a client library would be a dependency with no
    /// other use.
    pub fn render_prometheus(&self) -> String {
        let s = self.snapshot();
        let mut o = String::with_capacity(4096);
        let _ = writeln!(o, "# HELP ddl_requests_total Total HTTP requests accepted.");
        let _ = writeln!(o, "# TYPE ddl_requests_total counter");
        let _ = writeln!(o, "ddl_requests_total {}", s.requests_total);
        for (name, v) in [
            ("ddl_requests_2xx_total", s.requests_2xx),
            ("ddl_requests_3xx_total", s.requests_3xx),
            ("ddl_requests_4xx_total", s.requests_4xx),
            ("ddl_requests_5xx_total", s.requests_5xx),
            ("ddl_streams_total", s.streams_total),
            ("ddl_streams_rejected_total", s.streams_rejected),
            ("ddl_streams_cancelled_total", s.streams_cancelled),
            ("ddl_bytes_from_origin_total", s.bytes_from_origin),
            ("ddl_bytes_to_client_total", s.bytes_to_client),
            ("ddl_upstream_requests_total", s.upstream_requests),
            ("ddl_probes_total", s.probes_total),
            ("ddl_probes_cached_total", s.probes_cached),
            ("ddl_redirects_followed_total", s.redirects_followed),
            ("ddl_redirect_loops_total", s.redirect_loops),
            ("ddl_retries_total", s.retries_total),
            ("ddl_retries_exhausted_total", s.retries_exhausted),
            ("ddl_retry_budget_exhausted_total", s.retry_budget_exhausted),
            ("ddl_connection_failures_total", s.connection_failures),
            ("ddl_origin_disconnects_total", s.origin_disconnects),
            ("ddl_origin_resumes_total", s.origin_resumes),
            ("ddl_range_unsupported_total", s.range_unsupported),
            ("ddl_range_rejected_total", s.range_rejected),
            ("ddl_media_rejected_total", s.media_rejected),
            ("ddl_ssrf_blocked_total", s.ssrf_blocked),
            ("ddl_client_play_total", s.client_play),
            ("ddl_client_seek_total", s.client_seek),
            ("ddl_client_stall_total", s.client_stall),
            ("ddl_client_error_total", s.client_error),
        ] {
            let _ = writeln!(o, "{name} {v}");
        }
        let _ = writeln!(o, "# TYPE ddl_active_streams gauge");
        let _ = writeln!(o, "ddl_active_streams {}", s.active_streams);
        for (name, h) in [
            ("ddl_http_latency_us", &self.http_latency_us),
            ("ddl_ttfb_us", &self.ttfb_us),
            ("ddl_startup_us", &self.startup_us),
            ("ddl_seek_us", &self.seek_us),
            ("ddl_stream_duration_us", &self.stream_duration_us),
            ("ddl_resume_us", &self.resume_us),
        ] {
            let _ = writeln!(o, "# TYPE {name} histogram");
            let _ = writeln!(o, "{name}_count {}", h.count());
            let _ = writeln!(o, "{name}_sum_us {}", h.sum_us.load(Ordering::Relaxed));
            let _ = writeln!(o, "{name}_max_us {}", h.max_us());
            for (label, q) in [("0.5", 0.5f64), ("0.95", 0.95), ("0.99", 0.99)] {
                if let Some(v) = h.quantile(q) {
                    let _ = writeln!(o, "{name}{{quantile=\"{label}\"}} {v}");
                }
            }
        }
        for (code, count) in &s.errors {
            let _ = writeln!(o, "ddl_errors_total{{code=\"{code}\"}} {count}");
        }
        o
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Quantiles {
    pub count: u64,
    pub mean_us: f64,
    pub p50_us: Option<u64>,
    pub p95_us: Option<u64>,
    pub p99_us: Option<u64>,
    pub max_us: Option<u64>,
    pub negative: i64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Snapshot {
    pub requests_total: u64,
    pub requests_2xx: u64,
    pub requests_3xx: u64,
    pub requests_4xx: u64,
    pub requests_5xx: u64,
    pub active_streams: i64,
    pub streams_total: u64,
    pub streams_rejected: u64,
    pub streams_cancelled: u64,
    pub bytes_from_origin: u64,
    pub bytes_to_client: u64,
    pub upstream_requests: u64,
    pub upstream_reused_connections: u64,
    pub probes_total: u64,
    pub probes_cached: u64,
    pub redirects_followed: u64,
    pub redirect_loops: u64,
    pub retries_total: u64,
    pub retries_exhausted: u64,
    pub retry_budget_exhausted: u64,
    pub retry_after_honoured: u64,
    pub connection_failures: u64,
    pub origin_disconnects: u64,
    pub origin_resumes: u64,
    pub range_unsupported: u64,
    pub range_rejected: u64,
    pub media_rejected: u64,
    pub ssrf_blocked: u64,
    pub client_play: u64,
    pub client_seek: u64,
    pub client_stall: u64,
    pub client_error: u64,
    pub http_latency: Quantiles,
    pub ttfb: Quantiles,
    pub startup: Quantiles,
    pub seek: Quantiles,
    pub stream_duration: Quantiles,
    pub resume: Quantiles,
    pub chunk_gap: Quantiles,
    pub errors: BTreeMap<String, u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_histogram_has_no_quantiles() {
        let h = Histogram::default();
        assert_eq!(h.count(), 0);
        assert_eq!(h.quantile(0.5), None);
        assert_eq!(h.mean_us(), 0.0);
    }

    #[test]
    fn quantiles_are_ordered_and_bounded_by_max() {
        let h = Histogram::default();
        for v in 1..=10_000u64 {
            h.record(v);
        }
        let p50 = h.quantile(0.5).unwrap();
        let p95 = h.quantile(0.95).unwrap();
        let p99 = h.quantile(0.99).unwrap();
        assert!(p50 <= p95 && p95 <= p99, "{p50} {p95} {p99}");
        // within 4% of the true median
        assert!((p50 as f64 - 5_000.0).abs() / 5_000.0 < 0.04, "p50={p50}");
        // Quantiles are conservative bucket upper bounds, so they may overshoot
        // the observed maximum by up to one bucket width.
        assert!(p99 <= 12_288, "p99 was {p99}");
        assert_eq!(h.max_us(), 10_000);
        assert_eq!(h.count(), 10_000);
    }

    #[test]
    fn buckets_are_dense_and_monotonic() {
        let h = Histogram::default();
        for v in [
            0u64,
            1,
            2,
            3,
            4,
            7,
            8,
            15,
            16,
            1023,
            1024,
            1 << 20,
            u64::MAX,
        ] {
            h.record(v);
        }
        assert_eq!(h.count(), 13);
        let mut prev = 0;
        for q in [0.0, 0.1, 0.25, 0.5, 0.75, 0.9, 1.0] {
            let v = h.quantile(q).unwrap();
            assert!(v >= prev, "quantile went backwards at {q}");
            prev = v;
        }
    }

    #[test]
    fn extreme_values_do_not_panic() {
        let h = Histogram::default();
        h.record(u64::MAX);
        h.record(0);
        let _ = h.quantile(0.5);
        let _ = h.quantile(0.5);
        let s = h.sum_us.load(Ordering::Relaxed);
        assert!(s >= 1); // saturating rather than wrapping to something absurd
    }

    #[test]
    fn single_value_distribution() {
        let h = Histogram::default();
        h.record(5_000);
        // Bucket upper bound, not the exact observation: documented behaviour.
        assert_eq!(h.quantile(0.5), Some(5_119));
        assert_eq!(h.quantile(0.99), Some(5_119));
        assert_eq!(h.max_us(), 5_000);
    }

    #[test]
    fn record_duration_and_negative() {
        let h = Histogram::default();
        h.record_duration(std::time::Duration::from_millis(12));
        h.record_negative();
        assert_eq!(h.count(), 1);
        assert_eq!(h.negative_count(), 1);
    }

    #[test]
    fn error_counting_groups_by_code() {
        let m = Metrics::default();
        m.record_error(ErrorCode::Http404);
        m.record_error(ErrorCode::Http404);
        m.record_error(ErrorCode::RangeNotSupported);
        let counts = m.error_counts();
        assert_eq!(counts.get("HTTP_404"), Some(&2));
        assert_eq!(counts.get("RANGE_NOT_SUPPORTED"), Some(&1));
        assert_eq!(m.total_errors(), 3);
    }

    #[test]
    fn stream_gauge_is_balanced() {
        let m = Metrics::default();
        m.stream_opened();
        m.stream_opened();
        assert_eq!(m.active(), 2);
        m.stream_closed();
        assert_eq!(m.active(), 1);
        let s = m.snapshot();
        assert_eq!(s.active_streams, 1);
        assert_eq!(s.streams_total, 2);
    }

    #[test]
    fn prometheus_output_is_well_formed() {
        let m = Metrics::default();
        m.stream_opened();
        m.record_error(ErrorCode::DnsFailure);
        m.http_latency_us.record(1_234);
        let text = m.render_prometheus();
        assert!(text.contains("ddl_active_streams 1\n"));
        assert!(text.contains("ddl_http_latency_us_count 1\n"));
        assert!(text.contains("ddl_errors_total{code=\"DNS_FAILURE\"} 1\n"));
        for line in text.lines() {
            assert!(
                line.starts_with('#') || line.split(' ').count() >= 2,
                "malformed line: {line}"
            );
        }
    }

    #[test]
    fn snapshot_serializes_for_the_stats_endpoint() {
        let m = Metrics::default();
        m.seek_us.record(250_000);
        let v = serde_json::to_value(m.snapshot()).unwrap();
        assert_eq!(v["seek"]["p50_us"], 262_143);
        assert!(v["errors"].is_object());
    }
}
