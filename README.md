# DDL Player

**Paste a direct download link. It plays in your browser.**

A website, not an application: one page, one native `<video>` element, and a
Rust proxy in front of the origin that does byte ranges, seeking, cancellation
and failure handling properly. Nothing is written to disk.

```
frontend/   Vite + TypeScript, no framework, no runtime dependencies
backend/    Rust + Tokio + Axum + Reqwest
tests/      integration, chaos, leak, browser (Playwright), load (k6 + harness)
docs/       measured performance report
```

---

## Run it

```bash
npm run install:all     # installs the two frontend devDependencies
npm run build           # builds the site and the release binary
npm start               # http://127.0.0.1:8787
```

Or with Docker:

```bash
docker compose up --build              # the website on :8787
docker compose --profile demo up       # plus a local media origin to try
```

Development, with hot reload on the page and the proxy on 8787:

```bash
npm run dev:server      # cargo run
npm run dev:web         # vite, proxies /api to 8787
```

To try it without hunting for a link, run the fixture origin and use
`http://127.0.0.1:9000/media/720p.mp4`. That needs
`DDL_ALLOW_PRIVATE_HOSTS=1`, because the whole point of the SSRF guard is that
it refuses loopback by default.

---

## What it does

1. You paste a link.
2. The browser opens `GET /api/stream?url=…` on a native `<video>` element.
3. The proxy validates the URL, resolves and pins its DNS, validates every
   redirect hop against the same policy, and streams the body through a bounded
   channel.
4. A probe runs **in parallel** with the media request, so nothing is delayed
   waiting to find out whether the file is playable — but you still learn the
   length, the container, whether seeking works, and why not if something is
   wrong.
5. Seeking is a new byte-range request. The previous request's origin work is
   cancelled, not left to finish and be thrown away.

No FFmpeg, no transcoding, no container sniffing on the request path, no
MediaSource. If a browser can play the file, it plays it directly.

---

## The parts that matter

### Byte ranges

`backend/src/range/` parses inbound `Range` and `Content-Range` and outbound
`Range`, rejects hostile values, and refuses to guess a length. Every length
the proxy advertises downstream comes from the live origin response — never
from the cache — so a stale entry cannot corrupt a seek.

The transfer status we return depends on what **the client** asked for, not on
how we happened to window the origin transfer.

### Windowed origin reads

Every origin request is capped at `prefetch_window_bytes` (4 MiB by default)
and fully consumed, so its connection returns to the pool and is reused across
seeks. There is **no speculative over-fetch**: we only continue a window when
the client still wants more. Measured over-fetch under a read-then-abandon
workload was 7–16 %, which is exactly the partial final window.

### Seeking and cancellation

`backend/src/streaming/registry.rs` gives each playback session a monotonically
increasing generation. A newer generation cancels the previous one's upstream
work immediately; an older one delivered late is refused with `409`, so it can
never overwrite the live stream. Equal generations are allowed, because a
browser may legitimately have two ranges open at once.

Everything has an owner. The concurrency permit, the cancellation token, the
origin response and the client channel are all held by one task, and the
live-stream gauge is decremented by an RAII guard — including on the one path
nobody writes down, where the client disappears while we are still planning.
That path was a real leak, found by k6, and is now covered by a regression
test.

### Bounded everything

| Bound | Default | Where |
|---|---|---|
| Concurrent streams | 256 | `DDL_MAX_CONCURRENT_STREAMS` |
| Memory per stream | 512 KiB | `DDL_STREAM_BUFFER_BYTES` |
| Origin read window | 4 MiB | `DDL_PREFETCH_WINDOW_BYTES` |
| Redirects | 5 | `DDL_MAX_REDIRECTS` |
| Retry attempts / budget | 4 / 5 s | `DDL_MAX_RETRIES` |
| Metadata cache | 4096 entries / 1 MiB | `DDL_CACHE_CAPACITY` |

Media bytes are never accumulated anywhere. A 20 GB file costs the same
memory as a 20 MB one, and that is tested rather than asserted.

### Security

Every DDL is hostile input.

- Scheme, credentials, length and control characters are checked before DNS.
- Names that can only mean "inside the network" (`localhost`, `*.internal`,
  `metadata.google.internal`, …) are refused under **every** policy.
- The host is resolved by us, every answer is checked, and the resolved
  address is pinned into the connection — so the DNS-rebinding window does not
  exist.
- Loopback, RFC1918, CGNAT, link-local, documentation, benchmarking, multicast
  and reserved space are refused, in IPv4 and IPv6, including `::ffff:` and
  NAT64 embeddings.
- Every redirect hop is revalidated from scratch.

`DDL_ALLOW_PRIVATE_HOSTS=1` relaxes *address classes* for self-hosting and for
the test suites. It never relaxes the name rules, and it logs a warning at
startup.

### Errors

Every failure has a code, a sentence, a reason, a retry verdict and a next
action. There is no "something went wrong".

```json
{
  "code": "RANGE_NOT_SUPPORTED",
  "message": "The source does not support byte-range requests.",
  "reason": "origin answered 200 for a range request with Accept-Ranges: none",
  "retryable": false,
  "user_action": "Seeking may be limited on this source."
}
```

### Observability

`/metrics` (Prometheus text) and `/api/stats` (JSON with percentiles and limits).
Counters, gauges and 4-buckets-per-octave histograms. Query strings and
credentials are stripped from every log line and every error body.

---

## API

```http
POST /api/probe
{ "url": "https://example.com/video.mp4", "refresh": false }
->
{ "streamable": true, "content_type": "video/mp4",
  "content_length": 4999379, "range_supported": true,
  "container": "mp4", "evidence": "content-type",
  "ttfb_ms": 3.1, "probe_ms": 3.4, "cached": false, "warning": null }
```

```http
GET /api/stream?url=<encoded>[&s=<session>&g=<generation>]
Range: bytes=1048576-
->
206 Partial Content
Content-Range: bytes 1048576-4999378/4999379
Content-Length: 3950803
Content-Type: video/mp4
Accept-Ranges: bytes
X-DDL-Request-Id: 3f2a…
X-DDL-Range-Support: bytes | none
```

Session and generation travel as query parameters because a native
`<video src>` cannot carry custom headers. Header equivalents
(`x-ddl-session`, `x-ddl-generation`) are accepted for API clients and win if
present.

Also: `GET /api/health`, `GET /api/stats`, `GET /metrics`,
`POST /api/client-events` (counting only: play / seek / stall timings).

---

## Configuration

Everything is an environment variable; the defaults are the ones in
`backend/src/config.rs`.

| Variable | Default | Meaning |
|---|---|---|
| `DDL_BIND` | `0.0.0.0:8787` | listen address |
| `DDL_STATIC_DIR` | `frontend/dist` | built site to serve |
| `DDL_MAX_CONCURRENT_STREAMS` | `256` | hard ceiling; excess gets `429`, never a queue |
| `DDL_STREAM_BUFFER_BYTES` | `524288` | per-stream memory bound |
| `DDL_PREFETCH_WINDOW_BYTES` | `4194304` | max bytes per origin request |
| `DDL_CONNECT_TIMEOUT_MS` | `5000` | TCP connect |
| `DDL_RESPONSE_TIMEOUT_MS` | `15000` | TTFB deadline |
| `DDL_IDLE_TIMEOUT_MS` | `20000` | max gap between origin chunks |
| `DDL_DNS_TIMEOUT_MS` | `5000` | name resolution |
| `DDL_MAX_REDIRECTS` | `5` | redirect chain length |
| `DDL_MAX_RETRIES` | `4` | attempts per request |
| `DDL_RETRY_BASE_MS` / `DDL_RETRY_MAX_MS` | `100` / `2000` | backoff |
| `DDL_CACHE_CAPACITY` | `4096` | metadata entries |
| `DDL_CACHE_TTL_MS` | `300000` | metadata lifetime |
| `DDL_ALLOWED_MEDIA_TYPES` | see config | comma-separated override |
| `DDL_ALLOW_PRIVATE_HOSTS` | `0` | **re-opens SSRF**; tests and self-hosting only |
| `DDL_LOG_JSON` | `0` | structured logs |

---

## Tests

```bash
npm run test:rust        # 125 unit tests
cargo test --manifest-path backend/Cargo.toml --test integration   # 37 end-to-end
npm run test:chaos       # 17 fault-injection scenarios
npm run test:leaks       # 8 resource-leak and long-run scenarios
npm run bench            # criterion micro-benchmarks

powershell -NoProfile -ExecutionPolicy Bypass -File scripts\with-browser.ps1 -Project chromium
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\with-browser.ps1 -Project firefox
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\with-load.ps1 -Levels 1,10,50,100,200,256
k6 run tests/load/k6.js
```

The browser tests drive the real built page against a real origin and assert
that playback actually starts, that `currentTime` advances, that a seek lands
where it was asked to, and that failures produce sentences.

`tests/support/fixture-origin.mjs` is a dependency-free HTTP origin that serves
the fixtures and can be told to misbehave: `slow`, `flaky`, `truncate`,
`range-less`, `no-length`, `disconnect`, `redirect-loop`, `ssrf-redirect`,
`html`, `500`/`502`/`503`/`504`/`429`.

---

## Measured

Real numbers from this machine, with the commands that produce them, are in
[docs/PERFORMANCE.md](docs/PERFORMANCE.md). Everything there is labelled
MEASURED or EXPECTED, and nothing is extrapolated past what was run.

## Decisions worth knowing about

- **No MediaSource, no fetch, no service worker.** Native playback starts
  faster, uses less CPU and holds less memory. Custom headers would have forced
  one of them; query parameters cost nothing.
- **The probe is 1 KiB, not 1 byte.** `bytes=0-0` is cheaper but gives no
  container signature, so "is this actually MP4?" would be a guess. 1 KiB is
  the smallest request that lets us sniff the container.
- **The URL extension is the weakest evidence there is, and the last used.** A
  real direct-download link is routinely `/download/37334`, `/get?id=12345` or
  `/file?token=…` — no extension, an opaque id, a signature in the query. So
  container identification is ranked: response bytes, then `Content-Type`, then
  a `Content-Disposition` filename (the origin describing its own file), and
  only then the URL. Critically, once a body has been read, the extension can
  no longer rescue it: an HTML error page behind a `.mp4` path is an error page,
  and believing the URL is how an expired link gets reported as "convert your
  file to MP4".
- **The streaming path reads the body too, when it has to.** The probe is not in
  the critical path, so it was once allowed to trust `Content-Type` while the
  stream endpoint did not. That made the two disagree: the probe would say
  playable and the stream endpoint would answer 415 for the same URL, so nothing
  ever played. When the headers and the extension are both inconclusive, the
  stream path now fetches a 1 KiB prefix and identifies from the bytes — one
  small request on the cold-cache path only.
- **A refused source says what actually came back.** `text/html`, an empty body,
  a JSON error envelope and a content-encoded body are four different problems
  with four different fixes, and all four used to be reported as "unsupported
  container". Now they are named, because the viewer's file is usually fine and
  their link is what expired.
- **MPEG-TS, FLV, AVI and Matroska are refused with a reason.** They are valid
  video and no browser will play them from a bare `<video src>`; WebM is a
  constrained subset of Matroska, not the same thing, and reporting a `.mkv` as
  `video/webm` just moves the failure to the browser. Remuxing would mean FFmpeg
  in the request path. The seam for it is one function in the engine; today it
  reports why it will not, instead of half-playing the file.
- **HLS manifests are refused, deliberately.** Serving the manifest is easy,
  but the segment requests that follow would go straight to the origin, outside
  the proxy — which breaks signed and temporary links outright. Reporting it as
  playable and then failing is worse than saying so up front.
- **The metadata cache is not used to decide lengths.** Caching a length and
  trusting it later is how a proxy serves a corrupt seek. The cache makes
  probes fast; it never makes a byte range.