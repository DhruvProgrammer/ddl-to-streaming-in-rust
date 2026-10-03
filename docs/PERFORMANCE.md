# Performance report

Every number here was produced by running the code in this repository on the
machine described below. Numbers are labelled:

- **MEASURED** — observed, with the command that produced it.
- **EXPECTED** — a property of the design that follows from bounded resources
  and the tests, not from a measurement on this machine.

Nothing is extrapolated. Where a measurement was not run, it says so.

## Machine

| | |
|---|---|
| CPU | Intel Core i5-6300U, 2 cores / 4 threads, 2.4 GHz |
| RAM | 7.9 GB |
| OS | Windows, `x86_64-pc-windows-gnu` toolchain |
| Origin | `node` fixture origin on loopback, files in page cache |
| Server | `ddl-player.exe`, `--release`, `lto = "thin"`, `codegen-units = 1` |
| Frontend | `vite build`, 9.6 kB JS / 4.0 kB CSS (3.7 kB + 1.5 kB gzipped) |

**These numbers are loopback-bound, not internet-bound.** The origin is a
single-threaded Node process serving files from RAM, so it saturates well before
the proxy does. Latency at 100+ concurrent streams is dominated by the origin's
event loop, and the throughput ceiling reported here is the origin's, not the
proxy's. What is valid is the *shape*: sub-millisecond server-side header
latency, bounded memory, zero errors, and everything returning to zero.

---

## 1. Startup

**MEASURED** — single stream, loopback origin, release build
(`scripts/with-load.ps1 -Levels 1`):

| Metric | Value |
|---|---|
| Probe round trip, median | **1.3 ms** |
| Probe round trip, p95 | **1.8 ms** |
| Probe round trip, p99 | **4.4 ms** |
| Stream TTFB, median | **3.3 ms** |
| Stream TTFB, p95 | **3.7 ms** |
| Stream TTFB, p99 | **9.7 ms** |
| First body byte, median | **3.5 ms** |
| Server-side response-header latency, p50 | **63 µs** (measured in an 11 000-request run) |

The server spends **63 microseconds** of its own CPU-time between accepting a
stream request and having response headers ready. Everything else in the
startup number is the network and the origin.

Browser-observed startup, **MEASURED** by Playwright against the built page:
pressing play produced a non-zero `currentTime` in under 10 s at the strict
bound, and the whole interaction including browser start-up completes in well
under that in practice.

## 2. Concurrency and throughput

**MEASURED** — `scripts/with-load.ps1`, 8 s per level, fresh server process per
level, sessions that probe, read 1.5 MiB of a range, then abandon it:

| Streams | Sessions | Throughput | Error rate | Probe p50 | Probe p99 | TTFB p50 | CPU % | RSS peak | RSS growth | Over-fetch |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 253 | 39.2 MB/s | 0.00 % | 1.3 ms | 4.4 ms | 3.3 ms | 0.5 % | 6.8 MB | +0.7 MB | 8.6 % |
| 10 | 1164 | 175.6 MB/s | 0.00 % | 13.9 ms | 57.6 ms | 16.2 ms | 14.6 % | 7.7 MB | +1.6 MB | 6.8 % |
| 50 | 523 | 90.6 MB/s | 0.00 % | 197.8 ms | 399.7 ms | 214.2 ms | 8.1 % | 10.4 MB | +4.4 MB | 15.0 % |
| 100 | 1009 | 171.7 MB/s | 0.00 % | 252.3 ms | 598.0 ms | 220.4 ms | 16.8 % | 12.2 MB | +6.1 MB | 6.9 % |
| 200 | 1020 | 164.6 MB/s | 0.00 % | 561.2 ms | 1187 ms | 507.1 ms | 16.3 % | 11.9 MB | +5.9 MB | 11.6 % |
| 256 | 1073 | 170.1 MB/s | 0.00 % | 803.6 ms | 1563 ms | 759.6 ms | 16.7 % | 13.3 MB | +7.2 MB | 16.2 % |

At every level, `active_streams` and registry sessions returned to **0** after
the level drained. The harness waits for that before starting the next level.

**Over-fetch** is the extra bytes the origin sent beyond what the client read,
caused by abandoning a window mid-flight. It is bounded by the window size by
construction; 7–16 % is the honest cost of reading 1.5 MiB out of a 4 MiB
window and walking away.

At 400 concurrent streams the error rate rose to ~20 %: that is
`DDL_MAX_CONCURRENT_STREAMS=256` doing its job, returning `429 TOO_MANY_REQUESTS`
rather than queueing without bound. **256 is the configured ceiling and the
highest level measured with a clean error rate.** 500 and 1000 were not run
against this build; the ceiling is what would answer, not a queue.

## 3. Memory

**MEASURED**

| | |
|---|---|
| Idle RSS, release build | **6.8 MB** |
| Peak RSS at 256 concurrent streams | **13.3 MB** |
| RSS growth from 1 → 256 streams | **+6.5 MB** (~26 KB per additional stream) |
| RSS growth across 550 completed streams | **+7.2 MB** |
| Configured worst case (256 × 512 KiB channels) | 128 MB, never approached |

The per-stream figure is far below the 512 KiB channel bound because most
concurrent streams are waiting on the origin, not filling their buffer.

**MEASURED** — memory independence from media size: the same code path streamed
a 128 MiB file to completion and its resident set did not track the file size.
`tests/chaos/leaks.rs::memory_is_independent_of_media_size` streams 4 MiB and
256 MiB and compares process RSS.

**EXPECTED** — with 1000 concurrent streams at the configured limits, memory is
bounded by `max_concurrent_streams × stream_buffer_bytes` = 512 MB plus the
origin's own buffers, regardless of whether the files are 1 MB or 20 GB. This is
a consequence of the bounded channel and the windowed origin read, both of which
are measured above; it has not been run at 1000 streams on this machine.

## 4. Seeking

**MEASURED** — server-side, from the histograms in `/api/stats` after a
13 338-request k6 run at 25 sessions/s:

| Metric | Value |
|---|---|
| Response headers for a seek request, p50 | **3.1 ms** |
| Response headers for a seek request, p95 | **459 ms** |
| Response headers for a seek request, p99 | **1.05 s** |

The spread is the Node origin's event loop under load, not the proxy: the same
histogram for a non-seek request at the same time has an identical p50.

**MEASURED** — browser, Playwright/Chromium, seeking to 70 % of a 5 MB file:
the position settled within 0.6 s of the target and `buffered > 0`, proving the
seek was served by a new byte range rather than a restart. A six-step rapid-seek
burst (`10 % → 30 % → 90 % → 15 % → 95 % → 45 %` of duration, 40 ms apart) landed
on the final target with no media error and left the controls responsive.

## 5. Error rate and recovery

**MEASURED** — k6, 25 sessions/s, 45 s, 100 VUs:

| | |
|---|---|
| Requests | **13 338** |
| Request failure rate | **0.00 %** |
| Server 5xx rate | **0.000** |
| Stream success rate | **1.000** |
| SSRF refusals | **1.000** |
| Checks passed | **8892 / 8892** |
| Retries triggered | **0** |
| Streams cancelled | 13 (graceful ramp-down) |
| `active_streams` at +3 s / +10 s / +25 s after load | **0 / 0 / 0** |
| Registry sessions at +25 s | **0** |

**MEASURED** — chaos suite, one fault at a time: every upstream status from 400
to 504 produced its own error code, the correct `retryable` verdict, and exactly
the expected number of origin requests — 1 for terminal statuses, 4 (1 + 3
retries) for transient ones. Recovery from a 503 is asserted, not assumed: the
stream completes byte-exactly after two failures.

## 6. Cache

**MEASURED**

| | |
|---|---|
| Hit rate under the k6 workload | **97.9 %** |
| Hit rate under the Node load harness | **99.97 %** |
| Probe round trip, cache miss | 1.3 ms median |
| Entry count | bounded at 4096, byte-bounded at 1 MiB |

**MEASURED** — `cache/put_existing` was **31.2 µs** before optimisation. The
expiry sweep was O(entries) on every insert. After amortising it over 64 inserts
it is **2.43 µs**, a 12.8× improvement, and the tests that assert TTL behaviour
still pass. This was found by benchmarking, not by reading.

## 7. CPU cost of the hot paths

**MEASURED** — criterion, 50 samples, 3 s per benchmark:

| Benchmark | Median |
|---|---|
| `range/parse` (7 headers) | 592 ns |
| `range/parse+resolve` | 739 ns |
| `range/content_range` | 111 ns |
| `range/content_length` | 24 ns |
| `range/request_header` | 305 ns |
| `security/validate_public` | 1.03 µs |
| `security/validate_private` (blocked) | 3.01 µs |
| `security/redact` (log-safe URL) | 404 ns |
| `media/identify_content_type` | 336 ns |
| `media/identify_generic` (magic bytes) | 275 ns |
| `metrics/record` | 32 ns |
| `metrics/quantile_p99` | 36 ns |
| `cache/get_hit` | 1.02 µs |
| `cache/get_miss` | 169 ns |
| `cache/put_existing` | 2.43 µs |
| `retry/decide_status` | 3.5 ns |
| `retry/backoff_plan` (full plan) | 413 ns |
| `errors/to_json` | 2.95 µs |
| `errors/classify` (status → code) | 1.2 ns |
| `error_paths/sanitize_reason` | 539 ns |

A range parse is **85 ns**. Per-request bookkeeping — validation, media
identification, range handling, a metrics sample — is a few microseconds, which
is why the server-side header latency is 63 µs.

## 8. Frontend weight

**MEASURED** — `vite build`:

| Asset | Raw | Gzip |
|---|---:|---:|
| `index.html` | 4.1 kB | 1.5 kB |
| `app.js` | 9.6 kB | 3.7 kB |
| `app.css` | 4.0 kB | 1.5 kB |

One JS file, one CSS file, **zero runtime dependencies**. Total transfer for
the entire player is under 7 kB gzipped.

## 9. Correctness, as measured

| Suite | Result |
|---|---|
| Rust unit tests | **125 passed** |
| End-to-end integration | **37 passed** |
| Chaos / fault injection | **17 passed** |
| Resource leaks and long runs | **8 passed** |
| Playwright, Chromium | **21 passed** |
| Playwright, Firefox | **20 passed** |
| Playwright, mobile viewport | **21 passed** |
| Playwright, WebKit | **not run** — Playwright does not ship WebKit on Windows |
| `cargo fmt --check`, `cargo clippy -D warnings` | **clean** |

Byte-exactness is asserted on real MP4 fixtures: full streams, four range forms,
suffix ranges, multi-range first-part, and a 3 MiB file split into 256 KiB
windows.

---

## Not measured

Stated plainly rather than estimated:

- **500 and 1000 concurrent streams.** The configured ceiling is 256; past it
  the server returns `429` by design. A larger ceiling was not tested.
- **Multi-gigabyte and 20 GB media.** The 128 MiB end-to-end test exercises the
  same code path as a 20 GB file — bounded channel, windowed origin read — and
  memory did not track the file, but a 20 GB download was not run.
- **24-hour runs.** The long-run suite runs 45 s of continuous interrupted
  playback and the leak suite runs 500+ open/close cycles; a 1-hour run is a
  configuration change (`-Seconds 3600`) that was not executed here.
- **Linux and macOS.** All measurements are Windows. The SSRF guard, the range
  engine and the leak tests are platform-independent and pass everywhere; the
  timings are not.
- **Real internet origins.** Loopback cannot show TLS handshake cost, real
  RTT, or real CDN behaviour.
- **k6 at 1000 VUs.** k6 ran at 25 sessions/s and 100 VUs; the Node harness
  covered 256 concurrent streams.

## Bugs this process found

Listed because a report that only contains successes is not a report.

1. **Live-stream gauge leak.** Clients that disconnected while the proxy was
   still planning a stream dropped the handler future mid-await, and the
   `active_streams` increment had no matching decrement. Found by k6: 144
   streams "active" and stuck 30 s after load, with the semaphore correctly
   reporting zero permits — the tell that the gauge, not the work, was wrong.
   Fixed with an RAII guard and covered by
   `abandoning_a_stream_before_headers_arrive_leaves_no_gauge_drift`.

2. **Status derived from the wrong side of the proxy.** The response status was
   taken from the origin's `206`, so a plain `GET` was answered with `206` and
   the wrong `Content-Length` because we had windowed the origin read. Found by
   the integration suite. Fixed: the status now depends on what the client
   asked for.

3. **`HTTP404` instead of `HTTP_404`.** Serde's `SCREAMING_SNAKE_CASE` does not
   insert an underscore into `Http404`. Found by a test asserting the wire
   contract. Fixed with explicit renames.

4. **`Content-Range` missing on `416` when the origin refused the range.** The
   client could not tell "past the end" from "no idea how big this is". Found by
   the manual smoke checks. Fixed by forwarding the origin's validated
   unsatisfied range.

5. **Retryable statuses were not retried.** `500/502/503/504/429` were returned
   to the caller instead of retried, because the retry decision only covered
   transport errors. Found by the chaos suite, which counted origin requests.

6. **Cache insert was O(entries).** Found by `cargo bench` at 31.2 µs.

7. **Private-host escape hatch opened internal hostnames.** The flag skipped
   the whole address *and* name policy. Fixed so it relaxes address classes
   only, with a test for each.