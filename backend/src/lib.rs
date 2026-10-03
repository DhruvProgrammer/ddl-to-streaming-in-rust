//! `ddl-player` — a high-performance DDL streaming proxy.
//!
//! "Vite for DDL streaming": a native `<video>` element on the front, a
//! bounded, cancellable byte-range proxy on the back, and nothing in between
//! that does not earn its keep.

pub mod cache;
pub mod config;
pub mod errors;
pub mod media;
pub mod metrics;
pub mod range;
pub mod retry;
pub mod security;
pub mod server;
pub mod streaming;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use cache::{MemoryStore, MetadataStore};
use config::Config;
use metrics::Metrics;
use streaming::{Engine, OriginPool, StreamRegistry};

/// Build the fully wired application. Exposed so tests can spin up a real
/// server on an ephemeral port.
pub struct App {
    pub state: server::AppState,
    pub router: axum::Router,
    pub cfg: Arc<Config>,
}

pub fn build(cfg: Config) -> App {
    let cfg = Arc::new(cfg);
    let metrics = Arc::new(Metrics::default());
    let pool = Arc::new(OriginPool::new(&cfg));
    let cache: Arc<dyn MetadataStore> = Arc::new(MemoryStore::new(
        cfg.cache_capacity,
        // 1 MiB is plenty for tens of thousands of metadata entries.
        1024 * 1024,
        cfg.cache_ttl,
    ));
    let registry = StreamRegistry::new(cfg.max_concurrent_streams);
    let engine = Engine::new(cfg.clone(), pool, cache, metrics.clone(), registry);
    let state = server::AppState {
        engine,
        metrics,
        shutdown: CancellationToken::new(),
        started: std::time::Instant::now(),
        static_dir: cfg.static_dir.clone().map(Arc::new),
    };
    let router = server::router(state.clone());
    App { state, router, cfg }
}

pub async fn main() -> std::io::Result<()> {
    let cfg = Config::from_env();
    init_tracing(cfg.log_json);
    let addr = cfg.bind;
    let max_buffer = cfg.max_total_buffer_bytes();

    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        %addr,
        max_concurrent_streams = cfg.max_concurrent_streams,
        per_stream_buffer_bytes = cfg.stream_buffer_bytes,
        max_buffer_bytes = max_buffer,
        prefetch_window_bytes = cfg.prefetch_window_bytes,
        "ddl-player starting"
    );
    if cfg.allow_private_hosts {
        tracing::warn!(
            "DDL_ALLOW_PRIVATE_HOSTS is enabled: private, loopback and link-local \
             origins are reachable. This re-opens SSRF and is only appropriate \
             for tests and deliberate self-hosting."
        );
    }
    if max_buffer > 2 * 1024 * 1024 * 1024 {
        tracing::warn!(
            max_buffer_bytes = max_buffer,
            "configured stream buffers could exceed 2 GiB under full concurrency"
        );
    }

    let app = build(cfg);
    let shutdown = app.state.shutdown.clone();
    server::serve(addr, app.router, shutdown).await
}

/// Structured logging. Human-friendly by default, JSON when `DDL_LOG_JSON=1`.
pub fn init_tracing(json: bool) {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_env("DDL_LOG")
        .or_else(|_| EnvFilter::try_new("info"))
        .expect("static filter");
    if json {
        fmt()
            .json()
            .with_env_filter(filter)
            .with_current_span(false)
            .with_span_list(false)
            .init();
    } else {
        fmt()
            .compact()
            .with_env_filter(filter)
            .with_target(false)
            .init();
    }
}

/// Install signal handling; returns a token that fires on Ctrl-C or SIGTERM.
pub fn shutdown_token() -> CancellationToken {
    let token = CancellationToken::new();
    let child = token.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        #[cfg(unix)]
        {
            let mut term =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(s) => s,
                    Err(_) => {
                        let _ = ctrl_c.await;
                        child.cancel();
                        return;
                    }
                };
            tokio::select! {
                _ = ctrl_c => {}
                _ = term.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = ctrl_c.await;
        }
        tracing::info!("shutdown requested");
        child.cancel();
    });
    token
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_wires_every_component() {
        let cfg = Config {
            static_dir: None,
            ..Default::default()
        };
        let app = build(cfg);
        assert_eq!(app.state.engine.registry().stats().in_flight, 0);
        assert!(app.state.engine.pool().stats().dns_hits == 0);
        assert!(app.state.engine.cache().stats().is_some());
    }
}
