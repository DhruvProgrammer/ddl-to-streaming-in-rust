import { defineConfig } from "vite";

import { reticle } from '@reticlehq/vite-plugin';
// The Rust server serves `dist/` in production. In development Vite proxies the
// API to it so the player is same-origin and no CORS configuration exists.
// Read from the environment without depending on `@types/node`.
const env = (
  globalThis as { process?: { env?: Record<string, string | undefined> } }
).process?.env;
const target = env?.DDL_BACKEND ?? "http://127.0.0.1:8787";

export default defineConfig({
  plugins: [reticle()],
  server: {
    // Bind a named interface rather than `localhost`, which resolves to IPv6
    // first on some machines and makes http://127.0.0.1:5173 fail while
    // http://localhost:5173 works. One address, no surprises.
    host: "127.0.0.1",
    port: 5173,
    strictPort: true,
    proxy: {
      "/api": { target, changeOrigin: false },
      "/metrics": { target, changeOrigin: false },
    },
  },
  build: {
    target: "es2022",
    // One CSS file, one JS file, no vendor splitting: nothing here is big
    // enough to be worth a second request.
    cssCodeSplit: false,
    rollupOptions: {
      output: {
        entryFileNames: "assets/app.js",
        chunkFileNames: "assets/[name].js",
        assetFileNames: "assets/app.[ext]",
      },
    },
    reportCompressedSize: true,
  },
});