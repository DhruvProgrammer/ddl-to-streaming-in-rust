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
 * `--port 0` binds an ephemeral port and prints the one it got, so a test
 * harness never has to guess a free port and race another process for it.
 *
 * Faults:
 *   ok slow-flaky truncate range-less html no-length rate-limit
 *   500 502 503 504 429 slow-header disconnect redirect-loop ssrf-redirect
 *
 * Direct-download faults (they answer on *any* pathname, because a real DDL
 * link is `/download/37334`, not `/media/360p.mp4`):
 *   octet no-ct leading-free attachment truncated-head slow-body
 *   cdnr ext-in-redirect
 *
 * Not-media shapes, served with a 200 because that is what makes them hard:
 *   html-as-binary expired-200 json-error lies-mp4 empty gzip hls
 *
 * The DDL modes also accept `&head=full|free|truncated` to choose which body
 * shape they serve, and `&name=…` to choose the `Content-Disposition`
 * filename, so one mode can express "the bytes are truncated" as well as "the
 * origin lies about the filename".
 */

import http from "node:http";
import zlib from "node:zlib";
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

const LAST_MODIFIED = new Date(Date.UTC(2026, 0, 1)).toUTCString();

/** Extensionless path `cdnr` bounces to. */
const DDL_EXTENSIONLESS = "/dl/37334-8f2c1a";
/** Extension-bearing path `ext-in-redirect` bounces to. */
const DDL_EXTENSION = "/media/360p.mp4";

/**
 * Size-8 `free` box: legal ISO-BMFF padding that real muxers put ahead of
 * `ftyp`, and the reason `ftyp`-at-offset-4 detection misses real files.
 */
const LEADING_FREE = Buffer.from([0x00, 0x00, 0x00, 0x08, 0x66, 0x72, 0x65, 0x65]);

/** Body shapes the DDL modes can serve. Built once, on first use. */
let ddlBodies = null;
function bodies() {
  if (ddlBodies) return ddlBodies;
  const mp4 = fs.readFileSync(MEDIA["360p.mp4"]);
  ddlBodies = {
    full: mp4,
    free: Buffer.concat([LEADING_FREE, mp4]),
    // A box size with no type behind it: what a truncated MP4 looks like. It
    // identifies as nothing, which is the only way to reach the header-only
    // evidence paths (Content-Disposition, then the final URL's extension).
    truncated: Buffer.from([0x00, 0x00, 0x00, 0x18]),
  };
  return ddlBodies;
}

/**
 * Range-aware delivery of an in-memory DDL body under headers we choose.
 *
 * `delayFirstByteMs` is the slow-CDN shape: the response headers are already on
 * the wire and the first body byte is not, which is the case that must never
 * be reported to the viewer as an unsupported container.
 */
function serveDdl(res, req, body, headers, delayFirstByteMs = 0) {
  res.on("error", () => {});
  const size = body.length;
  const base = {
    ...headers,
    etag: `"${size.toString(16)}"`,
    "last-modified": LAST_MODIFIED,
    "accept-ranges": "bytes",
  };
  const range = parseRange(req.headers.range, size);
  if (range?.unsatisfiable) {
    return send(res, 416, { ...base, "content-range": `bytes */${size}` }, "");
  }
  const [start, end] = range ?? [0, size - 1];
  const slice = body.subarray(start, end + 1);
  if (range) {
    res.writeHead(206, {
      ...base,
      "content-range": `bytes ${start}-${end}/${size}`,
      "content-length": String(slice.length),
    });
  } else {
    res.writeHead(200, { ...base, "content-length": String(size) });
  }
  stats.bytes += slice.length;
  if (delayFirstByteMs > 0) {
    res.flushHeaders();
    setTimeout(() => res.end(slice), delayFirstByteMs);
    return;
  }
  res.end(slice);
}

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

  // --------------------------------------------------------- direct-download
  //
  // These must answer on whatever pathname they are given: a real DDL link has
  // no `media/360p.mp4` basename for the gate below to find, so anything that
  // sits behind that gate cannot reproduce the shape this product exists for.

  const shape =
    url.searchParams.get("head") === "truncated"
      ? "truncated"
      : url.searchParams.get("head") === "free"
        ? "free"
        : fault === "truncated-head"
          ? "truncated"
          : fault === "leading-free"
            ? "free"
            : "full";

  if (fault === "cdnr" || fault === "ext-in-redirect") {
    // The redirect carries the query forward, so the final hop is served by the
    // mode the test means: `cdnr` lands back here (generic type, real MP4 bytes,
    // no extension anywhere), `ext-in-redirect` lands on `octet`.
    if (url.pathname !== DDL_EXTENSIONLESS) {
      const target =
        fault === "cdnr"
          ? `${DDL_EXTENSIONLESS}?fault=cdnr&head=${shape}`
          : `${DDL_EXTENSION}?fault=octet&head=${shape}`;
      return send(res, 302, { location: target }, "");
    }
    return serveDdl(res, req, bodies()[shape], {
      "content-type": "application/octet-stream",
    });
  }

  if (
    fault === "octet" ||
    fault === "no-ct" ||
    fault === "leading-free" ||
    fault === "attachment" ||
    fault === "truncated-head" ||
    fault === "slow-body"
  ) {
    const headers = {};
    // `no-ct` is the harshest DDL shape there is: the origin says nothing at
    // all about what it is sending.
    if (fault !== "no-ct") headers["content-type"] = "application/octet-stream";
    if (fault === "attachment") {
      headers["content-disposition"] =
        `attachment; filename="${url.searchParams.get("name") ?? "Movie.mp4"}"`;
    }
    const delay = fault === "slow-body" ? 2500 : 0;
    return serveDdl(res, req, bodies()[shape], headers, delay);
  }

  // Not-media bodies, on an extensionless path. These are the shapes a real
  // DDL takes when the link has expired, the origin is misconfigured, or
  // something in front of it answers instead. Every one of them is served with
  // a 200, because that is what makes them hard: nothing in the status line
  // says the link is dead.
  if (fault === "html-as-binary" || fault === "expired-200" || fault === "lies-mp4") {
    const page = Buffer.from(
      "<!DOCTYPE html><html><head><title>403 Forbidden</title></head>" +
        "<body>Access denied</body></html>".padEnd(512, " "),
    );
    // `lies-mp4` is the dangerous one: a *recognised media type* over an error
    // page. A player that trusts the header will try to play a web page.
    const type = fault === "lies-mp4" ? "video/mp4" : "text/html; charset=utf-8";
    return serveDdl(res, req, page, { "content-type": type });
  }

  if (fault === "json-error") {
    return serveDdl(res, req, Buffer.from('{"error":"link_expired","code":410}'), {
      "content-type": "application/json",
    });
  }

  if (fault === "empty") {
    // Content-Length: 0. There is nothing to identify, and that is not the same
    // as "unsupported format".
    return serveDdl(res, req, Buffer.alloc(0), {
      "content-type": "application/octet-stream",
    });
  }

  if (fault === "gzip") {
    // We ask for `identity`; an origin that codes anyway makes every byte
    // offset a lie and the bytes unidentifiable.
    const gz = zlib.gzipSync(bodies()[shape]);
    return serveDdl(res, req, gz, {
      "content-type": "application/octet-stream",
      "content-encoding": "gzip",
      vary: "accept-encoding",
    });
  }

  if (fault === "hls") {
    // A manifest whose segments the proxy does not serve. Reporting it as
    // playable would mean the segments bypass the proxy entirely.
    return serveDdl(
      res,
      req,
      Buffer.from(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:6\n" +
          "#EXTINF:6.0,\nseg-0001.ts\n#EXT-X-ENDLIST\n",
      ),
      { "content-type": "application/vnd.apple.mpegurl", "cache-control": "no-store" },
    );
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
  const base = {
    "content-type": "video/mp4",
    etag,
    "last-modified": LAST_MODIFIED,
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
  // With `--port 0` this is not `PORT`, and the caller has to be told which port
  // it actually got: guessing a free one and handing it to `listen` races every
  // other process on the machine.
  const { port } = server.address();
  process.stdout.write(`fixture-origin listening on http://127.0.0.1:${port}\n`);
  process.stdout.write(`  media: ${Object.keys(MEDIA).join(", ")} from ${ROOT}\n`);
});