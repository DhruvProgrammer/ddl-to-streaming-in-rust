#!/usr/bin/env node
/**
 * Load and latency harness. Dependency-free, so it measures the server and not
 * a client library.
 *
 *   node tests/load/harness.mjs --levels 1,10,50,100,250 --seconds 6
 *
 * Each level runs `N` concurrent sessions that behave like a player:
 *   probe -> open a stream -> read a little -> seek -> read -> abandon.
 * Abandoning mid-stream is the interesting case: it is what proves the proxy
 * releases connections and permits when a client goes away.
 *
 * The wrapper script (scripts/with-load.ps1) samples resident memory per level,
 * because it is the process that can see the server's working set.
 */

import http from "node:http";

const argv = process.argv.slice(2);
const opt = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : fallback;
};

const SERVER = opt("server", process.env.DDL_SERVER ?? "http://127.0.0.1:8787");
const ORIGIN = process.env.DDL_ORIGIN ?? "http://127.0.0.1:9000";
const LEVELS = opt("levels", "1,10,50,100,250")
  .split(",")
  .map(Number);
const SECONDS = Number(opt("seconds", "5"));
const FILE = opt("file", "720p.mp4");
const LIMIT = Number(opt("limit", "1500000"));
const KEEPALIVE = !argv.includes("--no-keepalive");

const AGENT = new http.Agent({ keepAlive: KEEPALIVE, maxSockets: 8192 });

function quantile(values, q) {
  if (values.length === 0) return null;
  const s = [...values].sort((a, b) => a - b);
  const i = Math.min(s.length - 1, Math.max(0, Math.ceil(q * s.length) - 1));
  return s[i];
}

function fmt(n, unit = "ms") {
  if (n === null || n === undefined || Number.isNaN(n)) return "-";
  if (unit === "mbps") return `${(n / (1024 * 1024)).toFixed(1)} MB/s`;
  if (n >= 1000) return `${(n / 1000).toFixed(2)} s`;
  if (n >= 10) return `${n.toFixed(0)} ms`;
  if (n >= 1) return `${n.toFixed(1)} ms`;
  return `${n.toFixed(2)} ms`;
}

function postJson(path, body) {
  return new Promise((resolve, reject) => {
    const u = new URL(path, SERVER);
    const payload = JSON.stringify(body);
    const req = http.request(
      {
        agent: AGENT,
        method: "POST",
        hostname: u.hostname,
        port: u.port,
        path: u.pathname + u.search,
        headers: {
          "content-type": "application/json",
          "content-length": Buffer.byteLength(payload),
        },
      },
      (res) => {
        const chunks = [];
        res.on("data", (c) => chunks.push(c));
        res.on("end", () => {
          try {
            resolve({
              status: res.statusCode,
              body: JSON.parse(Buffer.concat(chunks).toString()),
            });
          } catch (e) {
            reject(e);
          }
        });
      },
    );
    req.on("error", reject);
    req.end(payload);
  });
}

function getJson(path) {
  return new Promise((resolve, reject) => {
    const u = new URL(path, SERVER);
    http
      .get(
        { agent: AGENT, hostname: u.hostname, port: u.port, path: u.pathname + u.search },
        (res) => {
          const chunks = [];
          res.on("data", (c) => chunks.push(c));
          res.on("end", () => {
            try {
              resolve(JSON.parse(Buffer.concat(chunks).toString()));
            } catch (e) {
              reject(e);
            }
          });
        },
      )
      .on("error", reject);
  });
}

/** Stream a range, then optionally abandon it the way a player does on seek. */
function streamRange(url, range, limit) {
  return new Promise((resolve) => {
    const u = new URL(url, SERVER);
    const started = process.hrtime.bigint();
    let settled = false;
    const done = (v) => {
      if (!settled) {
        settled = true;
        resolve(v);
      }
    };
    const req = http.request(
      {
        agent: AGENT,
        hostname: u.hostname,
        port: u.port,
        path: u.pathname + u.search,
        headers: range ? { range } : {},
      },
      (res) => {
        const headersMs = Number(process.hrtime.bigint() - started) / 1e6;
        let bytes = 0;
        let firstByteMs = null;
        res.on("data", (chunk) => {
          if (firstByteMs === null) {
            firstByteMs = Number(process.hrtime.bigint() - started) / 1e6;
          }
          bytes += chunk.length;
          if (limit && bytes >= limit) req.destroy();
        });
        res.on("end", () =>
          done({ status: res.statusCode, bytes, headersMs, firstByteMs, clean: true }),
        );
        res.on("close", () =>
          done({ status: res.statusCode, bytes, headersMs, firstByteMs, clean: false }),
        );
        res.on("error", () =>
          done({ status: res.statusCode, bytes, headersMs, firstByteMs, clean: false }),
        );
      },
    );
    req.on("error", (e) => done({ status: 0, bytes: 0, error: e.message, clean: false }));
    req.end();
  });
}

const FILE_SIZE = 5_000_000;

async function session(index, deadline) {
  const out = { probe: [], ttfb: [], firstByte: [], bytes: 0, errors: 0, requests: 0 };
  const origin = `${ORIGIN}/media/${FILE}`;
  const streamUrl = `/api/stream?url=${encodeURIComponent(origin)}`;

  while (Date.now() < deadline) {
    const t0 = process.hrtime.bigint();
    const probe = await postJson("/api/probe", { url: origin }).catch(() => ({ status: 0 }));
    const probeMs = Number(process.hrtime.bigint() - t0) / 1e6;
    if (probe.status === 200) out.probe.push(probeMs);
    else out.errors += 1;

    // Each iteration seeks somewhere new, which is the access pattern that
    // matters: the player abandons one request and opens a new range.
    const offset = (index * 7919 + out.requests * 262144) % FILE_SIZE;
    const r = await streamRange(streamUrl, `bytes=${offset}-`, LIMIT);
    if (r.status >= 200 && r.status < 300) {
      out.bytes += r.bytes;
      if (typeof r.headersMs === "number") out.ttfb.push(r.headersMs);
      if (typeof r.firstByteMs === "number") out.firstByte.push(r.firstByteMs);
    } else {
      out.errors += 1;
    }
    out.requests += 1;
    await new Promise((r) => setTimeout(r, 5 + Math.random() * 20));
  }
  return out;
}

async function runLevel(n) {
  const before = await getJson("/api/stats");
  const deadline = Date.now() + SECONDS * 1000;
  const t0 = process.hrtime.bigint();

  const results = await Promise.all(
    Array.from({ length: n }, (_, i) => session(i, deadline).catch(() => null)),
  );
  const wall = Number(process.hrtime.bigint() - t0) / 1e9;
  const after = await getJson("/api/stats");

  const good = results.filter(Boolean);
  const agg = (key) => good.flatMap((r) => r[key]);
  const probe = agg("probe");
  const ttfb = agg("ttfb");
  const firstByte = agg("firstByte");
  const bytes = good.reduce((a, r) => a + r.bytes, 0);
  const errors = good.reduce((a, r) => a + r.errors, 0);
  const sessions = probe.length + errors;

  const dClient = (after.bytes_to_client ?? 0) - (before.bytes_to_client ?? 0);
  const dOrigin = (after.bytes_from_origin ?? 0) - (before.bytes_from_origin ?? 0);

  return {
    streams: n,
    sessions,
    wall: Number(wall.toFixed(3)),
    bytes,
    throughput: bytes / wall,
    originOverhead: dClient > 0 ? (dOrigin - dClient) / dClient : 0,
    errors,
    errorRate: sessions > 0 ? errors / sessions : 0,
    probe: { p50: quantile(probe, 0.5), p95: quantile(probe, 0.95), p99: quantile(probe, 0.99) },
    ttfb: { p50: quantile(ttfb, 0.5), p95: quantile(ttfb, 0.95), p99: quantile(ttfb, 0.99) },
    firstByte: {
      p50: quantile(firstByte, 0.5),
      p95: quantile(firstByte, 0.95),
      p99: quantile(firstByte, 0.99),
    },
    activeAfter: after.active_streams,
    sessionsAfter: after.registry?.sessions ?? 0,
    server: {
      http_p50_us: after.http_latency?.p50_us ?? null,
      http_p95_us: after.http_latency?.p95_us ?? null,
      http_p99_us: after.http_latency?.p99_us ?? null,
      seek_p50_us: after.seek?.p50_us ?? null,
      seek_p95_us: after.seek?.p95_us ?? null,
      seek_p99_us: after.seek?.p99_us ?? null,
      startup_p50_us: after.startup?.p50_us ?? null,
      cache_hit_rate: after.cache?.hit_rate ?? null,
      errors: after.errors ?? {},
      rejected: after.registry?.rejected ?? 0,
    },
  };
}

async function main() {
  const rows = [];
  process.stdout.write(
    `target ${SERVER}  origin ${ORIGIN}  file ${FILE}  ${SECONDS}s/level  read-then-abandon ${LIMIT}B\n\n`,
  );
  process.stdout.write(
    "streams  sessions  throughput      err%   probe p50/p95/p99        ttfb p50/p95/p99        first-byte p50  leaked\n",
  );
  process.stdout.write("-".repeat(118) + "\n");

  for (const n of LEVELS) {
    const r = await runLevel(n);
    rows.push(r);
    process.stdout.write(
      [
        String(r.streams).padEnd(8),
        String(r.sessions).padEnd(10),
        fmt(r.throughput, "mbps").padEnd(15),
        (r.errorRate * 100).toFixed(2).padEnd(7),
        `${fmt(r.probe.p50)}/${fmt(r.probe.p95)}/${fmt(r.probe.p99)}`.padEnd(24),
        `${fmt(r.ttfb.p50)}/${fmt(r.ttfb.p95)}/${fmt(r.ttfb.p99)}`.padEnd(24),
        fmt(r.firstByte.p50).padEnd(15),
        `${r.activeAfter}/${r.sessionsAfter}`,
      ].join(""),
    );
    process.stdout.write("\n");

    // Everything must return to zero between levels or the numbers are fiction.
    for (let i = 0; i < 120; i += 1) {
      const s = await getJson("/api/stats");
      if (s.active_streams === 0 && (s.registry?.sessions ?? 0) === 0) break;
      await new Promise((r) => setTimeout(r, 50));
    }
    await new Promise((r) => setTimeout(r, 250));
  }

  process.stdout.write("\n" + JSON.stringify(rows, null, 2) + "\n");
  if (argv.includes("--json-out")) {
    const { writeFileSync } = await import("node:fs");
    writeFileSync("docs/load-results.json", JSON.stringify(rows, null, 2));
    process.stdout.write("wrote docs/load-results.json\n");
  }
  AGENT.destroy();
}

void main();
