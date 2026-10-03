//! Retry policy: exponential backoff with jitter, `Retry-After` support, and
//! a hard budget so no request can loop forever.
//!
//! Only genuinely transient failures are retried. `400/401/403/404/416` are
//! terminal: retrying them burns bandwidth and hides the real cause.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use crate::errors::ErrorCode;

/// Tunable retry behaviour.
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// Fraction of the delay that is randomized, e.g. 0.5 => 50-100%.
    pub jitter: f64,
    /// Upper bound on the total time spent sleeping across all attempts.
    pub total_backoff_budget: Duration,
    /// Applied when upstream sends `Retry-After` and it is sane.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 4,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
            jitter: 0.5,
            total_backoff_budget: Duration::from_secs(5),
            max_retry_after: Duration::from_secs(10),
        }
    }
}

/// What to do after a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Wait the given duration before trying again.
    Retry { after: Option<Duration> },
    /// Stop now.
    Fatal,
    /// Stop, but the caller should surface this specific code.
    FatalWith(ErrorCode),
}

impl Decision {
    pub const fn is_retry(self) -> bool {
        matches!(self, Self::Retry { .. })
    }
}

/// Classify an upstream HTTP status.
pub fn decide_status(status: u16, retry_after: Option<Duration>, policy: &RetryPolicy) -> Decision {
    match status {
        429 => Decision::Retry {
            after: retry_after.map(|d| d.min(policy.max_retry_after)),
        },
        500 | 502 | 503 | 504 | 507 | 509 => Decision::Retry { after: None },
        _ => Decision::FatalWith(crate::errors::from_status(status)),
    }
}

/// Classify a transport-level error code.
pub fn decide_error(
    code: ErrorCode,
    retry_after: Option<Duration>,
    policy: &RetryPolicy,
) -> Decision {
    match code {
        ErrorCode::UpstreamError
        | ErrorCode::RequestTimeout
        | ErrorCode::ConnectionTimeout
        | ErrorCode::NetworkInterrupted
        | ErrorCode::CorruptedResponse => Decision::Retry { after: None },
        ErrorCode::RateLimited => Decision::Retry {
            after: retry_after.map(|d| d.min(policy.max_retry_after)),
        },
        other => Decision::FatalWith(other),
    }
}

/// Tracks attempts, elapsed backoff and cancellation for one logical request.
#[derive(Debug)]
pub struct RetryState {
    policy: Arc<RetryPolicy>,
    attempt: u32,
    spent: Duration,
    rng: Xoshiro,
}

impl RetryState {
    pub fn new(policy: Arc<RetryPolicy>, seed: u64) -> Self {
        Self {
            policy,
            attempt: 0,
            spent: Duration::ZERO,
            rng: Xoshiro::seeded(seed),
        }
    }

    pub const fn attempts(&self) -> u32 {
        self.attempt
    }

    pub fn exhausted(&self) -> bool {
        self.attempt >= self.policy.max_attempts
    }

    /// Decide whether to make another attempt and for how long to wait.
    pub fn next(&mut self, decision: Decision) -> Option<Duration> {
        let Decision::Retry { after } = decision else {
            return None;
        };
        if self.exhausted() {
            return None;
        }
        // Backoff budget is a hard ceiling: a hostile origin that keeps
        // answering 503 cannot make us sleep forever.
        if self.spent >= self.policy.total_backoff_budget {
            return None;
        }
        let wait = after.unwrap_or_else(|| self.compute_backoff());
        let wait = wait.min(self.policy.total_backoff_budget - self.spent);
        self.spent += wait;
        self.attempt += 1;
        Some(wait)
    }

    fn compute_backoff(&mut self) -> Duration {
        let p = &self.policy;
        // base * 2^attempt, saturating.
        let shift = self.attempt.min(32);
        let scaled = p
            .base_delay
            .checked_mul(1u32 << shift)
            .unwrap_or(p.max_delay)
            .min(p.max_delay);
        let jitter = if p.jitter <= 0.0 {
            1.0
        } else {
            let unit: f64 = self.rng.next_f64();
            (1.0 - p.jitter) + unit * p.jitter
        };
        Duration::from_secs_f64((scaled.as_secs_f64() * jitter).max(0.0))
    }
}

/// Sleep, but abort immediately if the request is cancelled.
pub async fn sleep_or_cancel(token: &CancellationToken, d: Duration) -> Result<(), ()> {
    if d.is_zero() {
        return Ok(());
    }
    tokio::select! {
        _ = token.cancelled() => Err(()),
        _ = tokio::time::sleep(d) => Ok(()),
    }
}

/// Parse `Retry-After`, which is either delta-seconds or an HTTP-date.
pub fn parse_retry_after(raw: &str, now: SystemTime) -> Option<Duration> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw.bytes().all(|b| b.is_ascii_digit()) {
        return raw.parse::<u64>().ok().map(Duration::from_secs);
    }
    let target = parse_http_date(raw)?;
    // A date in the past means "retry now", not "retry never".
    Some(target.duration_since(now).unwrap_or(Duration::ZERO))
}

/// Parse IMF-fixdate, the only format servers are required to emit.
pub fn parse_http_date(raw: &str) -> Option<SystemTime> {
    const MONTHS: [&str; 12] = [
        "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
    ];
    let s = raw.trim();
    // "Sun, 06 Nov 1994 08:49:37 GMT"
    let rest = s.split_once(", ")?.1;
    let mut it = rest.split(' ');
    let day: u32 = it.next()?.parse().ok()?;
    let mon = it.next()?.to_ascii_lowercase();
    let month = MONTHS.iter().position(|m| mon.starts_with(m))? as u32 + 1;
    let year: i64 = it.next()?.parse().ok()?;
    let time = it.next()?;
    let mut tp = time.split(':');
    let h: u64 = tp.next()?.parse().ok()?;
    let mi: u64 = tp.next()?.parse().ok()?;
    let sec: u64 = tp.next()?.parse().ok()?;

    let days = days_from_civil(year, month, day);
    let secs = days
        .checked_mul(86_400)?
        .checked_add((h * 3600) as i64)?
        .checked_add((mi * 60) as i64)?
        .checked_add(sec as i64)?;
    if secs < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Howard Hinnant's civil-date algorithm; exact for the proleptic Gregorian
/// calendar, no dependency needed for a 6-line table lookup.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Tiny xorshift64* PRNG. We need jitter, not cryptography, and a dependency
/// for this would not be justified.
#[derive(Debug, Clone)]
pub struct Xoshiro(u64);

impl Xoshiro {
    pub fn seeded(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Counters for retry volume, exported as metrics.
#[derive(Debug, Default)]
pub struct RetryCounters {
    pub retries: AtomicU64,
    pub budget_exhausted: AtomicU64,
    pub retry_after_honoured: AtomicU64,
}

impl RetryCounters {
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.retries.load(Ordering::Relaxed),
            self.budget_exhausted.load(Ordering::Relaxed),
            self.retry_after_honoured.load(Ordering::Relaxed),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn transient_statuses_retry_terminal_ones_do_not() {
        let p = RetryPolicy::default();
        for s in [500, 502, 503, 504, 429] {
            assert!(decide_status(s, None, &p).is_retry(), "{s} should retry");
        }
        for s in [400, 401, 403, 404, 416, 400, 405, 410, 451] {
            assert!(!decide_status(s, None, &p).is_retry(), "{s} must not retry");
        }
        assert_eq!(
            decide_status(404, None, &p),
            Decision::FatalWith(ErrorCode::Http404)
        );
        assert_eq!(
            decide_status(403, None, &p),
            Decision::FatalWith(ErrorCode::Http403)
        );
    }

    #[test]
    fn transport_classification() {
        let p = RetryPolicy::default();
        for c in [
            ErrorCode::UpstreamError,
            ErrorCode::RequestTimeout,
            ErrorCode::ConnectionTimeout,
            ErrorCode::NetworkInterrupted,
            ErrorCode::CorruptedResponse,
        ] {
            assert!(decide_error(c, None, &p).is_retry(), "{c}");
        }
        for c in [
            ErrorCode::Http404,
            ErrorCode::InvalidUrl,
            ErrorCode::TlsFailure,
            ErrorCode::DnsFailure,
        ] {
            assert!(!decide_error(c, None, &p).is_retry(), "{c}");
        }
    }

    #[test]
    fn max_attempts_is_enforced() {
        let p = Arc::new(RetryPolicy {
            max_attempts: 3,
            ..Default::default()
        });
        let mut s = RetryState::new(p, 7);
        let d = Decision::Retry {
            after: Some(Duration::ZERO),
        };
        assert_eq!(s.next(d), Some(Duration::ZERO));
        assert_eq!(s.next(d), Some(Duration::ZERO));
        assert_eq!(s.next(d), Some(Duration::ZERO));
        assert_eq!(s.next(d), None);
        assert_eq!(s.attempts(), 3);
    }

    #[test]
    fn fatal_never_retries() {
        let p = Arc::new(RetryPolicy::default());
        let mut s = RetryState::new(p, 1);
        assert_eq!(s.next(Decision::Fatal), None);
        assert_eq!(s.attempts(), 0);
    }

    #[test]
    fn backoff_grows_and_is_bounded() {
        let p = Arc::new(RetryPolicy {
            max_attempts: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
            jitter: 0.0,
            total_backoff_budget: Duration::from_secs(60),
            max_retry_after: Duration::from_secs(5),
        });
        let mut s = RetryState::new(p, 42);
        let d = Decision::Retry { after: None };
        let waits: Vec<_> = (0..6).filter_map(|_| s.next(d)).collect();
        assert_eq!(
            waits,
            vec![
                Duration::from_millis(100),
                Duration::from_millis(200),
                Duration::from_millis(400),
                Duration::from_millis(800),
                Duration::from_millis(1000),
                Duration::from_millis(1000),
            ]
        );
    }

    #[test]
    fn jitter_stays_within_bounds_and_varies() {
        let p = Arc::new(RetryPolicy {
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(2),
            jitter: 0.5,
            ..Default::default()
        });
        let mut seen = std::collections::HashSet::new();
        for seed in 0..200u64 {
            let mut s = RetryState::new(p.clone(), seed * 0x9E37_79B9 + 1);
            let w = s.next(Decision::Retry { after: None }).unwrap();
            let ms = w.as_millis();
            assert!((50..=100).contains(&ms), "attempt 0 out of range: {ms}ms");
            seen.insert(w.as_nanos());
        }
        assert!(
            seen.len() > 100,
            "jitter should vary, got {} values",
            seen.len()
        );
    }

    #[test]
    fn total_backoff_budget_is_a_hard_ceiling() {
        let p = Arc::new(RetryPolicy {
            max_attempts: 1000,
            base_delay: Duration::from_secs(5),
            max_delay: Duration::from_secs(30),
            jitter: 0.0,
            total_backoff_budget: Duration::from_secs(3),
            max_retry_after: Duration::from_secs(60),
        });
        let mut s = RetryState::new(p, 3);
        let d = Decision::Retry { after: None };
        let mut total = Duration::ZERO;
        while let Some(w) = s.next(d) {
            total += w;
        }
        assert!(total <= Duration::from_secs(3), "slept {total:?}");
    }

    #[test]
    fn retry_after_overrides_backoff_and_is_capped() {
        let p = Arc::new(RetryPolicy {
            max_retry_after: Duration::from_secs(5),
            ..Default::default()
        });
        let mut s = RetryState::new(p, 9);
        let w = s.next(Decision::Retry {
            after: Some(Duration::from_secs(600)),
        });
        assert_eq!(w, Some(Duration::from_secs(5)));
    }

    #[test]
    fn retry_after_parsing() {
        let now = UNIX_EPOCH + Duration::from_secs(784_111_777); // 1994-11-06T08:49:37Z
        assert_eq!(
            parse_retry_after("120", now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(parse_retry_after("0", now), Some(Duration::ZERO));
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:51:37 GMT", now),
            Some(Duration::from_secs(120))
        );
        // A date in the past clamps to zero rather than underflowing.
        assert_eq!(
            parse_retry_after("Sun, 06 Nov 1994 08:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("", now), None);
        assert_eq!(parse_retry_after("later", now), None);
        assert_eq!(parse_retry_after("-5", now), None);
        assert_eq!(parse_retry_after("99999999999999999999", now), None);
        assert_eq!(
            parse_retry_after("Sun, 32 Xxx 1994 08:00:00 GMT", now),
            None
        );
    }

    #[test]
    fn http_date_round_trip_is_exact() {
        for (s, expected) in [
            ("Thu, 01 Jan 1970 00:00:00 GMT", 0i64),
            ("Sun, 06 Nov 1994 08:49:37 GMT", 784_111_777),
            ("Tue, 29 Feb 2000 12:00:00 GMT", 951_825_600),
            ("Fri, 01 Jan 2100 00:00:00 GMT", 4_102_444_800),
        ] {
            let t = parse_http_date(s).expect(s);
            assert_eq!(
                t.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64,
                expected,
                "{s}"
            );
        }
    }

    #[tokio::test]
    async fn cancellation_interrupts_backoff() {
        let token = CancellationToken::new();
        let child = token.clone();
        let t = tokio::spawn(async move { sleep_or_cancel(&child, Duration::from_secs(30)).await });
        tokio::time::sleep(Duration::from_millis(10)).await;
        token.cancel();
        let started = std::time::Instant::now();
        assert_eq!(t.await.unwrap(), Err(()));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn zero_delay_sleep_is_free() {
        let token = CancellationToken::new();
        assert!(sleep_or_cancel(&token, Duration::ZERO).await.is_ok());
    }
}
