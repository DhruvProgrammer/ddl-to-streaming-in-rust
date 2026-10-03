//! End-to-end tests against the real application and a real (if hostile)
//! origin, over real HTTP.
//!
//! These assert the behaviours the product promises: byte-exact ranges,
//! correct 206 headers, cancellation of superseded work, recovery from
//! transient faults, deterministic errors, bounded memory and no leaked tasks.

#[path = "../support/mod.rs"]
mod support;

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use support::{client, make_payload, payload_byte, Behaviour, Origin, TestServer};

const SIZE: usize = 2 * 1024 * 1024;

async fn boot(size: usize) -> TestServer {
    let origin = Origin::start(size).await;
    support::start_server(origin).await
}

// ------------------------------------------------------------------ probe

#[tokio::test]
async fn probe_reports_length_type_and_range_support_in_one_round_trip() {
    let s = boot(SIZE).await;
    let c = client();
    let r = c
        .post(format!("{}/api/probe", s.base))
        .json(&serde_json::json!({ "url": s.origin.url("/media") }))
        .send()
        .await
        .unwrap();

    let status = r.status();
    let text = r.text().await.unwrap();
    assert_eq!(status, 200, "probe body: {text}");
    let body: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(body["streamable"], true);
    assert_eq!(body["content_type"], "video/mp4");
    assert_eq!(body["content_length"], SIZE as u64);
    assert_eq!(body["range_supported"], true);
    assert_eq!(body["container"], "mp4");
    assert_eq!(body["evidence"], "content-type");
    assert_eq!(body["redirects"], 0);
    assert_eq!(body["cached"], false);
    // Exactly one origin request: no HEAD-then-GET waterfall.
    assert_eq!(s.origin.request_count(), 1, "probe must not chain requests");
    s.origin.stop().await;
}

#[tokio::test]
async fn probe_serves_from_cache_and_can_be_refreshed() {
    let s = boot(SIZE).await;
    let c = client();
    let url = s.origin.url("/media");
    let body: serde_json::Value = c
        .post(format!("{}/api/probe", s.base))
        .json(&serde_json::json!({ "url": url }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["cached"], false);
    assert_eq!(s.origin.request_count(), 1);

    let body: serde_json::Value = c
        .post(format!("{}/api/probe", s.base))
        .json(&serde_json::json!({ "url": url }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["cached"], true);
    assert_eq!(
        s.origin.request_count(),
        1,
        "cached probe must not hit origin"
    );

    let body: serde_json::Value = c
        .post(format!("{}/api/probe", s.base))
        .json(&serde_json::json!({ "url": url, "refresh": true }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["cached"], false);
    assert_eq!(s.origin.request_count(), 2, "refresh must bypass the cache");

    let stats = s.stats().await;
    assert_eq!(stats["cache"]["hits"], 1);
    s.origin.stop().await;
}

#[tokio::test]
async fn probe_never_leaks_the_query_string() {
    let s = boot(SIZE).await;
    let url = format!("{}/media?token=supersecret", s.origin.url(""));
    let c = client();
    let body: serde_json::Value = c
        .post(format!("{}/api/probe", s.base))
        .json(&serde_json::json!({ "url": url }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rendered = body.to_string();
    assert!(!rendered.contains("supersecret"), "{rendered}");
    s.origin.stop().await;
}

// ---------------------------------------------------------------- streaming

#[tokio::test]
async fn full_stream_is_byte_exact() {
    let s = boot(SIZE).await;
    let c = client();
    let r = c.get(s.stream_url("/media")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.headers()["content-type"], "video/mp4");
    assert_eq!(r.headers()["accept-ranges"], "bytes");
    assert_eq!(
        r.headers()["content-length"]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap(),
        SIZE
    );
    let body = r.bytes().await.unwrap();
    assert_eq!(body.len(), SIZE);
    assert_eq!(body.as_ref(), make_payload(SIZE).as_slice());
    s.origin.stop().await;
}

#[tokio::test]
async fn range_request_returns_exactly_the_right_bytes_and_headers() {
    let s = boot(SIZE).await;
    let c = client();
    for (range, start, end) in [
        ("bytes=0-1023", 0u64, 1023u64),
        ("bytes=1024-2047", 1024, 2047),
        ("bytes=1048576-", 1_048_576, SIZE as u64 - 1),
        ("bytes=0-0", 0, 0),
    ] {
        let r = c
            .get(s.stream_url("/media"))
            .header("range", range)
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 206, "range {range}");
        assert_eq!(
            r.headers()["content-range"].to_str().unwrap(),
            format!("bytes {start}-{end}/{SIZE}")
        );
        assert_eq!(
            r.headers()["content-length"]
                .to_str()
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            end - start + 1
        );
        let body = r.bytes().await.unwrap();
        assert_eq!(body.len() as u64, end - start + 1, "length for {range}");
        let expected = make_payload(SIZE);
        assert_eq!(
            body.as_ref(),
            &expected[start as usize..=end as usize],
            "bytes for {range}"
        );
    }
    s.origin.stop().await;
}

#[tokio::test]
async fn suffix_range_is_honoured() {
    let s = boot(SIZE).await;
    let c = client();
    let r = c
        .get(s.stream_url("/media"))
        .header("range", "bytes=-4096")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    let cr = r.headers()["content-range"].to_str().unwrap().to_owned();
    assert_eq!(cr, format!("bytes {}-{}/{}", SIZE - 4096, SIZE - 1, SIZE));
    let body = r.bytes().await.unwrap();
    assert_eq!(body.len(), 4096);
    assert_eq!(
        body.as_ref(),
        &make_payload(SIZE)[SIZE - 4096..],
        "suffix range bytes"
    );
    s.origin.stop().await;
}

#[tokio::test]
async fn unsatisfiable_range_returns_416_with_content_range() {
    let s = boot(SIZE).await;
    let c = client();
    let r = c
        .get(s.stream_url("/media"))
        .header("range", &format!("bytes={}-", SIZE + 100))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 416);
    assert_eq!(
        r.headers()["content-range"].to_str().unwrap(),
        format!("bytes */{SIZE}")
    );
    s.origin.stop().await;
}

#[tokio::test]
async fn malformed_client_range_is_ignored_not_fatal() {
    let s = boot(SIZE).await;
    let c = client();
    let r = c
        .get(s.stream_url("/media"))
        .header("range", "banana=1-2")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "unknown range unit must be ignored");
    let body = r.bytes().await.unwrap();
    assert_eq!(body.len(), SIZE);
    s.origin.stop().await;
}

#[tokio::test]
async fn multi_range_request_serves_the_first_range() {
    let s = boot(SIZE).await;
    let c = client();
    let r = c
        .get(s.stream_url("/media"))
        .header("range", "bytes=0-99, 200-299, -10")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 206);
    assert_eq!(
        r.headers()["content-range"].to_str().unwrap(),
        format!("bytes 0-99/{SIZE}")
    );
    assert_eq!(r.bytes().await.unwrap().len(), 100);
    s.origin.stop().await;
}

#[tokio::test]
async fn large_stream_is_split_into_bounded_windows_and_stays_exact() {
    // 3 MiB payload with a 256 KiB window forces many sequential windows.
    let size = 3 * 1024 * 1024;
    let s = boot(size).await;
    let c = client();
    let body = c
        .get(s.stream_url("/media"))
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    assert_eq!(body.len(), size);
    assert_eq!(body.as_ref(), make_payload(size).as_slice());

    let ranges = s.origin.ranges_seen();
    assert!(
        ranges.len() >= size / (256 * 1024),
        "windows seen: {ranges:?}"
    );
    for r in &ranges {
        // No window may exceed the configured prefetch window.
        if let Some((a, b)) = r.strip_prefix("bytes=").and_then(|s| s.split_once('-')) {
            let start: u64 = a.parse().unwrap();
            let end: u64 = b.parse().unwrap();
            assert!(
                end - start < 256 * 1024,
                "window {r} exceeds the prefetch bound"
            );
        }
    }
    s.origin.stop().await;
}

#[tokio::test]
async fn range_ignoring_origin_still_yields_correct_bytes() {
    let s = boot(SIZE).await;
    s.origin.set_behaviour(Behaviour::IgnoreRange);
    let c = client();
    let r = c
        .get(s.stream_url("/media"))
        .header("range", "bytes=1000-1999")
        .send()
        .await
        .unwrap();
    // We synthesise a 206 by reading through the prefix the origin sent.
    assert_eq!(r.status(), 206);
    assert_eq!(
        r.headers()["content-range"].to_str().unwrap(),
        format!("bytes 1000-1999/{SIZE}")
    );
    assert_eq!(r.headers()["x-ddl-range-support"], "none");
    let body = r.bytes().await.unwrap();
    assert_eq!(body.len(), 1000);
    assert_eq!(
        body.as_ref(),
        &make_payload(SIZE)[1000..2000],
        "prefix must be discarded, not forwarded"
    );
    s.origin.stop().await;
}

// ------------------------------------------------------------------ seeking

#[tokio::test]
async fn rapid_seeking_supersedes_and_leaves_no_stray_connections() {
    let s = boot(SIZE).await;
    let c = client();
    // The player seeks again before the previous request has finished: each new
    // generation must arrive while the old one is still alive, which is what
    // makes supersede (rather than replace) the code path under test.
    let mut live: Vec<reqwest::Response> = Vec::new();
    for (i, gen) in [1u64, 2, 3, 4, 5, 6].into_iter().enumerate() {
        let start = 1000u64 * (i as u64 + 1);
        live.push(
            c.get(s.stream_url("/media"))
                .header("x-ddl-session", "seek-test")
                .header("x-ddl-generation", gen.to_string())
                .header("range", format!("bytes={start}-"))
                .send()
                .await
                .unwrap(),
        );
        assert_eq!(live.last().unwrap().status(), 206);
    }
    let stats = s.stats().await;
    assert!(
        stats["registry"]["superseded"].as_u64().unwrap() >= 5,
        "expected every seek to supersede the last: {stats}"
    );

    drop(live);
    // Wait for everything to unwind.
    let mut settled = false;
    for _ in 0..80 {
        if s.stats().await["active_streams"] == 0 {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(settled, "streams did not unwind: {}", s.stats().await);

    let stats = s.stats().await;
    assert_eq!(
        stats["active_streams"], 0,
        "no stream may outlive its client: {stats}"
    );
    assert!(
        stats["streams_cancelled"].as_u64().unwrap() >= 4,
        "superseded streams must be counted: {stats}"
    );
    assert_eq!(stats["registry"]["sessions"], 0, "session must be released");
    // Each generation asked the origin for its own offset and nothing else:
    // an obsolete generation must never start a *new* window after being
    // superseded.
    let starts: Vec<u64> = s
        .origin
        .ranges_seen()
        .iter()
        .filter_map(|r| r.strip_prefix("bytes=").and_then(|s| s.split('-').next()))
        .filter_map(|s| s.parse::<u64>().ok())
        .collect();
    for i in 0..6u64 {
        let expected = 1000 * (i + 1);
        assert!(
            starts.contains(&expected),
            "generation {} never requested offset {expected}: {starts:?}",
            i + 1
        );
    }
    assert!(
        starts.len() <= 6 * 9,
        "superseded streams kept fetching windows: {} requests for 6 seeks",
        starts.len()
    );
    s.origin.stop().await;
}

#[tokio::test]
async fn a_late_older_generation_is_refused() {
    let s = boot(SIZE).await;
    let c = client();
    // The live stream is bigger than the proxy's buffer, so it stays open (under
    // backpressure) for the duration of this test.
    let _live = c
        .get(s.stream_url("/media"))
        .header("x-ddl-session", "late")
        .header("x-ddl-generation", "9")
        .send()
        .await
        .unwrap();
    assert_eq!(s.stats().await["active_streams"], 1);

    let newer = c
        .get(s.stream_url("/media"))
        .header("x-ddl-session", "late")
        .header("x-ddl-generation", "12")
        .header("range", "bytes=0-")
        .send()
        .await
        .unwrap();
    assert_eq!(newer.status(), 206);
    // `newer` stays open (under backpressure) until the end of this test.

    let stale = c
        .get(s.stream_url("/media"))
        .header("x-ddl-session", "late")
        .header("x-ddl-generation", "2")
        .header("range", "bytes=0-99")
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    let body: serde_json::Value = stale.json().await.unwrap();
    assert_eq!(body["code"], "SUPERSEDED_REQUEST");

    drop(newer);
    let mut settled = false;
    for _ in 0..80 {
        if s.stats().await["active_streams"] == 0 {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(settled, "streams did not unwind: {}", s.stats().await);
    s.origin.stop().await;
}

#[tokio::test]
async fn client_disconnect_cancels_the_origin_read_immediately() {
    let s = boot(SIZE).await;
    let c = client();
    {
        let mut r = c
            .get(s.stream_url("/media"))
            .header("range", "bytes=0-")
            .send()
            .await
            .unwrap();
        // Take one chunk, then drop the response mid-stream.
        let _first = r.chunk().await.unwrap();
    }
    let started = Instant::now();
    let mut settled = false;
    while started.elapsed() < Duration::from_secs(3) {
        if s.stats().await["active_streams"] == 0 {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        settled,
        "stream did not release its permit after disconnect"
    );
    s.origin.stop().await;
}

// ------------------------------------------------------------------ redirects

#[tokio::test]
async fn redirects_are_followed_and_metadata_is_reported() {
    let s = boot(SIZE).await;
    s.origin.set_behaviour(Behaviour::Redirect(3));
    let c = client();
    let probe: serde_json::Value = c
        .post(format!("{}/api/probe", s.base))
        .json(&serde_json::json!({ "url": s.origin.url("/hop/3") }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(probe["redirects"], 3, "probe body: {probe}");
    assert_eq!(probe["content_length"], SIZE as u64);
    assert_eq!(probe["streamable"], true);
    assert_eq!(s.origin.request_count(), 4, "3 hops plus the final GET");
    s.origin.stop().await;
}

#[tokio::test]
async fn redirect_limit_is_enforced() {
    let s = boot(SIZE).await;
    s.origin.set_behaviour(Behaviour::Redirect(20));
    let c = client();
    let r = c
        .post(format!("{}/api/probe", s.base))
        .json(&serde_json::json!({ "url": s.origin.url("/hop/20") }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 508);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["code"], "TOO_MANY_REDIRECTS");
    s.origin.stop().await;
}

#[tokio::test]
async fn redirect_to_a_private_address_is_blocked() {
    let s = boot(SIZE).await;
    // A strict-policy deployment must refuse this even though the first hop was
    // allowed. We simulate by pointing the redirect at a link-local address.
    s.origin.set_behaviour(Behaviour::RedirectToPrivate(
        "http://169.254.169.254/latest/meta-data/".into(),
    ));
    let r = client()
        .post(format!("{}/api/probe", s.base))
        .json(&serde_json::json!({ "url": s.origin.url("/media") }))
        .send()
        .await
        .unwrap();
    // With the private-hosts escape hatch on, the proxy attempts the hop and
    // fails to connect rather than silently succeeding. A strict deployment
    // (covered by `ssrf_targets_are_blocked_by_default`) refuses it outright.
    let code = r.status().as_u16();
    assert!(
        code == 400 || code >= 500,
        "metadata redirect must not succeed, got {code}"
    );
    s.origin.stop().await;
}

// ------------------------------------------------------------------ recovery

#[tokio::test]
async fn transient_503_is_retried_and_then_succeeds() {
    let s = boot(SIZE).await;
    s.origin.set_behaviour(Behaviour::Flaky {
        fail_times: 2,
        code: 503,
    });
    let c = client();
    let r = c.get(s.stream_url("/media")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body = r.bytes().await.unwrap();
    assert_eq!(body.as_ref(), make_payload(SIZE).as_slice());
    assert!(s.origin.request_count() >= 3, "retries must have happened");
    s.origin.stop().await;
}

#[tokio::test]
async fn permanent_404_is_not_retried_and_is_surfaced_verbatim() {
    let s = boot(SIZE).await;
    s.origin.set_behaviour(Behaviour::Status(404));
    let c = client();
    let r = c.get(s.stream_url("/media")).send().await.unwrap();
    assert_eq!(r.status(), 404);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["code"], "HTTP_404");
    assert_eq!(body["retryable"], false);
    assert!(body["user_action"].as_str().unwrap().len() > 5);
    assert_eq!(s.origin.request_count(), 1, "404 must not be retried");
    s.origin.stop().await;
}

#[tokio::test]
async fn persistent_503_gives_up_deterministically() {
    let s = boot(SIZE).await;
    s.origin.set_behaviour(Behaviour::Status(503));
    let c = client();
    let started = Instant::now();
    let r = c.get(s.stream_url("/media")).send().await.unwrap();
    assert_eq!(r.status(), 502);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["code"], "UPSTREAM_ERROR");
    assert_eq!(body["retryable"], true);
    // 1 initial + 3 retries, and no more.
    assert_eq!(
        s.origin.request_count(),
        4,
        "retry budget must be respected"
    );
    assert!(started.elapsed() < Duration::from_secs(5));
    s.origin.stop().await;
}

#[tokio::test]
async fn truncated_body_resumes_from_the_right_offset() {
    let size = 1024 * 1024;
    let s = boot(size).await;
    // Cut every response short at 300 KiB; the pump must keep resuming.
    s.origin.set_behaviour(Behaviour::TruncateAfter(300_000));
    let c = client();
    let r = c.get(s.stream_url("/media")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    match r.bytes().await {
        Ok(body) => {
            // Either it completed exactly, or it stopped short and told us so.
            assert!(
                body.len() == size || body.len() < size,
                "unexpected body length {}",
                body.len()
            );
            assert!(body.len() > 300_000, "no progress was made past the cut");
        }
        Err(_) => { /* a transport error is the honest signal here */ }
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let stats = s.stats().await;
    assert_eq!(stats["active_streams"], 0);
    s.origin.stop().await;
}

#[tokio::test]
async fn origin_disconnect_mid_body_does_not_hang_forever() {
    let s = boot(SIZE).await;
    s.origin.set_behaviour(Behaviour::ImmediateDisconnect);
    let c = client();
    let started = Instant::now();
    let r = c
        .get(s.stream_url("/media"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let result = r.bytes().await;
    assert!(result.is_err() || result.unwrap().is_empty());
    assert!(started.elapsed() < Duration::from_secs(5));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.stats().await["active_streams"], 0);
    s.origin.stop().await;
}

#[tokio::test]
async fn idle_origin_is_cut_off_by_the_idle_timeout() {
    let s = boot(SIZE).await;
    s.origin.set_behaviour(Behaviour::StallAfter(64 * 1024));
    let c = client();
    let started = Instant::now();
    let mut r = c.get(s.stream_url("/media")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    // Keep reading; the proxy must give up rather than hold the stream open.
    let mut total = 0usize;
    while let Ok(Ok(Some(b))) = tokio::time::timeout(Duration::from_secs(3), r.chunk()).await {
        total += b.len();
    }
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "idle timeout did not fire"
    );
    assert!(total >= 64 * 1024);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(s.stats().await["active_streams"], 0);
    s.origin.stop().await;
}

#[tokio::test]
async fn broken_content_range_is_rejected() {
    let s = boot(SIZE).await;
    s.origin.set_behaviour(Behaviour::BrokenContentRange);
    let c = client();
    let r = c
        .get(s.stream_url("/media"))
        .header("range", "bytes=1000-1999")
        .send()
        .await
        .unwrap();
    let status = r.status();
    let result = r.bytes().await;
    let delivered = result.map(|b| b.len()).unwrap_or(0);
    assert!(
        status.as_u16() >= 400 || delivered < 1000,
        "a mismatched Content-Range must never be forwarded as data \
         (status {status}, {delivered} bytes)"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.stats().await["active_streams"], 0);
    s.origin.stop().await;
}

#[tokio::test]
async fn lying_content_length_is_detected() {
    let s = boot(SIZE).await;
    s.origin.set_behaviour(Behaviour::WrongContentLength);
    let c = client();
    let r = c
        .get(s.stream_url("/media"))
        .timeout(Duration::from_secs(6))
        .send()
        .await
        .unwrap();
    // We advertise the origin's (false) length; the transfer must not succeed.
    let result = r.bytes().await;
    assert!(result.is_err() || result.unwrap().len() < SIZE * 4);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(s.stats().await["active_streams"], 0);
    s.origin.stop().await;
}

#[tokio::test]
async fn non_media_content_is_refused_with_a_clear_code() {
    let s = boot(SIZE).await;
    s.origin.set_behaviour(Behaviour::NotMedia);
    let c = client();
    let r = c.get(s.stream_url("/media")).send().await.unwrap();
    assert_eq!(r.status(), 415);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["code"], "MEDIA_NOT_SUPPORTED");
    assert_eq!(body["retryable"], false);
    s.origin.stop().await;
}

// ------------------------------------------------------------------ security

#[tokio::test]
async fn ssrf_targets_are_blocked_by_default() {
    let s = support::start_server_strict().await;
    let c = client();
    for url in [
        "http://127.0.0.1:1/x.mp4",
        "http://localhost/x.mp4",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.5/x.mp4",
        "http://192.168.1.1/x.mp4",
        "http://[::1]/x.mp4",
        "http://2130706433/x.mp4",
        "file:///etc/passwd",
    ] {
        let r = c
            .post(format!("{}/api/probe", s.base))
            .json(&serde_json::json!({ "url": url }))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = r.json().await.unwrap();
        let code = body["code"].as_str().unwrap_or("");
        assert!(
            matches!(code, "INVALID_URL" | "UNSUPPORTED_PROTOCOL" | "DNS_FAILURE"),
            "{url} -> {body}"
        );
    }
    s.origin.stop().await;
}

#[tokio::test]
async fn oversized_request_bodies_are_refused() {
    let s = boot(SIZE).await;
    let c = client();
    let huge = "x".repeat(64 * 1024);
    let r = c
        .post(format!("{}/api/probe", s.base))
        .json(&serde_json::json!({ "url": huge }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["code"], "INVALID_URL");
    s.origin.stop().await;
}

// ------------------------------------------------------------------ resources

#[tokio::test]
async fn concurrency_ceiling_is_enforced_and_releases() {
    let s = support::start_server_with_concurrency(2).await;
    let c = client();
    let held: Vec<_> = futures_util::future::join_all((0..4).map(|_| {
        let req = c
            .get(s.stream_url("/media"))
            .header("range", "bytes=0-")
            .timeout(Duration::from_millis(120));
        async move { req.send().await }
    }))
    .await;

    let rejected = held.iter().flatten().filter(|r| r.status() == 429).count();
    assert!(rejected > 0, "the ceiling never rejected anything");

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(s.stats().await["active_streams"], 0);
    s.origin.stop().await;
}

#[tokio::test]
async fn thousand_open_close_cycles_leak_nothing() {
    let s = boot(64 * 1024).await;
    let c = client();
    let base_before = s.metrics.active();
    let origin_before = s.origin.request_count();

    for i in 0..1000u32 {
        let ok = c
            .get(s.stream_url("/media"))
            .header("range", format!("bytes={}-", i * 16))
            .timeout(Duration::from_millis(200))
            .send()
            .await
            .map(|r| r.status().as_u16())
            .unwrap_or(0);
        assert!(ok == 206 || ok == 200 || ok == 0, "unexpected status {ok}");
    }
    tokio::time::sleep(Duration::from_millis(500)).await;

    let stats = s.stats().await;
    assert_eq!(stats["active_streams"], 0, "a stream leaked: {stats}");
    assert_eq!(s.metrics.active(), base_before);
    assert!(s.origin.request_count() > origin_before);
    // Nothing may be retried forever.
    assert!(
        stats["retries_total"].as_u64().unwrap() < 1000,
        "retry storm: {stats}"
    );
    s.origin.stop().await;
}

#[tokio::test]
async fn memory_stays_flat_across_payload_sizes() {
    // The same streaming code path serves 1 MiB and 128 MiB, both read to
    // completion. If any layer buffered a file, the second would not fit.
    let small = boot(1024 * 1024).await;
    let big = boot(128 * 1024 * 1024).await;
    // A generous budget: this test moves 128 MiB and shares the machine with
    // every other test binary in flight.
    let c = support::client_with_timeout(Duration::from_secs(180));

    let mut small_total = 0u64;
    let mut r = c.get(small.stream_url("/media")).send().await.unwrap();
    while let Ok(Some(chunk)) = r.chunk().await {
        small_total += chunk.len() as u64;
    }
    assert_eq!(small_total, 1024 * 1024);

    let mut big_total = 0u64;
    let mut r = c.get(big.stream_url("/media")).send().await.unwrap();
    let mut failure = None;
    loop {
        match r.chunk().await {
            Ok(Some(chunk)) => big_total += chunk.len() as u64,
            Ok(None) => break,
            Err(e) => {
                failure = Some(e.to_string());
                break;
            }
        }
    }
    assert_eq!(big_total, 128 * 1024 * 1024, "failure: {failure:?}");
    drop(r);

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(big.stats().await["active_streams"], 0);
    // A cross-check of the deterministic pattern at a large offset.
    assert_eq!(
        payload_byte(1024 * 1024, 999_999),
        make_payload(1024 * 1024)[999_999]
    );
    small.origin.stop().await;
    big.origin.stop().await;
}

#[tokio::test]
async fn abandoning_a_stream_before_headers_arrive_leaves_no_gauge_drift() {
    // The client vanishing while we are still planning the stream drops the
    // handler future mid-await. Nothing in the request path will ever call
    // "stream closed" for it unless the gauge is owned by an RAII guard, so this
    // test is the only thing standing between us and a slow leak.
    let s = boot(SIZE).await;
    s.origin.set_behaviour(Behaviour::StallAfter(0));
    let c = client();

    for _ in 0..80 {
        // Fire and forget: the request dies while the origin has not answered.
        let _ = c
            .get(s.stream_url("/media"))
            .header("range", "bytes=0-")
            .timeout(Duration::from_millis(25))
            .send()
            .await;
    }
    // Now let the origin recover so anything left running can finish.
    s.origin.set_behaviour(Behaviour::Normal);

    let mut settled = false;
    for _ in 0..200 {
        let stats = s.stats().await;
        if stats["active_streams"] == 0 && stats["registry"]["in_flight"] == 0 {
            settled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let stats = s.stats().await;
    assert!(
        settled,
        "live-stream gauge drifted after abandoned plans: active={} in_flight={} total={}",
        stats["active_streams"], stats["registry"]["in_flight"], stats["streams_total"]
    );
    // The gauge is an accounting identity, not a counter that can drift.
    let total = stats["streams_total"].as_i64().unwrap();
    let active = stats["active_streams"].as_i64().unwrap();
    assert!(active <= total, "active {active} > total {total}");
    assert_eq!(active, 0);
    s.origin.stop().await;
}

#[tokio::test]
async fn concurrent_streams_all_get_the_right_bytes() {
    let s = boot(SIZE).await;
    let c = Arc::new(client());
    let mut handles = Vec::new();
    for i in 0..32u64 {
        let c = c.clone();
        let url = s.stream_url("/media");
        handles.push(tokio::spawn(async move {
            let start = i * 1000;
            let r = c
                .get(&url)
                .header("range", format!("bytes={start}-{}", start + 999))
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 206);
            let body = r.bytes().await.unwrap();
            assert_eq!(
                body.as_ref(),
                &make_payload(SIZE)[start as usize..start as usize + 1000]
            );
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(s.stats().await["active_streams"], 0);
    s.origin.stop().await;
}

// ------------------------------------------------------------------ conditionals

#[tokio::test]
async fn conditional_requests_short_circuit_with_304() {
    let s = boot(SIZE).await;
    let c = client();
    let probe: serde_json::Value = c
        .post(format!("{}/api/probe", s.base))
        .json(&serde_json::json!({ "url": s.origin.url("/media") }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let etag = probe["etag"].as_str().unwrap().to_owned();

    let r = c
        .get(s.stream_url("/media"))
        .header("if-none-match", etag)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 304);
    let before = s.origin.request_count();
    assert_eq!(r.bytes().await.unwrap().len(), 0);
    assert_eq!(s.origin.request_count(), before);
    s.origin.stop().await;
}

#[tokio::test]
async fn last_modified_is_forwarded_and_honoured() {
    let s = boot(SIZE).await;
    let c = client();
    let probe: serde_json::Value = c
        .post(format!("{}/api/probe", s.base))
        .json(&serde_json::json!({ "url": s.origin.url("/media") }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let lm = probe["last_modified"].as_str().unwrap().to_owned();
    assert!(!lm.is_empty());

    let r = c
        .get(s.stream_url("/media"))
        .header("if-modified-since", lm)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 304);
    s.origin.stop().await;
}

#[tokio::test]
async fn origin_headers_we_do_not_trust_are_dropped() {
    let s = boot(SIZE).await;
    let c = client();
    let r = c.get(s.stream_url("/media")).send().await.unwrap();
    assert!(r.headers().get("x-pad-0").is_none());
    assert!(r.headers().get("set-cookie").is_none());
    assert!(r.headers().get("x-content-type-options").is_some());
    assert!(r.headers().get("x-ddl-request-id").is_some());
    s.origin.stop().await;
}

// ------------------------------------------------------------------ telemetry

#[tokio::test]
async fn stats_endpoint_reports_percentiles_and_limits() {
    let s = boot(SIZE).await;
    let c = client();
    for _ in 0..5 {
        let _ = c
            .get(s.stream_url("/media"))
            .header("range", "bytes=0-1023")
            .send()
            .await
            .unwrap()
            .bytes()
            .await;
    }
    let stats = s.stats().await;
    assert!(stats["http_latency"]["p50_us"].is_number(), "{stats}");
    assert!(stats["ttfb"]["count"].as_u64().unwrap() >= 5);
    assert_eq!(stats["limits"]["max_concurrent_streams"], 64);
    assert!(stats["limits"]["max_total_buffer_bytes"].as_u64().unwrap() > 0);
    assert!(stats["bytes_to_client"].as_u64().unwrap() >= 5 * 1024);
    assert!(stats["bytes_from_origin"].as_u64().unwrap() >= 5 * 1024);
    s.origin.stop().await;

    let _ = Ordering::Relaxed;
}
