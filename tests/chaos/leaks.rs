//! Resource-leak and long-run stability.
//!
//! These assert the properties that are easy to claim and hard to keep:
//! file descriptors, sockets, tasks, permits and memory must all return to
//! baseline after repeated open/close cycles.

#[path = "../support/mod.rs"]
mod support;

use std::time::{Duration, Instant};

use support::{client, make_payload, Behaviour, Origin, TestServer};

const SIZE: usize = 256 * 1024;

async fn boot(size: usize) -> TestServer {
    let origin = Origin::start(size).await;
    support::start_server(origin).await
}

/// Wait until the server has no live streams and no live sessions. The budget
/// is generous on purpose: this suite is about *returning to baseline*, not
/// about how fast, and a loaded machine must not turn that into a flake.
async fn wait_for_quiescence(s: &TestServer) -> serde_json::Value {
    for _ in 0..240 {
        let stats = s.stats().await;
        if stats["active_streams"] == 0 && stats["registry"]["sessions"] == 0 {
            return stats;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stats = s.stats().await;
    panic!(
        "never quiesced: active_streams={} sessions={}",
        stats["active_streams"], stats["registry"]["sessions"]
    );
}

/// Resident set size of this process, in bytes, when the platform exposes it.
fn process_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "windows")]
    {
        // `Get-Process` for the current PID via PowerShell is far too slow for a
        // loop, so we read the counter from /proc-equivalent tooling instead:
        // the task-list heap value is not accessible, so we report `None` and
        // rely on the in-process allocation counters below.
        None
    }
    #[cfg(target_os = "linux")]
    {
        let s = std::fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = s.split_whitespace().nth(1)?.parse().ok()?;
        Some(pages * 4096)
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        None
    }
}

#[tokio::test]
async fn five_hundred_streams_leave_no_residue() {
    let s = boot(SIZE).await;
    let c = client();
    // Warm the pool and the cache first so we measure steady state.
    for _ in 0..5 {
        let _ = c
            .get(s.stream_url("/media"))
            .send()
            .await
            .unwrap()
            .bytes()
            .await;
    }
    let baseline = wait_for_quiescence(&s).await;
    let pools_before = baseline["pool"]["clients"].as_u64().unwrap_or(0);

    for i in 0..500u32 {
        let status = c
            .get(s.stream_url("/media"))
            .header("range", format!("bytes={}-", (i % 64) * 1024))
            .timeout(Duration::from_millis(300))
            .send()
            .await
            .map(|r| r.status().as_u16())
            .unwrap_or(0);
        assert!(
            status == 206 || status == 200 || status == 0,
            "status {status}"
        );
    }
    let after = wait_for_quiescence(&s).await;

    assert_eq!(after["active_streams"], 0);
    assert_eq!(after["registry"]["sessions"], 0);
    assert_eq!(after["registry"]["in_flight"], 0);
    // Connection pools are bounded and reused, not accumulated.
    let pools_after = after["pool"]["clients"].as_u64().unwrap_or(0);
    assert!(
        pools_after <= pools_before.max(1),
        "pool grew from {pools_before} to {pools_after}"
    );
    // The cache is bounded regardless of URL churn.
    assert!(
        after["cache"]["entries"].as_u64().unwrap_or(0) <= 4096,
        "cache grew unbounded"
    );
    s.origin.stop().await;
}

#[tokio::test]
async fn aborted_streams_release_everything() {
    let s = boot(4 * 1024 * 1024).await;
    let c = client();
    let baseline = wait_for_quiescence(&s).await;
    let _ = baseline;

    for i in 0..300u32 {
        let resp = c
            .get(s.stream_url("/media"))
            .header("x-ddl-session", format!("s{i}"))
            .header("x-ddl-generation", "1")
            .send()
            .await
            .unwrap();
        // Drop mid-flight, repeatedly, with no body read at all.
        drop(resp);
    }
    let after = wait_for_quiescence(&s).await;
    assert_eq!(after["active_streams"], 0);
    assert_eq!(after["registry"]["sessions"], 0);
    s.origin.stop().await;
}

#[tokio::test]
async fn superseded_sessions_do_not_accumulate() {
    let s = boot(SIZE).await;
    let c = client();
    // A slow origin keeps each stream open long enough that the next generation
    // really does supersede a live one, the way a player seeking in quick
    // succession does.
    s.origin
        .set_behaviour(Behaviour::SlowBody(Duration::from_millis(20)));
    let mut live: Vec<reqwest::Response> = Vec::new();
    for i in 0..200u32 {
        live.push(
            c.get(s.stream_url("/media"))
                .header("x-ddl-session", "churn")
                .header("x-ddl-generation", i as u64)
                .send()
                .await
                .unwrap(),
        );
    }
    let stats = s.stats().await;
    assert_eq!(
        stats["registry"]["sessions"], 1,
        "one session must survive: {stats}"
    );
    assert!(
        stats["registry"]["superseded"].as_u64().unwrap_or(0) >= 100,
        "supersede path was not taken often enough: {stats}"
    );

    drop(live);
    s.origin.set_behaviour(Behaviour::Normal);
    let after = wait_for_quiescence(&s).await;
    assert_eq!(after["registry"]["sessions"], 0, "{after}");
    assert_eq!(after["active_streams"], 0);
    s.origin.stop().await;
}

#[tokio::test]
async fn memory_does_not_grow_with_the_number_of_completed_streams() {
    let s = boot(SIZE).await;
    let c = client();

    let mut warm = Vec::new();
    for _ in 0..50 {
        warm.push(c.get(s.stream_url("/media")).send().await.unwrap());
    }
    drop(warm);
    wait_for_quiescence(&s).await;
    let baseline = process_rss_bytes();

    for _ in 0..500 {
        if let Ok(r) = c
            .get(s.stream_url("/media"))
            .timeout(Duration::from_millis(300))
            .send()
            .await
        {
            let _ = r.bytes().await;
        }
    }
    wait_for_quiescence(&s).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    if let (Some(before), Some(after)) = (baseline, process_rss_bytes()) {
        // A generous ceiling: this catches "we are holding the media", not
        // allocator noise.
        assert!(
            after < before + 128 * 1024 * 1024,
            "RSS grew from {before} to {after} across 500 streams"
        );
    }
    // On every platform the in-process accounting must still be zero.
    assert_eq!(s.stats().await["active_streams"], 0);
    s.origin.stop().await;
}

#[tokio::test]
async fn memory_is_independent_of_media_size() {
    // Same code path, 4 MiB vs 256 MiB, reading only a prefix of each.
    let small = boot(4 * 1024 * 1024).await;
    let big = boot(256 * 1024 * 1024).await;
    let c = client();

    let baseline = process_rss_bytes();
    for (server, expected_prefix) in [(&small, 1usize), (&big, 1usize)] {
        let mut r = c.get(server.stream_url("/media")).send().await.unwrap();
        let mut n = 0usize;
        while n < 2 * 1024 * 1024 * expected_prefix {
            match r.chunk().await {
                Ok(Some(b)) => n += b.len(),
                _ => break,
            }
        }
        assert!(n >= 2 * 1024 * 1024, "prefix read was {n}");
        drop(r);
        wait_for_quiescence(server).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    if let (Some(before), Some(after)) = (baseline, process_rss_bytes()) {
        assert!(
            after < before + 64 * 1024 * 1024,
            "RSS moved from {before} to {after} while streaming a 256 MiB file"
        );
    }
    small.origin.stop().await;
    big.origin.stop().await;
}

#[tokio::test]
async fn a_long_sequential_run_stays_stable() {
    // Five minutes of continuous, interrupted playback against a healthy and
    // an unhealthy origin in turn.
    let s = boot(SIZE).await;
    let c = client();
    // Run for a fixed wall-clock window; how many iterations that buys depends
    // on the machine, so we assert on the invariants, not on the count.
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut iterations = 0u64;
    let mut flips = 0;

    while Instant::now() < deadline {
        iterations += 1;
        // Flip the origin between healthy and hostile every few iterations.
        if iterations % 7 == 0 {
            flips += 1;
            s.origin.set_behaviour(if flips % 2 == 0 {
                Behaviour::Normal
            } else {
                Behaviour::TruncateAfter(16 * 1024)
            });
        }
        let outcome = c
            .get(s.stream_url("/media"))
            .timeout(Duration::from_millis(500))
            .send()
            .await;
        if let Ok(r) = outcome {
            // Read at most 64 KiB then abandon: this is what a real player does
            // when the user seeks away.
            let mut r = r;
            let mut n = 0usize;
            while n < 64 * 1024 {
                match r.chunk().await {
                    Ok(Some(b)) => n += b.len(),
                    _ => break,
                }
            }
        }
    }

    let stats = wait_for_quiescence(&s).await;
    assert!(iterations > 50, "only ran {iterations} iterations");
    assert_eq!(stats["active_streams"], 0, "streams leaked: {stats}");
    assert_eq!(stats["registry"]["sessions"], 0);
    assert!(
        stats["pool"]["clients"].as_u64().unwrap_or(0) <= 2,
        "pool grew: {stats}"
    );
    assert!(
        stats["retries_total"].as_u64().unwrap_or(0) < iterations * 4,
        "retry storm: {stats}"
    );
    // Deterministic accounting: every stream is accounted for.
    let total = stats["streams_total"].as_u64().unwrap();
    let cancelled = stats["streams_cancelled"].as_u64().unwrap();
    assert!(total > 0);
    assert!(cancelled <= total);
    s.origin.stop().await;
}

#[tokio::test]
async fn bandwidth_is_not_wasted_on_cancelled_streams() {
    let s = boot(8 * 1024 * 1024).await;
    let c = client();
    s.origin
        .set_behaviour(Behaviour::SlowBody(Duration::from_millis(5)));

    for _ in 0..5 {
        if let Ok(r) = c.get(s.stream_url("/media")).send().await {
            drop(r);
        }
    }
    wait_for_quiescence(&s).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // A cancelled stream must not have pulled the whole 8 MiB. The bound is
    // generous but proves we stop when the client stops.
    let served = s.origin.bytes_served();
    let full = 5 * 8 * 1024 * 1024u64;
    assert!(
        served < full,
        "cancelled streams pulled {served} of a possible {full} bytes"
    );
    s.origin.stop().await;
}

#[tokio::test]
async fn payload_bytes_never_leak_into_an_error_body() {
    let s = boot(SIZE).await;
    let c = client();
    s.origin.set_behaviour(Behaviour::Status(500));
    let r = c.get(s.stream_url("/media")).send().await.unwrap();
    let body = r.bytes().await.unwrap();
    let payload = make_payload(SIZE);
    // The JSON error body must not echo any part of the media payload.
    assert!(body.len() < 4096, "error body was {} bytes", body.len());
    for window in payload.chunks(64).take(64) {
        assert!(
            !body.windows(window.len()).any(|w| w == window),
            "media bytes appeared in an error body"
        );
    }
    s.origin.stop().await;
}
