#!/usr/bin/env node
/**
 * Development: the Rust proxy on :8787 and the Vite dev server on :5173,
 * together, with prefixed logs and one Ctrl-C to stop both.
 *
 * `npm run dev`
 *
 * The page is served by Vite on :5173 during development; it proxies /api to
 * the proxy on :8787. Open http://127.0.0.1:5173.
 */

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const cargo = process.platform === "win32" ? "cargo.exe" : "cargo";
// Vite is invoked through its own bin with the current Node, not through
// `npm.cmd`: Node refuses to spawn a .cmd without a shell, and a shell here
// would mean a shell-quoting problem waiting to happen.
const viteBin = path.join(root, "frontend", "node_modules", "vite", "bin", "vite.js");

const COLOURS = { api: "[36m", web: "[35m", reset: "[0m" };

function run(name, command, args, cwd) {
  const child = spawn(command, args, {
    cwd,
    shell: false,
    env: {
      ...process.env,
      // The proxy keeps its log level; the dev server owns its own output.
      DDL_LOG: process.env.DDL_LOG ?? "info",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });

  const prefix = `${COLOURS[name] ?? ""}[${name}]${COLOURS.reset} `;
  const pipe = (stream, out) => {
    let buffer = "";
    stream.setEncoding("utf8");
    stream.on("data", (chunk) => {
      buffer += chunk;
      const lines = buffer.split("\n");
      buffer = lines.pop() ?? "";
      for (const line of lines) out.write(prefix + line + "\n");
    });
  };
  pipe(child.stdout, process.stdout);
  pipe(child.stderr, process.stderr);

  child.on("exit", (code, signal) => {
    process.stdout.write(`${prefix}exited (${signal ?? code})\n`);
    shutdown();
  });
  child.on("error", (e) => {
    process.stderr.write(`${prefix}failed to start: ${e.message}\n`);
    shutdown(1);
  });

  return child;
}

const children = [];
let closing = false;

function shutdown(code = 0) {
  if (closing) return;
  closing = true;
  for (const c of children) {
    if (c.exitCode === null) c.kill("SIGTERM");
  }
  // Give them a moment, then leave.
  setTimeout(() => process.exit(code), 300).unref();
}

process.on("SIGINT", () => shutdown());
process.on("SIGTERM", () => shutdown());

if (!existsSync(path.join(root, "frontend", "node_modules"))) {
  process.stderr.write("frontend dependencies are missing - run: npm run install:all\n");
  process.exit(1);
}
if (!existsSync(viteBin)) {
  process.stderr.write(`vite is not installed at ${viteBin} - run: npm run install:all\n`);
  process.exit(1);
}

process.stdout.write(
  "\n  page   http://127.0.0.1:5173\n" +
    "  api    http://127.0.0.1:8787\n\n" +
    "  ctrl-c stops both\n\n",
);

children.push(run("api", cargo, ["run", "--manifest-path", "backend/Cargo.toml"], root));
children.push(run("web", process.execPath, [viteBin], path.join(root, "frontend")));