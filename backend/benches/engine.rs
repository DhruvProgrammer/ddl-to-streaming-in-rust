//! Micro-benchmarks for the hot paths that do not touch the network:
//! range parsing and resolution, the metrics histogram, the LRU cache, the
//! retry policy and URL validation.
//!
//! These measure CPU cost only. Network behaviour is measured by the k6 and
//! chaos suites, not here.

use std::time::Duration;

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use ddl_player::cache::{MemoryStore, MetadataStore, ResourceMeta};
use ddl_player::config::Config;
use ddl_player::errors::{ErrorCode, PlayerError};
use ddl_player::media;
use ddl_player::metrics::Histogram;
use ddl_player::range::{self, ByteRange, RangeSpec};
use ddl_player::retry::{self, RetryPolicy, RetryState};
use ddl_player::security;
use ddl_player::streaming::pool::RequestId;

const TOTAL: u64 = 734_003_200;
const RANGE_HEADERS: &[&str] = &[
    "bytes=0-",
    "bytes=0-1048575",
    "bytes=1048576-",
    "bytes=5242880-8388607",
    "bytes=-1048576",
    "bytes=734003199-",
    "bytes=100-100",
];

fn bench_ranges(c: &mut Criterion) {
    let mut group = c.benchmark_group("range");
    group.throughput(Throughput::Elements(RANGE_HEADERS.len() as u64));

    group.bench_function("parse", |b| {
        b.iter(|| {
            for h in RANGE_HEADERS {
                black_box(range::parse_range(h));
            }
        })
    });

    group.bench_function("parse+resolve", |b| {
        b.iter(|| {
            for h in RANGE_HEADERS {
                let spec = range::parse_range(h);
                let _ = black_box(range::resolve(spec, Some(TOTAL)));
            }
        })
    });

    group.bench_function("content_range", |b| {
        b.iter(|| {
            black_box(range::parse_content_range("bytes 0-1048575/734003200"));
        })
    });

    group.bench_function("content_length", |b| {
        b.iter(|| black_box(range::parse_content_length("734003200")));
    });

    group.bench_function("etag", |b| {
        b.iter(|| black_box(range::parse_single_etag("\"6d7c1f2a9b\"")));
    });

    // Outbound header construction, i.e. the per-window hot path.
    group.bench_function("request_header", |b| {
        b.iter(|| {
            let r = range::Resolution {
                start: 4_194_304,
                end: Some(8_388_607),
                total: Some(TOTAL),
            };
            black_box(range::request_header(&r));
        })
    });
    group.finish();
}

fn bench_security(c: &mut Criterion) {
    let mut group = c.benchmark_group("security");
    group.bench_function("validate_public", |b| {
        b.iter(|| {
            black_box(security::validate_syntax(
                "https://cdn.example.com/a/b/video.mp4",
            ))
        });
    });
    group.bench_function("validate_private", |b| {
        b.iter(|| {
            black_box(security::validate_syntax(
                "http://192.168.1.10:8080/video.mp4",
            ))
        });
    });
    group.bench_function("redact", |b| {
        let u = url::Url::parse("https://cdn.example.com/a/b/video.mp4?token=secret").unwrap();
        b.iter(|| black_box(security::redact_url(&u)));
    });
    group.bench_function("parse_retry_after", |b| {
        b.iter(|| black_box(retry::parse_retry_after("120", std::time::UNIX_EPOCH)));
    });
    group.finish();
}

fn bench_media(c: &mut Criterion) {
    let mut mp4 = Vec::new();
    mp4.extend_from_slice(&24u32.to_be_bytes());
    mp4.extend_from_slice(b"ftypisom");
    mp4.extend_from_slice(&[0u8; 56]);

    let mut group = c.benchmark_group("media");
    group.bench_function("sniff_mp4", |b| b.iter(|| black_box(media::sniff(&mp4))));
    group.bench_function("identify_content_type", |b| {
        b.iter(|| black_box(media::identify(Some("video/mp4"), "/v.mp4", Some(&mp4))));
    });
    group.bench_function("identify_generic", |b| {
        b.iter(|| {
            black_box(media::identify(
                Some("application/octet-stream"),
                "/v.mp4",
                Some(&mp4),
            ))
        });
    });
    group.finish();
}

fn bench_histogram(c: &mut Criterion) {
    let mut group = c.benchmark_group("metrics");
    let h = Histogram::default();
    group.bench_function("record", |b| b.iter(|| h.record(black_box(12_345))));
    group.throughput(Throughput::Elements(256));
    group.bench_function("quantile_p99", |b| b.iter(|| black_box(h.quantile(0.99))));
    group.finish();
}

fn bench_cache(c: &mut Criterion) {
    let meta = ResourceMeta {
        final_url: "https://cdn.example.com/video.mp4".into(),
        origin: "cdn.example.com:443".into(),
        content_type: Some("video/mp4".into()),
        content_length: Some(TOTAL),
        accept_ranges: Some("bytes".into()),
        range_supported: true,
        etag: Some("\"6d7c1f2a9b\"".into()),
        last_modified: Some("Wed, 21 Oct 2026 07:28:00 GMT".into()),
        media: media::identify(Some("video/mp4"), "/video.mp4", None),
        probed_at: std::time::Instant::now(),
    };

    let mut group = c.benchmark_group("cache");
    let store = MemoryStore::new(4096, 1024 * 1024, Duration::from_secs(300));
    for i in 0..512 {
        store.put(format!("https://cdn.example.com/v{i}.mp4"), meta.clone());
    }

    group.bench_function("get_hit", |b| {
        b.iter(|| black_box(store.get(&"https://cdn.example.com/v7.mp4".to_owned())))
    });
    group.bench_function("get_miss", |b| {
        b.iter(|| black_box(store.get(&"https://cdn.example.com/absent.mp4".to_owned())))
    });
    group.bench_function("put_existing", |b| {
        b.iter(|| {
            store.put("https://cdn.example.com/v7.mp4".into(), meta.clone());
        })
    });
    group.finish();
}

fn bench_retry(c: &mut Criterion) {
    let mut group = c.benchmark_group("retry");
    let policy = std::sync::Arc::new(RetryPolicy::default());

    group.bench_function("backoff_plan", |b| {
        b.iter(|| {
            let mut s = RetryState::new(policy.clone(), black_box(RequestId::next().raw()));
            let d = retry::Decision::Retry { after: None };
            let mut n = 0u32;
            while s.next(d).is_some() {
                n += 1;
            }
            n
        })
    });

    group.bench_function("decide_status", |b| {
        b.iter(|| black_box(retry::decide_status(503, None, &policy)));
    });
    group.finish();
}

fn bench_errors(c: &mut Criterion) {
    let mut group = c.benchmark_group("errors");
    group.bench_function("to_json", |b| {
        let e = PlayerError::new(ErrorCode::RangeNotSupported);
        b.iter(|| black_box(e.to_json()));
    });
    group.bench_function("classify", |b| {
        b.iter(|| black_box(ddl_player::errors::from_status(503)));
    });
    group.finish();
}

fn bench_config(c: &mut Criterion) {
    let mut group = c.benchmark_group("config");
    group.bench_function("total_buffer_bytes", |b| {
        b.iter(|| black_box(Config::default().max_total_buffer_bytes()))
    });
    group.finish();
}

fn bench_error_paths(c: &mut Criterion) {
    let mut group = c.benchmark_group("error_paths");
    let e = PlayerError::new(ErrorCode::Http404).with_reason("upstream status 404");
    group.bench_function("clone_error", |b| b.iter(|| black_box(e.clone())));
    group.bench_function("sanitize_reason", |b| {
        b.iter(|| {
            black_box(
                PlayerError::new(ErrorCode::UpstreamError)
                    .with_reason("stalled for 20000ms while reading body"),
            )
        });
    });
    group.finish();
}

fn bench_ids(c: &mut Criterion) {
    let mut group = c.benchmark_group("identifiers");
    group.bench_function("range_resolve_open", |b| {
        b.iter(|| {
            black_box(range::resolve(
                RangeSpec::Single(ByteRange::FromTo {
                    start: 0,
                    end: None,
                }),
                Some(TOTAL),
            ))
        })
    });
    group.bench_function("request_id_next", |b| {
        b.iter(|| black_box(RequestId::next().raw()));
    });
    group.finish();
}

criterion_group!(
    benches,
    bench_ranges,
    bench_security,
    bench_media,
    bench_histogram,
    bench_cache,
    bench_retry,
    bench_errors,
    bench_config,
    bench_error_paths,
    bench_ids
);
criterion_main!(benches);

#[test]
fn metrics_render_is_sane() {
    let m = Metrics::default();
    assert!(m.render_prometheus().contains("ddl_requests_total"));
}
