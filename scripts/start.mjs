#!/usr/bin/env node
/**
 * Start the website.
 *
 * Invoked by `npm start`. Deliberately a Node script rather than a bare path in
 * package.json: npm runs scripts through the platform shell, and a relative
 * path with forward slashes is not portable to `cmd.exe`. Resolving the binary
 * here means one code path on Windows, macOS and Linux.
 *
 * Serves `frontend/dist` so the page and the API are same-origin.
 */

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const binary = path.join(root, "backend", "target", "release", "ddl-player.exe");
const fallback = path.join(root, "backend", "target", "release", "ddl-player");
const exe = existsSync(binary) ? binary : fallback;

if (!existsSync(exe)) {
  process.stderr.write(
    `Server binary not found at ${exe}\n` +
      `Build it first:  npm run build:server\n`,
  );
  process.exit(1);
}

const dist = path.join(root, "frontend", "dist");
if (!existsSync(path.join(dist, "index.html"))) {
  process.stderr.write(
    `Frontend build not found at ${dist}\n` +
      `Build it first:  npm run build:web\n`,
  );
  process.exit(1);
}

// Sensible defaults for a local run. Every one is overridable in the
// environment; none of them weaken the production defaults, except the
// private-host escape hatch, which is off unless you ask for it.
const env = {
  ...process.env,
  DDL_BIND: process.env.DDL_BIND ?? "127.0.0.1:8787",
  DDL_STATIC_DIR: process.env.DDL_STATIC_DIR ?? dist,
  DDL_LOG: process.env.DDL_LOG ?? "info",
};

const child = spawn(exe, [], {
  env,
  stdio: "inherit",
  shell: false,
});

const stop = (signal) => {
  if (child.exitCode === null) child.kill(signal);
};
process.on("SIGINT", () => stop("SIGINT"));
process.on("SIGTERM", () => stop("SIGTERM"));

child.on("exit", (code, signal) => {
  process.exitCode = signal ? 1 : (code ?? 0);
});
child.on("error", (e) => {
  process.stderr.write(`failed to start the server: ${e.message}\n`);
  process.exit(1);
});

process.stdout.write(`\nserving ${dist}\n`);