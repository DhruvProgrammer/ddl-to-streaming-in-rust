// Playwright drives the *website*: the built page served by the Rust binary,
// against a fixture origin. Chromium/Firefox/WebKit where the browser is
// installed; the runner skips what it cannot launch.
//
//   npx playwright test            (needs the stack running)
//   npx playwright test --project=mobile

import { defineConfig, devices } from "@playwright/test";

const SERVER = process.env.DDL_SERVER ?? "http://127.0.0.1:8787";
const ORIGIN = process.env.DDL_ORIGIN ?? "http://127.0.0.1:9000";

export default defineConfig({
  testDir: "./tests/browser",
  timeout: 60_000,
  expect: { timeout: 15_000 },
  fullyParallel: false,
  workers: 1,
  retries: 0,
  reporter: [["list"]],
  use: {
    baseURL: SERVER,
    trace: "retain-on-failure",
    video: "off",
  },
  projects: [
    {
      name: "chromium",
      use: { ...devices["Desktop Chrome"] },
    },
    {
      name: "mobile",
      use: { ...devices["Pixel 7"] },
    },
    {
      name: "firefox",
      use: { ...devices["Desktop Firefox"] },
    },
    {
      name: "webkit",
      use: { ...devices["Desktop Safari"] },
    },
  ],
});

export const originUrl = (path: string, fault?: string) =>
  `${ORIGIN}/media/${path}${fault ? `?fault=${fault}` : ""}`;
