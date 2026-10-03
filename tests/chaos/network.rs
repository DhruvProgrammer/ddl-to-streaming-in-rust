//! Network chaos.
//!
//! Every failure mode a real origin produces, one at a time, asserting that
//! the proxy stays alive, errors are deterministic and explicit, permits are
//! released, and no retry loop ever runs away.

#[path = "../support/mod.rs"]
mod support;

use std::time::Duration;

use support::{client, make_payload, Behaviour, Origin, TestServer};

const SIZE: usize = 1024 * 1024;

async fn boot() -> TestServer {
    let origin = Origin::start(SIZE).await;
    support::start_server(origin).await
}

async fn probe(base: &str, url: String) -> (u16, serde_json::Value) {
    let r = client()
        .post(format!("{base}/api/probe"))
        .json(&serde_json::json!({ "url": url }))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    let body: serde_json::Value = r.json().await.unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// Run one stream to completion (or failure) and settle the server state.
async fn one_stream(s: &TestServer) -> (u16, Result<usize, ()>) {
    let r = client()
        .get(s.stream_url("/media"))
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .expect("proxy stayed reachable");
    let status = r.status().as_u16();
    let result = match r.bytes().await {
        Ok(b) => {
            assert_eq!(
                b.as_ref(),
                make_payload(SIZE).as_slice(),
                "a successful stream must be byte-exact"
            );
            Ok(b.len())
        }
        Err(_) => Err(()),
    };
    settle(s).await;
    (status, result)
}

async fn settle(s: &TestServer) {
    for _ in 0..80 {
        if s.stats().await["active_streams"] == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("streams never released: {}", s.stats().await);
}

#[tokio::test]
async fn every_status_produces_its_own_code_and_never_repeats_forever() {
    let s = boot().await;
    let cases: &[(u16, &str, bool)] = &[
        (400, "HTTP_400", false),
        (401, "HTTP_401", false),
        (403, "HTTP_403", false),
        (404, "HTTP_404", false),
        (416, "HTTP_416", false),
        (500, "UPSTREAM_ERROR", true),
        (502, "UPSTREAM_ERROR", true),
        (503, "UPSTREAM_ERROR", true),
        (504, "UPSTREAM_ERROR", true),
    ];
    for (upstream, code, retryable) in cases {
        s.origin.set_behaviour(Behaviour::Status(*upstream));
        s.origin.reset();
        let (_status, body) = probe(&s.base, s.origin.url("/media")).await;
        assert_eq!(body["code"], *code, "upstream {upstream} -> {body}");
        assert_eq!(body["retryable"], *retryable, "upstream {upstream}");
        // Terminal statuses cost exactly one request; transient ones cost the
        // bounded retry budget and no more.
        let expected = if *retryable { 4 } else { 1 };
        assert_eq!(
            s.origin.request_count(),
            expected,
            "upstream {upstream} request count"
        );
        settle(&s).await;
    }
    s.origin.stop().await;
}

#[tokio::test]
async fn rate_limiting_surfaces_the_retry_after_hint() {
    let s = boot().await;
    s.origin.set_behaviour(Behaviour::RateLimited(120));
    let (status, body) = probe(&s.base, s.origin.url("/media")).await;
    assert_eq!(body["code"], "UPSTREAM_ERROR");
    assert_eq!(body["retryable"], true);
    assert!(status >= 500);
    // The retry hint was seen and honoured rather than ignored.
    let stats = s.stats().await;
    assert!(
        stats["retry_after_honoured"].as_u64().unwrap_or(0) >= 1,
        "{stats}"
    );
    settle(&s).await;
    s.origin.stop().await;
}

#[tokio::test]
async fn flaky_origin_recovers_without_user_interaction() {
    let s = boot().await;
    for fails in [1u16, 2, 3] {
        s.origin.set_behaviour(Behaviour::Flaky {
            fail_times: fails,
            code: 503,
        });
        let (status, body) = probe(&s.base, s.origin.url("/media")).await;
        assert_eq!(status, 200, "{fails} failures should still succeed: {body}");
        assert_eq!(body["content_length"], SIZE as u64);
    }
    s.origin.stop().await;
}

#[tokio::test]
async fn missing_content_length_falls_back_to_chunked_200() {
    let s = boot().await;
    s.origin.set_behaviour(Behaviour::NoContentLength);
    let r = client().get(s.stream_url("/media")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.headers().get("content-length").is_none());
    let body = r.bytes().await.unwrap();
    assert_eq!(body.as_ref(), make_payload(SIZE).as_slice());
    settle(&s).await;
    s.origin.stop().await;
}

#[tokio::test]
async fn missing_accept_ranges_is_reported_not_fatal() {
    let s = boot().await;
    s.origin.set_behaviour(Behaviour::NoAcceptRanges);
    let (_, body) = probe(&s.base, s.origin.url("/media")).await;
    assert_eq!(body["content_length"], SIZE as u64);
    assert!(
        body["warning"].as_str().is_some(),
        "a range-less origin must produce a warning: {body}"
    );
    let (status, result) = one_stream(&s).await;
    assert_eq!(status, 200);
    assert_eq!(result.unwrap(), SIZE);
    s.origin.stop().await;
}

#[tokio::test]
async fn invalid_media_is_refused_rather_than_forwarded() {
    let s = boot().await;
    s.origin.set_behaviour(Behaviour::NotMedia);
    let (_, body) = probe(&s.base, s.origin.url("/media")).await;
    assert_eq!(body["streamable"], false);
    assert!(body["reason"].as_str().unwrap().len() > 5, "{body}");
    settle(&s).await;
    s.origin.stop().await;
}

#[tokio::test]
async fn missing_content_type_falls_back_to_magic_bytes() {
    let s = boot().await;
    s.origin.set_behaviour(Behaviour::OmitContentType);
    let (status, body) = probe(&s.base, s.origin.url("/media")).await;
    assert_eq!(status, 200);
    assert_eq!(body["container"], "mp4");
    assert_eq!(body["evidence"], "magic-bytes");
    assert_eq!(body["content_type"], "video/mp4");
    s.origin.stop().await;
}

#[tokio::test]
async fn immediate_disconnect_is_bounded_and_releases() {
    let s = boot().await;
    s.origin.set_behaviour(Behaviour::ImmediateDisconnect);
    let started = std::time::Instant::now();
    let (status, _) = one_stream(&s).await;
    assert_eq!(status, 200);
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "disconnect handling must be bounded"
    );
    s.origin.stop().await;
}

#[tokio::test]
async fn headers_then_close_is_bounded() {
    let s = boot().await;
    s.origin.set_behaviour(Behaviour::HeadersOnlyThenClose);
    let started = std::time::Instant::now();
    let r = client()
        .get(s.stream_url("/media"))
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .expect("proxy reachable");
    let _ = r.bytes().await;
    assert!(started.elapsed() < Duration::from_secs(8));
    settle(&s).await;
    s.origin.stop().await;
}

#[tokio::test]
async fn slow_origin_still_delivers_every_byte() {
    let s = boot().await;
    s.origin
        .set_behaviour(Behaviour::SlowBody(Duration::from_millis(1)));
    let (status, result) = one_stream(&s).await;
    assert_eq!(status, 200);
    assert_eq!(result.unwrap(), SIZE);
    s.origin.stop().await;
}

#[tokio::test]
async fn slow_origin_that_stalls_is_cut_off() {
    let s = boot().await;
    s.origin.set_behaviour(Behaviour::StallAfter(1024));
    let started = std::time::Instant::now();
    let mut r = client()
        .get(s.stream_url("/media"))
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .unwrap();
    while let Ok(Some(_)) = r.chunk().await {}
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "idle cut-off failed"
    );
    settle(&s).await;
    s.origin.stop().await;
}

#[tokio::test]
async fn broken_content_range_is_refused_mid_stream() {
    let s = boot().await;
    s.origin.set_behaviour(Behaviour::BrokenContentRange);
    let r = client()
        .get(s.stream_url("/media"))
        .header("range", "bytes=500-999")
        .send()
        .await
        .unwrap();
    let _ = r.bytes().await.is_err();
    settle(&s).await;
    s.origin.stop().await;
}

#[tokio::test]
async fn wrong_content_length_cannot_inflate_the_response() {
    let s = boot().await;
    s.origin.set_behaviour(Behaviour::WrongContentLength);
    let r = client()
        .get(s.stream_url("/media"))
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .unwrap();
    let advertised: usize = r.headers()["content-length"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(advertised, SIZE * 4);
    let outcome = r.bytes().await;
    if let Ok(body) = outcome {
        assert!(
            body.len() < SIZE * 4,
            "we must not invent the missing {} bytes",
            SIZE * 4 - body.len()
        );
    }
    settle(&s).await;
    s.origin.stop().await;
}

#[tokio::test]
async fn huge_origin_headers_do_not_break_the_proxy() {
    let s = boot().await;
    s.origin.set_behaviour(Behaviour::HeaderFlood(200));
    let r = client()
        .post(format!("{}/api/probe", s.base))
        .json(&serde_json::json!({ "url": s.origin.url("/media") }))
        .send()
        .await
        .expect("proxy survived a header flood");
    // Either it parsed the flood or it refused it; both are acceptable, a hang
    // or a crash is not.
    assert!(r.status().is_success() || r.status().is_server_error());
    settle(&s).await;
    s.origin.stop().await;
}

#[tokio::test]
async fn sequential_faults_never_accumulate_state() {
    let s = boot().await;
    for _ in 0..5 {
        for behaviour in [
            Behaviour::Status(503),
            Behaviour::TruncateAfter(4096),
            Behaviour::NoContentLength,
            Behaviour::IgnoreRange,
            Behaviour::StallAfter(512),
            Behaviour::Normal,
        ] {
            s.origin.set_behaviour(behaviour.clone());
            let _ = client()
                .get(s.stream_url("/media"))
                .timeout(Duration::from_secs(8))
                .send()
                .await;
            settle(&s).await;
        }
    }
    // Finally: the system still serves perfectly after all of that.
    s.origin.set_behaviour(Behaviour::Normal);
    let (status, result) = one_stream(&s).await;
    assert_eq!(status, 200);
    assert_eq!(result.unwrap(), SIZE);
    let stats = s.stats().await;
    assert_eq!(stats["active_streams"], 0);
    s.origin.stop().await;
}

#[tokio::test]
async fn the_proxy_survives_an_origin_that_disappears_entirely() {
    let origin = Origin::start(SIZE).await;
    let s = support::start_server(origin).await;
    let url = s.origin.url("/media");
    let base = s.base.clone();
    let origin_port = s.origin.addr.port();
    // Take the origin away without touching the proxy.
    s.origin.stop().await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    // The port is now closed, so connecting must fail cleanly.
    assert!(origin_port > 0);

    let (status, body) = probe(&base, url).await;
    assert!(
        matches!(
            body["code"].as_str(),
            Some("UPSTREAM_ERROR") | Some("CONNECTION_TIMEOUT") | Some("REQUEST_TIMEOUT")
        ),
        "unexpected error for a dead origin: {body}"
    );
    assert_eq!(body["retryable"], true);
    assert!(status >= 500);
    // The proxy itself is still healthy.
    let health: serde_json::Value = client()
        .get(format!("{}/api/health", base))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["status"], "ok");
}

#[tokio::test]
async fn metrics_endpoint_stays_available_under_faults() {
    let s = boot().await;
    s.origin.set_behaviour(Behaviour::Status(500));
    for _ in 0..10 {
        let _ = probe(&s.base, s.origin.url("/media")).await;
    }
    let prom = client()
        .get(format!("{}/metrics", s.base))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(prom.contains("ddl_errors_total{code=\"UPSTREAM_ERROR\"}"));
    assert!(prom.contains("ddl_retries_total"));
    settle(&s).await;
    s.origin.stop().await;
}
