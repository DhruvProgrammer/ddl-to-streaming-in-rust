/**
 * k6 load test.
 *
 *   k6 run tests/load/k6.js
 *   K6_RATE=200 K6_VUS=200 k6 run tests/load/k6.js
 *
 * One iteration is one player session: probe, open a stream, seek three times,
 * and make one deliberately bad request. Every read is bounded by a `Range`
 * header so an iteration finishes quickly and the scenario stays an honest
 * concurrency test rather than a measurement of how fast k6 can buffer 5 MB.
 *
 * Needs the stack running:
 *   powershell -NoProfile -ExecutionPolicy Bypass -File scripts\hold-stack.ps1
 */

import http from "k6/http";
import { check } from "k6";
import { Counter, Rate, Trend } from "k6/metrics";

const SERVER = __ENV.DDL_SERVER || "http://127.0.0.1:8787";
const ORIGIN = __ENV.DDL_ORIGIN || "http://127.0.0.1:9000";
const FILE = __ENV.DDL_FILE || "720p.mp4";
const ORIGIN_URL = `${ORIGIN}/media/${FILE}`;
const STREAM_URL = `${SERVER}/api/stream?url=${encodeURIComponent(ORIGIN_URL)}`;

const RATE = Number(__ENV.K6_RATE || 25); // sessions per second
const VUS = Number(__ENV.K6_VUS || 100);
const DURATION = __ENV.K6_DURATION || "60s";

const startup = new Trend("startup_ms", true);
const ttfb = new Trend("ttfb_ms", true);
const server5xx = new Rate("server_5xx");
const bytesRead = new Counter("bytes_read");
const superseded = new Counter("superseded_requests");
const ssrfRefused = new Rate("ssrf_refused");
const streamOk = new Rate("stream_ok");

export const options = {
  scenarios: {
    players: {
      executor: "constant-arrival-rate",
      rate: RATE,
      timeUnit: "1s",
      duration: DURATION,
      preAllocatedVUs: Math.max(1, Math.floor(VUS / 2)),
      maxVUs: VUS,
    },
  },
  thresholds: {
    // The product's promises. If the server regresses past these, k6 fails.
    "server_5xx": ["rate<0.001"],
    ssrf_refused: ["rate==1"],
    stream_ok: ["rate>0.99"],
    "startup_ms": ["p(95)<750"],
    "ttfb_ms": ["p(95)<1500"],
  },
  discardResponseBodies: false,
  noConnectionReuse: false,
};

function drain(res) {
  const n = res.body ? res.body.length : 0;
  bytesRead.add(n);
  return n;
}

export default function () {
  const session = `k6-${__VU}-${__ITER}`;

  // 1. What are we about to play?
  const t0 = Date.now();
  const probe = http.post(
    `${SERVER}/api/probe`,
    JSON.stringify({ url: ORIGIN_URL }),
    { headers: { "content-type": "application/json" }, tags: { endpoint: "probe" } },
  );
  startup.add(Date.now() - t0);
  const ok = check(probe, {
    "probe is 200": (r) => r.status === 200,
    "probe is streamable": (r) => r.status === 200 && r.json("streamable") === true,
    "probe found a length": (r) => r.status === 200 && r.json("content_length") > 0,
  });
  if (!ok) {
    server5xx.add(probe.status >= 500);
    return;
  }
  const total = probe.json("content_length");

  // 2. Play the first quarter second of media.
  const play = http.get(STREAM_URL, {
    headers: { Range: "bytes=0-262143" },
    tags: { endpoint: "stream" },
  });
  ttfb.add(play.timings.waiting);
  if (play.status === 206) {
    drain(play);
    streamOk.add(true);
  } else {
    streamOk.add(false);
  }
  server5xx.add(play.status >= 500);

  // 3. Seek. Each seek is a fresh byte-range request with a new generation.
  for (const fraction of [0.25, 0.6, 0.85]) {
    const offset = Math.floor(total * fraction);
    const seek = http.get(STREAM_URL, {
      headers: {
        Range: `bytes=${offset}-${offset + 131071}`,
        "x-ddl-session": session,
        "x-ddl-generation": String(Math.round(fraction * 100)),
      },
      tags: { endpoint: "seek" },
    });
    if (seek.status === 206) drain(seek);
    // 409 means a newer generation already won: correct behaviour, not a fault.
    if (seek.status === 409) superseded.add(1);
    server5xx.add(seek.status >= 500);
  }

  // 4. One deliberately bad request: a name that is internal under *every*
  //    policy. (Loopback addresses are allowed when the private-host escape
  //    hatch is on, but internal names never are.) A 400 is the correct
  //    answer, declared expected so it never counts as a failure.
  const bad = http.get(
    `${SERVER}/api/stream?url=${encodeURIComponent("http://localhost:9/x.mp4")}`,
    { tags: { endpoint: "ssrf" }, responseCallback: http.expectedStatuses(400) },
  );
  check(bad, {
    "internal name refused": (r) => r.status === 400 && r.json("code") === "INVALID_URL",
  });  ssrfRefused.add(bad.status === 400 && bad.json("code") === "INVALID_URL");
}

function fmtMs(v) {
  if (typeof v !== "number") return "-";
  return v >= 1000 ? `${(v / 1000).toFixed(2)}s` : `${v.toFixed(1)}ms`;
}

export function handleSummary(data) {
  const m = data.metrics;
  const out = ["\n===== k6 summary ====="];
  const val = (name, key, suffix = "") => {
    const metric = m[name];
    const v = metric ? metric.values[key] : undefined;
    return `${name}${suffix}=${typeof v === "number" ? v.toFixed(3) : "-"}`;
  };
  out.push(`  requests            ${m.http_reqs ? m.http_reqs.values.count : 0}`);
  out.push(`  request failure     ${m.http_req_failed ? (m.http_req_failed.values.rate * 100).toFixed(2) + "%" : "-"}`);
  out.push(`  server 5xx rate     ${val("server_5xx", "rate")}`);
  out.push(`  stream ok rate      ${val("stream_ok", "rate")}`);
  out.push(`  ssrf refused rate   ${val("ssrf_refused", "rate")}`);
  out.push(`  startup_ms          ${val("startup_ms", "median", " (median)")}  ${fmtMs(m.startup_ms?.values["p(95)"])} p95  ${fmtMs(m.startup_ms?.values["p(99)"])} p99`);
  out.push(`  ttfb_ms             ${fmtMs(m.ttfb_ms?.values.median)} median  ${fmtMs(m.ttfb_ms?.values["p(95)"])} p95  ${fmtMs(m.ttfb_ms?.values["p(99)"])} p99`);
  out.push(`  bytes read          ${(m.bytes_read ? m.bytes_read.values.count : 0).toLocaleString()}`);
  out.push(`  superseded (409)    ${m.superseded_requests ? m.superseded_requests.values.count : 0}`);
  const checks = m.checks;
  out.push(
    `  checks              ${checks ? `${checks.values.passes}/${checks.values.passes + checks.values.fails} (${((checks.values.passes / Math.max(1, checks.values.passes + checks.values.fails)) * 100).toFixed(1)}%)` : "-"}`,
  );
  return { stdout: out.join("\n") + "\n" };
}