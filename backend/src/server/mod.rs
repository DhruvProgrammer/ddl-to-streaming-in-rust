//! The HTTP surface. Six routes, no framework sugar, no middleware stack.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderName, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::errors::{ErrorCode, PlayerError};
use crate::metrics::Metrics;
use crate::security::validate_with;
use crate::streaming::engine::StreamRequest;
use crate::streaming::{Engine, RequestId};

const HDR_SESSION: HeaderName = HeaderName::from_static("x-ddl-session");
const HDR_GENERATION: HeaderName = HeaderName::from_static("x-ddl-generation");
const HDR_REQUEST_ID: HeaderName = HeaderName::from_static("x-ddl-request-id");

/// Everything a handler needs. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    pub engine: Engine,
    pub metrics: Arc<Metrics>,
    pub shutdown: CancellationToken,
    pub started: Instant,
    pub static_dir: Option<Arc<String>>,
}

impl AppState {
    pub fn concurrency(&self) -> usize {
        self.engine.registry().stats().in_flight
    }
}

/// Truncate a client-supplied opaque value to a safe length.
fn bounded(v: Option<String>, max: usize) -> Option<String> {
    v.filter(|s| !s.is_empty())
        .map(|s| s.chars().take(max).collect())
}

/// Address policy in force for this deployment.
fn ddl_policy(state: &AppState) -> crate::security::Policy {
    crate::security::Policy::from_allow_private(state.engine.config().allow_private_hosts)
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/api/probe", post(probe))
        .route("/api/stream", get(stream))
        .route("/api/stats", get(stats))
        .route("/api/health", get(health))
        .route("/metrics", get(metrics_prometheus))
        .route("/api/client-events", post(client_events))
        .fallback(static_or_404)
        .with_state(state)
}

// ---------------------------------------------------------------- handlers

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProbeBody {
    url: String,
    /// Bypass the metadata cache.
    #[serde(default)]
    refresh: bool,
}

async fn probe(State(state): State<AppState>, req: Request<Body>) -> Response {
    let started = Instant::now();
    let body = match read_bounded_body(req, state.engine.config().max_request_body_bytes).await {
        Ok(b) => b,
        Err(e) => return error_response(e),
    };
    let parsed: ProbeBody = match serde_json::from_slice(&body) {
        Ok(p) => p,
        Err(e) => {
            return error_response(
                PlayerError::new(ErrorCode::InvalidUrl)
                    .with_reason(format!("malformed request body: {e}")),
            )
        }
    };
    if parsed.url.len() > state.engine.config().max_url_len {
        return error_response(
            PlayerError::new(ErrorCode::InvalidUrl).with_reason("url exceeds the length limit"),
        );
    }
    let policy = ddl_policy(&state);
    let safe = match validate_with(&parsed.url, policy) {
        Ok(s) => s,
        Err(e) => {
            state.metrics.record_error(e.code);
            return error_response(e);
        }
    };

    match state.engine.probe(&safe, parsed.refresh).await {
        Ok(result) => {
            state
                .metrics
                .http_latency_us
                .record_duration(started.elapsed());
            json(StatusCode::OK, &result)
        }
        Err(e) => {
            state.metrics.record_error(e.code);
            error_response(e)
        }
    }
}

#[derive(Debug, Deserialize)]
struct StreamQuery {
    url: String,
    /// Playback session id. Travels in the query string because a native
    /// `<video src>` cannot carry custom headers.
    #[serde(default)]
    s: Option<String>,
    /// Monotonic request generation within the session.
    #[serde(default)]
    g: Option<u64>,
}

async fn stream(
    State(state): State<AppState>,
    Query(q): Query<StreamQuery>,
    req: Request<Body>,
) -> Response {
    let rid = RequestId::next();
    let started = Instant::now();

    if state.shutdown.is_cancelled() {
        return error_response(PlayerError::new(ErrorCode::Shutdown));
    }

    let safe = match validate_with(&q.url, ddl_policy(&state)) {
        Ok(s) => s,
        Err(e) => {
            state.metrics.record_error(e.code);
            return tagged_error_response(e, rid);
        }
    };

    let headers = req.headers();
    // Headers win over query parameters: an explicit header is a deliberate API
    // client, and a query parameter is the browser path.
    let session = bounded(
        headers
            .get(HDR_SESSION)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
            .or(q.s),
        64,
    );
    let generation = headers
        .get(HDR_GENERATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .or(q.g);
    if headers.contains_key(HDR_GENERATION) && generation.is_none() {
        return tagged_error_response(
            PlayerError::new(ErrorCode::InvalidUrl).with_reason("malformed generation header"),
            rid,
        );
    }

    let request = StreamRequest {
        url: safe,
        raw_range: headers
            .get(http::header::RANGE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.chars().take(128).collect()),
        session,
        generation,
        if_none_match: headers
            .get(http::header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.chars().take(256).collect()),
        if_modified_since: headers
            .get(http::header::IF_MODIFIED_SINCE)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.chars().take(128).collect()),
    };

    let sresp = state.engine.clone().open_stream(request, rid).await;
    state
        .metrics
        .http_latency_us
        .record_duration(started.elapsed());
    match sresp {
        Ok(r) => build_stream_response(r),
        Err(e) => {
            state.metrics.record_error(e.code);
            tagged_error_response(e, rid)
        }
    }
}

/// Copy exactly the headers we generated, then hand over the body.
/// Nothing from the origin reaches the client unfiltered.
fn build_stream_response(r: crate::streaming::StreamResponse) -> Response {
    const FORWARD: [HeaderName; 10] = [
        axum::http::header::CONTENT_TYPE,
        axum::http::header::CONTENT_LENGTH,
        axum::http::header::CONTENT_RANGE,
        axum::http::header::ACCEPT_RANGES,
        axum::http::header::ETAG,
        axum::http::header::LAST_MODIFIED,
        axum::http::header::CACHE_CONTROL,
        axum::http::header::X_CONTENT_TYPE_OPTIONS,
        HeaderName::from_static("x-ddl-request-id"),
        HeaderName::from_static("x-ddl-range-support"),
    ];
    let mut builder = Response::builder().status(r.status);
    for name in FORWARD {
        if let Some(v) = r.headers.get(&name) {
            builder = builder.header(name, v);
        }
    }
    if let Some(v) = r.headers.get(&HDR_GENERATION) {
        builder = builder.header(HDR_GENERATION, v);
    }
    builder
        .body(Body::from_stream(r.body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

/// Minimal, cheap observability from the player. Counting only.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientEvent {
    /// `play` | `seek` | `stall` | `error`
    name: String,
    /// Milliseconds, when meaningful (e.g. time to first frame).
    #[serde(default)]
    ms: Option<f64>,
}

async fn client_events(State(state): State<AppState>, req: Request<Body>) -> Response {
    let body = match read_bounded_body(req, 1024).await {
        Ok(b) => b,
        Err(e) => return error_response(e),
    };
    let Ok(ev) = serde_json::from_slice::<ClientEvent>(&body) else {
        return json(StatusCode::BAD_REQUEST, &serde_json::json!({ "ok": false }));
    };
    let m = &state.metrics;
    match ev.name.as_str() {
        "play" => Metrics::incr(&m.client_play),
        "seek" => Metrics::incr(&m.client_seek),
        "stall" => Metrics::incr(&m.client_stall),
        "error" => Metrics::incr(&m.client_error),
        _ => {}
    }
    if let Some(ms) = ev.ms.filter(|v| v.is_finite() && *v >= 0.0) {
        let d = std::time::Duration::from_secs_f64((ms / 1000.0).min(3600.0));
        match ev.name.as_str() {
            "play" => m.startup_us.record_duration(d),
            "seek" => m.seek_us.record_duration(d),
            _ => {}
        }
    }
    StatusCode::NO_CONTENT.into_response()
}

async fn stats(State(state): State<AppState>) -> Response {
    let engine = state.engine.clone();
    let mut v = serde_json::to_value(state.metrics.snapshot()).unwrap_or(serde_json::Value::Null);
    let pool = engine.pool().stats();
    let cache = state
        .engine
        .cache()
        .stats()
        .map(|s| s.to_json())
        .unwrap_or(serde_json::Value::Null);
    let registry = engine.registry().stats();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("pool".into(), serde_json::to_value(pool).unwrap());
        obj.insert("registry".into(), serde_json::to_value(registry).unwrap());
        obj.insert(
            "cache".into(),
            if cache.is_null() {
                serde_json::json!({ "entries": 0, "bytes": 0, "hit_rate": null })
            } else {
                cache
            },
        );
        obj.insert(
            "uptime_s".into(),
            serde_json::json!(state.started.elapsed().as_secs_f64()),
        );
        obj.insert(
            "limits".into(),
            serde_json::json!({
                "max_concurrent_streams": engine.config().max_concurrent_streams,
                "stream_buffer_bytes": engine.config().stream_buffer_bytes,
                "max_total_buffer_bytes": engine.config().max_total_buffer_bytes(),
                "prefetch_window_bytes": engine.config().prefetch_window_bytes,
                "max_redirects": engine.config().max_redirects,
                "max_retries": engine.config().retry.max_attempts,
            }),
        );
    }
    json(StatusCode::OK, &v)
}

async fn health(State(state): State<AppState>) -> Response {
    let healthy = !state.shutdown.is_cancelled();
    json(
        if healthy {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        &serde_json::json!({
            "status": if healthy { "ok" } else { "draining" },
            "uptime_s": state.started.elapsed().as_secs_f64(),
            "active_streams": state.concurrency(),
        }),
    )
}

async fn metrics_prometheus(State(state): State<AppState>) -> Response {
    let mut text = state.metrics.render_prometheus();
    let pool = state.engine.pool().stats();
    use std::fmt::Write as _;
    let _ = writeln!(text, "# TYPE ddl_pool_clients gauge");
    let _ = writeln!(text, "ddl_pool_clients {}", pool.clients);
    let _ = writeln!(text, "ddl_pool_clients_built_total {}", pool.built);
    let _ = writeln!(text, "ddl_pool_clients_evicted_total {}", pool.evictions);
    let _ = writeln!(text, "ddl_dns_cache_hits_total {}", pool.dns_hits);
    let _ = writeln!(text, "ddl_dns_cache_misses_total {}", pool.dns_misses);
    let _ = writeln!(
        text,
        "ddl_registry_superseded_total {}",
        state.engine.registry().stats().superseded
    );
    (
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; version=0.0.4",
        )],
        text,
    )
        .into_response()
}

async fn static_or_404(State(state): State<AppState>, uri: axum::http::Uri) -> Response {
    let Some(dir) = state.static_dir.clone() else {
        return json(
            StatusCode::NOT_FOUND,
            &serde_json::json!({ "code": "HTTP_404" }),
        );
    };
    let path = uri.path();
    let rel = if path == "/" {
        "index.html"
    } else {
        path.trim_start_matches('/')
    };
    // Reject traversal outright; we only ever serve from inside `dir`.
    if rel.contains("..") || rel.contains('\\') || rel.contains('\0') {
        return json(
            StatusCode::NOT_FOUND,
            &serde_json::json!({ "code": "HTTP_404" }),
        );
    }
    let full = std::path::Path::new(dir.as_str()).join(rel);
    match tokio::fs::read(&full).await {
        Ok(bytes) => {
            let ct = guess_content_type(rel);
            (
                StatusCode::OK,
                [(axum::http::header::CONTENT_TYPE, ct)],
                bytes,
            )
                .into_response()
        }
        Err(_) => {
            match tokio::fs::read(std::path::Path::new(dir.as_str()).join("index.html")).await {
                Ok(bytes) => (
                    StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    bytes,
                )
                    .into_response(),
                Err(_) => json(
                    StatusCode::NOT_FOUND,
                    &serde_json::json!({ "code": "HTTP_404" }),
                ),
            }
        }
    }
}

fn guess_content_type(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "png" => "image/png",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------- helpers

fn json<T: serde::Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(mut b) => {
            b.push(b'\n');
            (
                status,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                b,
            )
                .into_response()
        }
        Err(_) => error_response(PlayerError::new(ErrorCode::UnknownError)),
    }
}

/// The single error shape every failing endpoint returns.
pub fn error_response(e: PlayerError) -> Response {
    let status = StatusCode::from_u16(e.http_status()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut body = e.to_json();
    if let Some(o) = body.as_object_mut() {
        o.insert("status".into(), serde_json::Value::from(status.as_u16()));
    }
    let mut builder = Response::builder()
        .status(status)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .header(axum::http::header::CACHE_CONTROL, "no-store")
        // `416` requires `Content-Range: bytes * /<total>`; a body alone is
        // not a conforming answer and browsers rely on it.
        .header(axum::http::header::ACCEPT_RANGES, "bytes");
    if let Some(cr) = &e.content_range {
        builder = builder.header(axum::http::header::CONTENT_RANGE, cr);
    }
    builder
        .body(axum::body::Body::from(
            serde_json::to_vec(&body).unwrap_or_default(),
        ))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn tagged_error_response(e: PlayerError, rid: RequestId) -> Response {
    let mut resp = error_response(e);
    if let Ok(v) = HeaderValue::from_str(&rid.to_string()) {
        resp.headers_mut().insert(HDR_REQUEST_ID, v);
    }
    resp
}

/// Read at most `limit` bytes; refuse anything larger outright.
async fn read_bounded_body(req: Request<Body>, limit: usize) -> Result<Vec<u8>, PlayerError> {
    if let Some(len) = req
        .headers()
        .get(axum::http::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok())
    {
        if len > limit {
            return Err(PlayerError::new(ErrorCode::InvalidUrl)
                .with_reason("request body too large")
                .with_user_action("Send a shorter request."));
        }
    }
    let limited = http_body_util::Limited::new(req.into_body(), limit);
    let bytes = http_body_util::BodyExt::collect(limited)
        .await
        .map_err(|_| {
            PlayerError::new(ErrorCode::InvalidUrl)
                .with_reason("request body exceeds the size limit")
        })?;
    Ok(bytes.to_bytes().to_vec())
}

/// Bind, serve, and shut down cleanly on signal.
pub async fn serve(
    addr: SocketAddr,
    router: Router,
    shutdown: CancellationToken,
) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let local = listener.local_addr()?;
    tracing::info!(%local, "listening");
    let signal = shutdown.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            signal.cancelled().await;
            tracing::info!("shutdown signal received");
        })
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types_for_assets() {
        assert_eq!(guess_content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(
            guess_content_type("a/b/app.js"),
            "text/javascript; charset=utf-8"
        );
        assert_eq!(guess_content_type("app.css"), "text/css; charset=utf-8");
        assert_eq!(guess_content_type("x.bin"), "application/octet-stream");
    }

    #[test]
    fn error_response_carries_the_structured_body() {
        let r = error_response(PlayerError::new(ErrorCode::RangeNotSupported));
        assert_eq!(r.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert_eq!(
            r.headers().get(axum::http::header::CACHE_CONTROL).unwrap(),
            "no-store"
        );
    }

    #[tokio::test]
    async fn bounded_body_rejects_oversized_requests() {
        let req = Request::builder()
            .method("POST")
            .header(axum::http::header::CONTENT_LENGTH, "99999")
            .body(Body::empty())
            .unwrap();
        let e = read_bounded_body(req, 1024).await.unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidUrl);
    }

    #[tokio::test]
    async fn bounded_body_accepts_small_requests() {
        let req = Request::builder()
            .method("POST")
            .body(Body::from("{\"url\":\"https://a.example/x\"}"))
            .unwrap();
        let b = read_bounded_body(req, 1024).await.unwrap();
        assert_eq!(b.len(), 29);
    }
}
