# DDL Player — Rust Video Streaming Proxy for Direct Download Links

[![Rust](https://img.shields.io/badge/rust-1.82%2B-000?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![TypeScript](https://img.shields.io/badge/typescript-5.6-3178C6?logo=typescript&logoColor=white)](https://www.typescriptlang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)
[![Tokio](https://img.shields.io/badge/async-tokio-2E2E2E?logo=rust&logoColor=white)](https://tokio.rs/)
[![Axum](https://img.shields.io/badge/web-axum-DD4444)](https://github.com/tokio-rs/axum)

**Paste a direct download link (DDL). It plays in your browser — with real byte-range seeking, no transcoding, and nothing written to disk.**

DDL Player is a self-hosted **video streaming proxy written in Rust** with a
zero-dependency **TypeScript front end**. It solves the problem every `<video>`
element runs into: browsers refuse to seek media they did not fetch
themselves, and a cross-origin file server usually will not hand one over.

Point it at a link like `https://example.com/download/37334` — no extension, an
opaque ID, a signed query string, a Cloudflare Tunnel host — and it streams.

```
frontend/   Vite + TypeScript, no framework, no runtime dependencies
backend/    Rust + Tokio + Axum + Reqwest
tests/      integration, chaos, leak, browser (Playwright), load (k6)
docs/       measured performance report
```

---

## Table of contents

- [Why this exists](#why-this-exists)
- [Features](#features)
- [Quick start](#quick-start)
- [Use cases](#use-cases)
- [How it works](#how-it-works)
- [Direct download links without file extensions](#direct-download-links-without-file-extensions)
- [API reference](#api-reference)
- [Configuration](#configuration)
- [Security](#security)
- [Performance](#performance)
- [Testing](#testing)
- [How this compares](#how-this-compares)
- [FAQ](#faq)
- [Contributing](#contributing)
- [License](#license)

---

## Why this exists

Serving video from a download host fails in four predictable ways:

1. **No seeking.** `<video>` can only seek media it fetched itself, and a cross-origin
   server that does not expose `Content-Range` gives you a file you can watch but
   cannot scrub.
2. **CORS.** The origin usually sends no `Access-Control-Allow-Origin`, so the browser
   blocks the response before a byte is decoded.
3. **Signed and temporary links.** Query-string tokens, expiring URLs and `/download/<id>`
   paths have no file extension, so anything that guesses by extension refuses them.
4. **Ambiguous failures.** A dead link returns an HTML page behind a `200`, and a player
   that trusts the path reports "unsupported format" — sending you to convert a file that
   was never broken.

DDL Player is the proxy in front. It makes the bytes look like they came from
your own origin, and it decides what a link actually is by looking at the
response rather than at the URL.

## Features

**Streaming**

- True **HTTP byte-range seeking** (`206 Partial Content`), not a restart
- Native `<video>` playback — no MediaSource, no MSE, no service worker
- **Progressive streaming**: playback starts on the first frame, not after the
  whole file downloads
- Per-session cancellation, so a seek aborts the old upstream request instead
  of discarding it afterwards
- Bounded memory: a 20 GB file costs the same as a 20 MB one
- Connection pooling with windowed origin reads (4 MiB default)

**Link handling**

- Works with **extensionless DDL links** — `/download/37334`, `/get?id=12345`,
  `/file?token=…`, signed URLs, temporary hosts
- Container detection from **magic bytes**, `Content-Type` and
  `Content-Disposition`; the URL extension is the weakest signal and the last one
- Follows redirects, revalidating **every hop**
- Honours `Content-Length`, `Content-Range`, `Accept-Ranges`, `ETag` and
  `Last-Modified`
- Failures name the real cause: expired link, empty body, JSON error envelope,
  compressed body — not "unsupported format"

**Security**

- Full **SSRF protection**: scheme allowlist, DNS resolution and pinning (no
  rebinding window), private/loopback/link-local/CGNAT refusal in IPv4 and IPv6,
  every redirect hop revalidated
- Hostname denylist for internal-only names, trailing-dot normalisation
- Credentials stripped from every log line and error body
- 256 concurrent streams maximum, enforced with `429` rather than an
  unbounded queue

**Operations**

- Prometheus `/metrics` and a JSON `/api/stats` endpoint
- Structured logs, health check, client-event telemetry
- Single static binary, no runtime dependencies, Docker image included

## Quick start

Requires Rust 1.82+ (the crate's `rust-version`) and Node 20+.

```bash
git clone https://github.com/DhruvProgrammer/ddl-to-streaming-in-rust.git
cd ddl-to-streaming-in-rust

npm run install:all     # frontend dev dependencies
npm run build           # builds the site and the release binary
npm start               # http://127.0.0.1:8787
```

With Docker:

```bash
docker compose up --build              # the player on :8787
docker compose --profile demo up       # plus a local media origin to try
```

To have something to play without hunting for a link, run the bundled fixture
origin and open `http://127.0.0.1:9000/media/720p.mp4`. It needs
`DDL_ALLOW_PRIVATE_HOSTS=1`, because the SSRF guard refuses loopback by design.

Development, with hot reload on the page and the proxy on 8787:

```bash
npm run dev:server      # cargo run
npm run dev:web         # vite, proxies /api to 8787
```

## Use cases

- **Self-hosted media player** — serve files from any host, including ones that
  are not yours and send no CORS headers
- **Watch a direct download in the browser** — no VLC, no download folder
- **Scrub long files instantly** — range requests instead of re-downloading
- **Private or signed links** — tokens stay in the URL, never in the page
- **Internal tooling** — a video preview for an object store, a backup bucket or
  a NAS, without opening those services to the internet
- **Learning reference** — a production-shaped Rust streaming server: bounded
  concurrency, cancellation, SSRF defence, Prometheus metrics

## How it works

1. You paste a link.
2. The browser points a native `<video>` element at
   `GET /api/stream?url=…`.
3. The proxy validates the URL, resolves and **pins** its DNS, validates every
   redirect hop against the same policy, and streams the body through a bounded
   channel.
4. A 1 KiB **probe** runs *in parallel* with the media request — nothing waits
   on it — and reports the length, the container, whether seeking will work, and
   why not if something is wrong.
5. Seeking issues a new byte-range request. The previous upstream request is
   cancelled rather than left to finish and be thrown away.

No FFmpeg, no transcoding, no remuxing on the request path. If a browser can
play the file, it plays directly.

## Direct download links without file extensions

This is the part most proxies get wrong, so it is worth being explicit.

A valid DDL link frequently has **no file extension at all**:

```
https://example.com/video.mp4                       ← has an extension
https://example.com/download/37334                  ← does not
https://example.com/download/A_SLu4ViVaRA1e51eV8yNiRGIQpB4_D25-okgLU5OxlphDIfGg
https://example.com/get?id=12345                    ← id in the query
https://example.com/file?token=abcdef               ← signed / temporary
```

So DDL Player ranks its evidence, strongest first:

| Rank | Evidence | Notes |
|---|---|---|
| 1 | **Response bytes** | Container magic, read from a 1 KiB prefix |
| 2 | **`Content-Type`** | Parsed, parameters and case handled |
| 3 | **`Content-Disposition` filename** | The origin naming its own file |
| 4 | **URL extension** | Last resort only |

The rule that matters: **once a body has been read, the URL extension can no
longer rescue it.** An HTML error page behind a `.mp4` path is an error page.
Believing the extension is how an expired link gets reported as "convert your
file to MP4".

The same ranking applies on the streaming path. When the headers and the
extension are both inconclusive, the stream endpoint fetches a short prefix and
identifies from the bytes — which is what makes a cold, un-probed,
extensionless link play instead of returning `415`.

## API reference

### `POST /api/probe`

Identifies a source without transferring it. Safe to call on any URL.

```http
POST /api/probe
{ "url": "https://example.com/download/37334", "refresh": false }
```

```json
{
  "streamable": true,
  "content_type": "video/mp4",
  "origin_content_type": "application/octet-stream",
  "content_length": 4999379,
  "range_supported": true,
  "accept_ranges": "bytes",
  "container": "mp4",
  "evidence": "magic-bytes",
  "needs_remux": false,
  "etag": "\"4c48d3\"",
  "last_modified": "Thu, 01 Jan 2026 00:00:00 GMT",
  "redirects": 0,
  "ttfb_ms": 1.5,
  "probe_ms": 1.6,
  "cached": false,
  "reason": null,
  "warning": null
}
```

`evidence` names what decided the verdict — `magic-bytes`, `content-type`,
`content-disposition` or `extension` — so a surprising result is explainable.

### `GET /api/stream?url=<encoded>[&s=<session>&g=<generation>]`

```http
GET /api/stream?url=https%3A%2F%2Fexample.com%2Fdownload%2F37334
Range: bytes=1048576-
->
206 Partial Content
Content-Range: bytes 1048576-4999378/4999379
Content-Length: 3950803
Content-Type: video/mp4
Accept-Ranges: bytes
X-DDL-Request-Id: 3f2a…
X-DDL-Generation: 4
X-DDL-Range-Support: bytes
```

Session and generation travel as query parameters because a native
`<video src>` cannot carry custom headers. Header equivalents
(`x-ddl-session`, `x-ddl-generation`) are accepted for API clients and win when
present.

### Everything else

| Endpoint | Purpose |
|---|---|
| `GET /api/health` | Liveness |
| `GET /api/stats` | JSON metrics, percentiles and limits |
| `GET /metrics` | Prometheus text exposition |
| `POST /api/client-events` | Client telemetry: play / seek / stall timings |

### Errors

Every failure carries a code, a sentence, a reason, a retry verdict and a next
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

## Configuration

Everything is an environment variable; the defaults live in
`backend/src/config.rs`.

| Variable | Default | Meaning |
|---|---|---|
| `DDL_BIND` | `0.0.0.0:8787` | Listen address |
| `DDL_STATIC_DIR` | `frontend/dist` | Built site to serve |
| `DDL_MAX_CONCURRENT_STREAMS` | `256` | Hard ceiling; excess gets `429`, never a queue |
| `DDL_STREAM_BUFFER_BYTES` | `524288` | Per-stream memory bound |
| `DDL_PREFETCH_WINDOW_BYTES` | `4194304` | Max bytes per origin request |
| `DDL_CONNECT_TIMEOUT_MS` | `5000` | TCP connect |
| `DDL_RESPONSE_TIMEOUT_MS` | `15000` | TTFB deadline |
| `DDL_IDLE_TIMEOUT_MS` | `20000` | Max gap between origin chunks |
| `DDL_DNS_TIMEOUT_MS` | `5000` | Name resolution |
| `DDL_MAX_REDIRECTS` | `5` | Redirect chain length |
| `DDL_MAX_RETRIES` | `4` | Attempts per request |
| `DDL_RETRY_BASE_MS` / `DDL_RETRY_MAX_MS` | `100` / `2000` | Backoff |
| `DDL_CACHE_CAPACITY` | `4096` | Metadata entries |
| `DDL_CACHE_TTL_MS` | `300000` | Metadata lifetime |
| `DDL_ALLOWED_MEDIA_TYPES` | MP4, WebM, Ogg, MOV, MP3, WAV | Comma-separated override |
| `DDL_ALLOW_PRIVATE_HOSTS` | `0` | **Re-opens SSRF.** Self-hosting and tests only |
| `DDL_LOG_JSON` | `0` | Structured logs |

## Security

Every DDL is hostile input.

- Scheme, credentials, length and control characters are checked before DNS.
- Names that can only mean "inside the network" (`localhost`, `*.internal`,
  `*.local`, `metadata.google.internal`, …) are refused under **every** policy,
  including after trailing-dot normalisation.
- The host is resolved by us, every answer is checked, and the resolved address
  is **pinned** into the connection — so the DNS-rebinding window does not exist.
- Loopback, RFC1918, CGNAT, link-local, documentation, benchmarking, multicast
  and reserved space are refused, in IPv4 and IPv6, including `::ffff:` and
  NAT64 embeddings.
- Every redirect hop is revalidated from scratch.
- Query strings and credentials are stripped from every log line and error body.

`DDL_ALLOW_PRIVATE_HOSTS=1` relaxes *address classes* for self-hosting and for
the test suites. It never relaxes the name rules, and it logs a warning at
startup.

## Performance

Real numbers from the development machine, with the commands that produce them,
are in [docs/PERFORMANCE.md](docs/PERFORMANCE.md). Everything there is labelled
MEASURED or EXPECTED, and nothing is extrapolated past what was run.

Design choices that keep it fast:

- **Native playback.** No MediaSource, no `fetch`, no service worker — playback
  starts sooner and uses less CPU and memory. Custom headers would have forced
  one of them; query parameters cost nothing.
- **The probe is 1 KiB, not 1 byte.** `bytes=0-0` is cheaper but carries no
  container signature, so "is this actually MP4?" would be a guess.
- **No speculative over-fetch.** A window continues only while the client still
  wants more. Measured over-fetch under a read-then-abandon workload was
  7–16 %, which is exactly the partial final window.
- **The cache never makes a byte range.** Caching a length and trusting it later
  is how a proxy serves a corrupt seek.

## Testing

263 Rust tests and 126 browser tests.

```bash
npm run test:rust          # 139 unit
npm run test:integration   # 37 end-to-end
npm run test:chaos         # 17 fault-injection scenarios
npm run test:leaks         # 8 resource-leak and long-run scenarios
npm run bench              # criterion micro-benchmarks

cargo test --manifest-path backend/Cargo.toml --test extensionless   # 49 DDL identification
cargo test --manifest-path backend/Cargo.toml --test ddl_live        # 13 live-server DDL flows

npm run test:browser                                     # Playwright, Chromium
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\with-browser.ps1 -Project firefox
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\with-load.ps1 -Levels 1,10,50,100,200,256
```

The browser tests drive the real built page against a real origin and assert
that playback actually starts, that `currentTime` advances, that a seek lands
where it was asked to, and that failures produce sentences rather than a black
rectangle.

`tests/support/fixture-origin.mjs` is a dependency-free HTTP origin that serves
the fixtures and can be told to misbehave:

| Group | Faults |
|---|---|
| Transport | `slow-flaky` `truncate` `range-less` `html` `no-length` `rate-limit` `500` `502` `503` `504` `429` `disconnect` `redirect-loop` `ssrf-redirect` |
| Extensionless DDL | `octet` `no-ct` `leading-free` `attachment` `truncated-head` `slow-body` |
| Redirects | `cdnr` `ext-in-redirect` |
| Not media | `html-as-binary` `expired-200` `json-error` `lies-mp4` `empty` `gzip` `hls` |

The DDL modes also accept `&head=full\|free\|truncated` and `&name=…` to choose
the body shape and the `Content-Disposition` filename.

## How this compares

| | Browser `<video src>` | nginx `proxy_pass` | MediaSource in JS | **DDL Player** |
|---|---|---|---|---|
| Seeking without CORS | ✗ | ✓ | ✓ | ✓ |
| Extensionless DDL links | ✗ | ✓ | depends | ✓ |
| SSRF protection | n/a | ✗ | ✗ | ✓ |
| DNS rebinding defence | n/a | ✗ | ✗ | ✓ |
| Names the real failure cause | ✗ | ✗ | ✗ | ✓ |
| Transcoding / remuxing | ✗ | ✗ | ✗ | ✗ (by design) |
| Runtime memory for a 20 GB file | n/a | n/a | grows with buffer | flat |
| Extra infrastructure | none | nginx | none | one binary |

DDL Player does not transcode. MPEG-TS, FLV, AVI and Matroska are valid video
that no browser will play from a bare `<video src>`, so they are **refused with
a reason** rather than half-played; the seam for a remux is one function in the
engine.

HLS manifests are refused too, deliberately: the segment requests that follow
would go straight to the origin, outside the proxy, which breaks signed and
temporary links outright.

## FAQ

**Does it work with links that have no file extension?**
Yes. That is the primary design goal. `/download/37334`, `/get?id=12345` and
`/file?token=…` are identified from response bytes and headers, never from the
URL. See [Direct download links without file extensions](#direct-download-links-without-file-extensions).

**Does it transcode or remux?**
No. FFmpeg in the request path would mean CPU cost and latency on every
request. If a browser can play the file directly, it does. Non-playable
containers are refused with a reason that says what to do.

**Does it download the whole file first?**
No. Playback starts on the first frame. Origin reads are windowed (4 MiB) and
continued only while the client is still watching.

**Can I use it with a private or signed URL?**
Yes. Tokens stay in the URL and are never written to disk or echoed into the
page. The interface keeps the origin's query string out of anything the browser
can read.

**Is it safe to expose to the internet?**
It is built as hostile input, with SSRF defence, a 256-stream ceiling and full
redirect revalidation. It has **not** been through an external security audit,
so treat it accordingly.

**Can it play HLS (`.m3u8`) streams?**
Not yet — a manifest is refused with an explanation, because its segments would
be fetched from the origin directly, outside the proxy. This is a deliberate
gap rather than a silent failure.

**Why does it refuse loopback by default?**
Because the SSRF guard is the point: without it, the proxy would fetch
`http://169.254.169.254/` for anyone who asked. Set `DDL_ALLOW_PRIVATE_HOSTS=1`
for self-hosting and local testing.

**Do I need Node at runtime?**
No. `npm run build` produces a static site; the release binary serves it. Node
is only needed to build the front end.

## Contributing

Issues and pull requests are welcome. Please run the checks first:

```bash
npm run check    # typecheck, cargo fmt, clippy -D warnings, full test suite
```

If you are changing how a source is identified, `tests/integration/extensionless.rs`
and `tests/integration/ddl_live.rs` are the tests to run — and if you can make
them fail first, that is the best possible contribution.

## License

MIT — see [LICENSE](LICENSE).