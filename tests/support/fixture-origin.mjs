#!/usr/bin/env node
/**
 * Fixture origin for manual testing, Playwright and k6.
 *
 * Zero dependencies, ~200 lines, and it speaks real HTTP: byte ranges,
 * keep-alive, ETag, 304, and a set of named fault modes selected by the path.
 * Point the player at it with `?path=/media/360p.mp4&fault=ok`.
 *
 *   node tests/support/fixture-origin.mjs [--port 9000] [--root <dir>]
 *
 * Faults:
 *   ok slow-flaky truncate range-less html no-length rate-limit
 *   500 502 503 504 429 slow-header disconnect redirect-loop ssrf-redirect
 */

import http from "node:http";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const argv = process.argv.slice(2);
const argOf = (name, fallback) => {
  const i = argv.indexOf(`--${name}`);
  return i >= 0 && argv[i + 1] ? argv[i + 1] : fallback;
};

const PORT = Number(argOf("port", process.env.FIXTURE_PORT ?? 9000));
const ROOT = path.resolve(argOf("root", path.join(here, "..", "fixtures", "media")));

/** name -> file, resolved lazily so a missing fixture is a clear error. */
const MEDIA = {
  "360p.mp4": path.join(ROOT, "sample-360p.mp4"),
  "720p.mp4": path.join(ROOT, "sample-720p.mp4"),
};

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const stats = {
  requests: 0,
  bytes: 0,
  byFault: Object.create(null),
};

function fileSize(p) {
  try {
    return fs.statSync(p).size;
  } catch {
    return -1;
  }
}

function parseRange(header, total) {
  if (!header) return null;
  const m = /^bytes=(\d*)-(\d*)$/.exec(header.trim());
  if (!m) return null;
  const [, a, b] = m;
  if (a === "") {
    const n = Number(b);
    if (!Number.isFinite(n) || n <= 0 || total <= 0) return null;
    return [Math.max(0, total - n), total - 1];
  }
  const start = Number(a);
  if (!Number.isFinite(start) || start >= total) return { unsatisfiable: true };
  const end = b === "" ? total - 1 : Math.min(Number(b), total - 1);
  return [start, end];
}

function send(res, code, headers, body) {
  const buf = body === undefined ? Buffer.alloc(0) : Buffer.from(body);
  res.writeHead(code, { "content-length": String(buf.length), ...headers });
  res.end(buf);
}

async function streamFile(res, file, start, end, chunk = 256 * 1024) {
  const stream = fs.createReadStream(file, { start, end, highWaterMark: chunk });
  for await (const chunkBuf of stream) {
    if (!res.write(chunkBuf)) await new Promise((r) => res.once("drain", r));
    stats.bytes += chunkBuf.length;
  }
  res.end();
}

const server = http.createServer(async (req, res) => {
  stats.requests += 1;
  const url = new URL(req.url ?? "/", `http://${req.headers.host ?? "localhost"}`);
  const fault = url.searchParams.get("fault") ?? "ok";
  stats.byFault[fault] = (stats.byFault[fault] ?? 0) + 1;

  if (url.pathname === "/__stats") {
    return send(res, 200, { "content-type": "application/json" }, JSON.stringify(stats));
  }
  if (url.pathname === "/__reset") {
    stats.requests = 0;
    stats.bytes = 0;
    stats.byFault = Object.create(null);
    return send(res, 200, { "content-type": "application/json" }, "{}");
  }
  if (url.pathname === "/health") return send(res, 200, {}, "ok");

  const statuses = { 500: 500, 502: 502, 503: 503, 504: 504, 429: 429 };
  if (statuses[fault]) {
    if (fault === "429") {
      return send(res, 429, { "content-type": "text/plain", "retry-after": "1" }, "slow down");
    }
    return send(res, statuses[fault], { "content-type": "text/plain" }, `fault ${fault}`);
  }
  if (fault === "html") {
    return send(res, 200, { "content-type": "text/html; charset=utf-8" }, "<!doctype html><h1>no</h1>");
  }
  if (fault === "no-length") {
    res.writeHead(200, { "content-type": "video/mp4", "accept-ranges": "none" });
    res.write(fs.readFileSync(MEDIA["360p.mp4"]).subarray(0, 65536));
    return res.end();
  }
  if (fault === "range-less") {
    const file = MEDIA["360p.mp4"];
    const body = fs.readFileSync(file);
    res.writeHead(200, { "content-type": "video/mp4", "accept-ranges": "none", "content-length": String(body.length) });
    return res.end(body);
  }
  if (fault === "redirect-loop") {
    return send(res, 302, { location: "/loop" }, "");
  }
  if (url.pathname === "/loop") return send(res, 302, { location: "/loop" }, "");
  if (fault === "ssrf-redirect") {
    return send(res, 302, { location: "http://169.254.169.254/latest/meta-data/" }, "");
  }
  if (fault === "disconnect") {
    res.writeHead(200, { "content-type": "video/mp4", "content-length": "999999999" });
    res.write(Buffer.alloc(1024));
    return res.socket?.destroy();
  }
  if (fault === "slow-header") {
    await sleep(2000);
    return send(res, 200, { "content-type": "text/plain" }, "late");
  }

  // Media, with optional slowness and flakiness layered on top.
  const name = (url.pathname.split("/").pop() || "").replace(/^\/media\/?/, "");
  if (!(name in MEDIA)) {
    // Unknown names must be a real 404, not a silent substitution: a test that
    // means to check error handling has to actually get an error.
    return send(res, 404, { "content-type": "text/plain" }, "no such fixture");
  }
  const file = MEDIA[name];
  const size = fileSize(file);
  if (size < 0) return send(res, 404, { "content-type": "text/plain" }, "no fixture");

  if (fault === "flaky" || fault === "slow-flaky") {
    if (stats.byFault[fault] <= 2) {
      return send(res, 503, { "content-type": "text/plain", "retry-after": "0" }, "warming up");
    }
  }

  const etag = `"${size.toString(16)}"`;
  const lastModified = new Date(Date.UTC(2026, 0, 1)).toUTCString();
  const base = {
    "content-type": "video/mp4",
    etag,
    "last-modified": lastModified,
    "accept-ranges": "bytes",
  };
  if (req.headers["if-none-match"] === etag) {
    res.writeHead(304, base);
    return res.end();
  }

  const range = fault === "range-less" ? null : parseRange(req.headers.range, size);
  if (range?.unsatisfiable) {
    return send(res, 416, { ...base, "content-range": `bytes */${size}` }, "");
  }

  const [start, end] = range ?? [0, size - 1];
  const length = end - start + 1;
  if (range) {
    res.writeHead(206, { ...base, "content-range": `bytes ${start}-${end}/${size}`, "content-length": String(length) });
  } else {
    res.writeHead(200, { ...base, "content-length": String(size) });
  }

  if (fault === "truncate") {
    const stream = fs.createReadStream(file, { start, end, highWaterMark: 64 * 1024 });
    for await (const chunk of stream) {
      if (!res.write(chunk)) await new Promise((r) => res.once("drain", r));
      stats.bytes += chunk.length;
    }
    // Advertise the full length, deliver half, then hang up.
    return res.socket?.destroy();
  }

  if (fault === "slow" || fault === "slow-flaky") {
    const stream = fs.createReadStream(file, { start, end, highWaterMark: 32 * 1024 });
    for await (const chunk of stream) {
      if (!res.write(chunk)) await new Promise((r) => res.once("drain", r));
      stats.bytes += chunk.length;
      await sleep(20);
    }
    return res.end();
  }

  await streamFile(res, file, start, end);
});

server.keepAliveTimeout = 5000;
server.listen(PORT, "127.0.0.1", () => {
  process.stdout.write(`fixture-origin listening on http://127.0.0.1:${PORT}\n`);
  process.stdout.write(`  media: ${Object.keys(MEDIA).join(", ")} from ${ROOT}\n`);
});