import { expect, test } from "@playwright/test";

/**
 * Capture the page for visual review. Writes to `docs/` (git-ignored).
 * Not part of the correctness suite; run it when the UI changes.
 */
const OUT = "docs/shot";

test("screenshot: idle, playing, and failed", async ({ page }, testInfo) => {
  const dir = testInfo.config.rootDir.replace(/tests\\browser$/, "").replace(/tests\/browser$/, "");
  const origin = process.env.DDL_ORIGIN ?? "http://127.0.0.1:9000";

  await page.setViewportSize({ width: 1280, height: 800 });
  await page.goto("/");
  await expect(page.getByTestId("veil-empty")).toBeVisible();
  await page.screenshot({ path: `${dir}/${OUT}-idle.png`, fullPage: true });

  // Submitting starts playback, so there is no second click to wait for.
  await page.fill("#urlInput", `${origin}/media/720p.mp4`);
  await page.click("#playBtn");
  await expect
    .poll(
      () => page.locator("#videoElement").evaluate((v: HTMLVideoElement) => v.currentTime),
      { timeout: 25_000 },
    )
    .toBeGreaterThan(0.3);
  await page.mouse.move(640, 400);
  await page.waitForTimeout(600);
  await page.screenshot({ path: `${dir}/${OUT}-playing.png`, fullPage: true });

  await page.fill("#urlInput", `${origin}/media/does-not-exist.mp4`);
  await page.click("#playBtn");
  await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
    timeout: 25_000,
  });
  await page.waitForTimeout(400);
  await page.screenshot({ path: `${dir}/${OUT}-error.png`, fullPage: true });
});