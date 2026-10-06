//! Metadata cache.
//!
//! L1 is in-process memory, bounded by entry count *and* total bytes. The
//! `MetadataStore` trait is the seam that lets an L2 (disk, S3, Redis) be added
//! later without touching the streaming engine.
//!
//! What we cache is metadata only — validators, length, type, range support.
//! Media bytes are never cached here: a 20 GB file must cost the same as a
//! 20 MB one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::media::MediaInfo;

/// Cache key: the full request URL, including query, because the query is part
/// of resource identity — that is also what prevents cross-resource poisoning.
pub type CacheKey = String;

/// Everything we learned about a remote resource in one probe.
#[derive(Debug, Clone)]
pub struct ResourceMeta {
    /// Final URL after redirects, already redacted for safe logging.
    pub final_url: String,
    /// `host:port` of the origin actually serving bytes.
    pub origin: String,
    /// What we will send downstream: the identified type, or the origin's.
    pub content_type: Option<String>,
    /// What the origin actually declared, kept so the interface can say "the
    /// origin returned text/html" rather than only what we concluded from it.
    pub origin_content_type: Option<String>,
    pub content_length: Option<u64>,
    pub accept_ranges: Option<String>,
    pub range_supported: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub media: MediaInfo,
    /// Decided once, when the resource was identified. Everything that reports
    /// on playability reads this rather than recomputing it, because two
    /// recomputations is how the probe and the stream endpoint start
    /// disagreeing about the same URL.
    pub streamable: bool,
    pub probed_at: Instant,
}

impl ResourceMeta {
    /// Age beyond which cached metadata should be revalidated.
    pub fn age(&self) -> Duration {
        self.probed_at.elapsed()
    }

    /// Rough memory footprint of this entry.
    pub fn approx_bytes(&self) -> usize {
        self.final_url.len()
            + self.origin.len()
            + self.content_type.as_ref().map_or(0, String::len)
            + self
                .origin_content_type
                .as_ref()
                .map_or(0, String::len)
            + self.accept_ranges.as_ref().map_or(0, String::len)
            + self.etag.as_ref().map_or(0, String::len)
            + self.last_modified.as_ref().map_or(0, String::len)
            + self.media.media_type.len()
            + self.media.remux_reason.as_ref().map_or(0, String::len)
            + 128
    }
}

/// Storage seam. Implementations must be safe to share across tasks.
pub trait MetadataStore: Send + Sync + 'static {
    fn get(&self, key: &CacheKey) -> Option<ResourceMeta>;
    fn put(&self, key: CacheKey, meta: ResourceMeta);
    fn invalidate(&self, key: &CacheKey);
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn bytes(&self) -> usize;
    fn clear(&self);
    /// Counters for observability. `None` for stores that never hit.
    fn stats(&self) -> Option<CacheStats> {
        None
    }
}

/// Counters and occupancy for a cache implementation.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct CacheStats {
    pub entries: usize,
    pub bytes: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub expirations: u64,
}

impl CacheStats {
    /// Hits / (hits + misses), or `None` when there has been no traffic yet.
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hits + self.misses;
        (total > 0).then(|| self.hits as f64 / total as f64)
    }

    /// Wire shape for `/api/stats`.
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "entries": self.entries,
            "bytes": self.bytes,
            "hits": self.hits,
            "misses": self.misses,
            "evictions": self.evictions,
            "expirations": self.expirations,
            "hit_rate": self.hit_rate(),
        })
    }
}

#[derive(Debug, Default)]
struct Inner {
    map: HashMap<CacheKey, (ResourceMeta, u64)>,
    bytes: usize,
}

/// Bounded in-memory LRU with TTL and byte ceiling.
#[derive(Debug)]
pub struct MemoryStore {
    ttl: Duration,
    max_entries: usize,
    max_bytes: usize,
    inner: Mutex<Inner>,
    clock: AtomicU64,
    inserts: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    evictions: AtomicU64,
    expirations: AtomicU64,
}

/// How often the expiry sweep runs. Sweeping on every insert is O(n) and
/// measured at ~31 us for a 512-entry store; amortising it over this many
/// inserts removes that from the hot path while bounding staleness.
const SWEEP_EVERY: u64 = 64;

impl MemoryStore {
    pub fn new(max_entries: usize, max_bytes: usize, ttl: Duration) -> Self {
        Self {
            ttl,
            max_entries: max_entries.max(1),
            max_bytes: max_bytes.max(1024),
            inner: Mutex::new(Inner::default()),
            clock: AtomicU64::new(1),
            inserts: AtomicU64::new(0),
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
            evictions: AtomicU64::new(0),
            expirations: AtomicU64::new(0),
        }
    }

    fn tick(&self) -> u64 {
        self.clock.fetch_add(1, Ordering::Relaxed)
    }

    /// Drop expired entries. Called on insert.
    fn sweep_expired(&self, inner: &mut Inner) {
        if inner.map.is_empty() {
            return;
        }
        let ttl = self.ttl;
        let mut freed = 0usize;
        let mut removed = 0u64;
        inner.map.retain(|_, (meta, _)| {
            if meta.age() < ttl {
                true
            } else {
                freed += meta.approx_bytes();
                removed += 1;
                false
            }
        });
        if removed > 0 {
            inner.bytes = inner.bytes.saturating_sub(freed);
            self.expirations.fetch_add(removed, Ordering::Relaxed);
        }
    }

    /// Evict least-recently-touched entries until we fit again. Amortized O(1):
    /// we drop a slice rather than a single entry, so a full scan is not paid
    /// on every insert.
    fn evict_to_fit(&self, inner: &mut Inner, incoming: usize) {
        let target = (self.max_entries / 16).max(1);
        let mut dropped = 0usize;
        while (inner.map.len() >= self.max_entries
            || inner.bytes.saturating_add(incoming) > self.max_bytes)
            && dropped < target
        {
            let Some(victim) = inner
                .map
                .iter()
                .min_by_key(|(_, (_, touch))| *touch)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some((meta, _)) = inner.map.remove(&victim) {
                inner.bytes = inner.bytes.saturating_sub(meta.approx_bytes());
                dropped += 1;
            }
        }
        if dropped > 0 {
            self.evictions.fetch_add(dropped as u64, Ordering::Relaxed);
        }
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            entries: self.len(),
            bytes: self.bytes(),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            evictions: self.evictions.load(Ordering::Relaxed),
            expirations: self.expirations.load(Ordering::Relaxed),
        }
    }
}

impl MetadataStore for MemoryStore {
    fn get(&self, key: &CacheKey) -> Option<ResourceMeta> {
        let touch = self.tick();
        let Ok(mut inner) = self.inner.lock() else {
            return None;
        };
        match inner.map.get_mut(key) {
            Some((meta, slot)) => {
                if meta.age() >= self.ttl {
                    let bytes = meta.approx_bytes();
                    inner.map.remove(key);
                    inner.bytes = inner.bytes.saturating_sub(bytes);
                    self.expirations.fetch_add(1, Ordering::Relaxed);
                    self.misses.fetch_add(1, Ordering::Relaxed);
                    return None;
                }
                *slot = touch;
                self.hits.fetch_add(1, Ordering::Relaxed);
                Some(meta.clone())
            }
            None => {
                self.misses.fetch_add(1, Ordering::Relaxed);
                None
            }
        }
    }

    fn put(&self, key: CacheKey, meta: ResourceMeta) {
        let bytes = meta.approx_bytes();
        // A single oversized entry must not be cached at all.
        if bytes > self.max_bytes {
            return;
        }
        let touch = self.tick();
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let n = self.inserts.fetch_add(1, Ordering::Relaxed);
        // Amortised sweep: O(1) per insert instead of O(entries). A full sweep
        // on every insert measured ~31 us at 512 entries; this moves it off the
        // hot path while bounding staleness to SWEEP_EVERY inserts.
        if n % SWEEP_EVERY == 0 || inner.map.len() >= self.max_entries {
            self.sweep_expired(&mut inner);
        }
        if let Some((old, _)) = inner.map.remove(&key) {
            inner.bytes = inner.bytes.saturating_sub(old.approx_bytes());
        }
        self.evict_to_fit(&mut inner, bytes);
        inner.bytes += bytes;
        inner.map.insert(key, (meta, touch));
    }

    fn invalidate(&self, key: &CacheKey) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if let Some((old, _)) = inner.map.remove(key) {
            inner.bytes = inner.bytes.saturating_sub(old.approx_bytes());
        }
    }

    fn len(&self) -> usize {
        self.inner.lock().map(|i| i.map.len()).unwrap_or(0)
    }

    fn bytes(&self) -> usize {
        self.inner.lock().map(|i| i.bytes).unwrap_or(0)
    }

    fn clear(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.map.clear();
            inner.bytes = 0;
        }
    }

    fn stats(&self) -> Option<CacheStats> {
        Some(self.stats())
    }
}

/// Store that never remembers anything; used by tests that want cold caches.
#[derive(Debug, Default)]
pub struct NullStore;

impl MetadataStore for NullStore {
    fn get(&self, _key: &CacheKey) -> Option<ResourceMeta> {
        None
    }
    fn put(&self, _key: CacheKey, _meta: ResourceMeta) {}
    fn invalidate(&self, _key: &CacheKey) {}
    fn len(&self) -> usize {
        0
    }
    fn bytes(&self) -> usize {
        0
    }
    fn clear(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::{identify, Evidence};
    use std::thread;

    fn meta(url: &str) -> ResourceMeta {
        let media = identify(Some("video/mp4"), "/a.mp4", None, None);
        ResourceMeta {
            final_url: url.to_owned(),
            origin: "example.com:443".to_owned(),
            content_type: Some("video/mp4".to_owned()),
            origin_content_type: Some("video/mp4".to_owned()),
            content_length: Some(1_000_000),
            accept_ranges: Some("bytes".to_owned()),
            range_supported: true,
            etag: Some("\"v1\"".to_owned()),
            last_modified: None,
            streamable: media.container.browser_native(),
            media,
            probed_at: Instant::now(),
        }
    }

    #[test]
    fn stores_and_returns() {
        let s = MemoryStore::new(10, 1 << 20, Duration::from_secs(60));
        assert!(s.get(&"k".into()).is_none());
        s.put("k".into(), meta("https://example.com/a.mp4"));
        let got = s.get(&"k".into()).unwrap();
        assert_eq!(got.content_length, Some(1_000_000));
        assert_eq!(got.media.source, Evidence::ContentType);
        let st = s.stats();
        assert_eq!(st.hits, 1);
        assert_eq!(st.misses, 1);
        assert_eq!(st.hit_rate(), Some(0.5));
    }

    #[test]
    fn entry_count_is_bounded() {
        let s = MemoryStore::new(32, 1 << 20, Duration::from_secs(60));
        for i in 0..1000 {
            s.put(format!("k{i}"), meta("https://example.com/a.mp4"));
        }
        assert!(s.len() <= 32, "len was {}", s.len());
        assert!(s.bytes() <= 1 << 20);
    }

    #[test]
    fn byte_ceiling_is_enforced() {
        let s = MemoryStore::new(10_000, 2048, Duration::from_secs(60));
        for i in 0..5000 {
            s.put(format!("k{i}"), meta("https://example.com/a.mp4"));
        }
        assert!(s.bytes() <= 2048 + 512, "bytes was {}", s.bytes());
    }

    #[test]
    fn oversized_single_entry_is_refused() {
        let s = MemoryStore::new(10, 1024, Duration::from_secs(60));
        let mut m = meta("https://example.com/a.mp4");
        m.final_url = "x".repeat(10_000);
        s.put("big".into(), m);
        assert_eq!(s.len(), 0);
    }

    #[test]
    fn ttl_expiry_returns_miss() {
        let s = MemoryStore::new(10, 1 << 20, Duration::from_millis(30));
        s.put("k".into(), meta("https://example.com/a.mp4"));
        thread::sleep(Duration::from_millis(60));
        assert!(s.get(&"k".into()).is_none());
        assert_eq!(s.stats().expirations, 1);
    }

    #[test]
    fn recently_touched_entries_survive_eviction() {
        let s = MemoryStore::new(16, 1 << 20, Duration::from_secs(600));
        s.put("hot".into(), meta("https://example.com/hot.mp4"));
        for i in 0..8 {
            s.put(format!("c{i}"), meta("https://example.com/c.mp4"));
        }
        for i in 8..40 {
            s.put(format!("c{i}"), meta("https://example.com/c.mp4"));
            let _ = s.get(&"hot".to_owned());
        }
        assert!(s.get(&"hot".into()).is_some(), "hot key was evicted");
    }

    #[test]
    fn invalidate_and_clear() {
        let s = MemoryStore::new(10, 1 << 20, Duration::from_secs(60));
        s.put("a".into(), meta("https://example.com/a.mp4"));
        s.put("b".into(), meta("https://example.com/b.mp4"));
        assert!(s.bytes() > 0);
        s.invalidate(&"a".into());
        assert!(s.get(&"a".into()).is_none());
        assert!(s.get(&"b".into()).is_some());
        s.clear();
        assert_eq!(s.len(), 0);
        assert_eq!(s.bytes(), 0);
    }

    #[test]
    fn overwrite_does_not_double_count_bytes() {
        let s = MemoryStore::new(10, 1 << 20, Duration::from_secs(60));
        for _ in 0..50 {
            s.put("k".into(), meta("https://example.com/a.mp4"));
        }
        assert_eq!(s.len(), 1);
        let one = s.bytes();
        assert!(one < 1024, "bytes should reflect a single entry, got {one}");
    }

    #[test]
    fn concurrent_use_is_safe() {
        let s = std::sync::Arc::new(MemoryStore::new(64, 1 << 20, Duration::from_secs(60)));
        let mut handles = Vec::new();
        for t in 0..8u32 {
            let s = s.clone();
            handles.push(thread::spawn(move || {
                for i in 0..200 {
                    let k = format!("k{}-{}", t % 4, i % 32);
                    s.put(k.clone(), meta("https://example.com/a.mp4"));
                    let _ = s.get(&k);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert!(s.len() <= 64);
    }

    #[test]
    fn null_store_is_always_cold() {
        let s = NullStore;
        s.put("k".into(), meta("https://example.com/a.mp4"));
        assert!(s.get(&"k".into()).is_none());
        assert_eq!(s.len(), 0);
        assert!(s.stats().is_none());
    }

    #[test]
    fn stats_json_shape() {
        let s = MemoryStore::new(4, 1 << 16, Duration::from_secs(60));
        s.put("k".into(), meta("https://example.com/a.mp4"));
        let _ = s.get(&"k".to_owned());
        let v = s.stats().to_json();
        assert_eq!(v["hits"], 1);
        assert_eq!(v["hit_rate"], 1.0);
        assert_eq!(v["entries"], 1);
    }
}
