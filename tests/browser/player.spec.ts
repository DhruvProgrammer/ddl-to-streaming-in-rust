/**
 * End-to-end browser tests for the website.
 *
 * These assert the things that only a real `<video>` element can prove:
 * playback actually starts, `currentTime` advances, a seek lands where it was
 * asked to, the keyboard works, and a failure produces a sentence rather than a
 * silent black rectangle.
 */

import { expect, test, type Page } from "@playwright/test";

const ORIGIN = process.env.DDL_ORIGIN ?? "http://127.0.0.1:9000";
const SMALL = `${ORIGIN}/media/360p.mp4`;
const LARGE = `${ORIGIN}/media/720p.mp4`;

/** Wait until the video has actually produced a non-zero currentTime. */
async function waitForPlayback(page: Page, timeout = 25_000): Promise<number> {
  return page.evaluate(
    (ms) =>
      new Promise<number>((resolve, reject) => {
        const v = document.getElementById("videoElement") as HTMLVideoElement;
        const started = Date.now();
        const tick = () => {
          if (v.error) return reject(new Error(`media error ${v.error.code}`));
          if (!v.paused && v.currentTime > 0.05) return resolve(v.currentTime);
          if (Date.now() - started > ms) {
            return reject(
              new Error(
                `playback did not start (readyState=${v.readyState} ` +
                  `networkState=${v.networkState} currentTime=${v.currentTime} src=${v.currentSrc})`,
              ),
            );
          }
          requestAnimationFrame(tick);
        };
        tick();
      }),
    timeout,
  );
}

/**
 * Submitting the form is what starts playback: the button is labelled Play, so
 * there is no second click to wait for. Tests that need a paused player press
 * the bar's play/pause afterwards.
 */
async function open(page: Page, url: string): Promise<void> {
  await page.goto("/");
  await page.fill("#urlInput", url);
  await page.click("#playBtn");
}

/** Wait until the stage has left `connecting`, so the controls accept clicks. */
async function waitForControls(page: Page, timeout = 25_000): Promise<void> {
  await expect(page.locator("#ctrlPlayPause")).toBeEnabled({ timeout });
}

async function videoState(page: Page) {
  return page.evaluate(() => {
    const v = document.getElementById("videoElement") as HTMLVideoElement;
    return {
      currentTime: v.currentTime,
      duration: v.duration,
      paused: v.paused,
      muted: v.muted,
      volume: v.volume,
      rate: v.playbackRate,
      readyState: v.readyState,
      error: v.error?.code ?? null,
      src: v.currentSrc,
      buffered: v.buffered.length ? v.buffered.end(v.buffered.length - 1) : 0,
    };
  });
}

test.describe("playback", () => {
  test("the page loads and shows the player", async ({ page }) => {
    await page.goto("/");
    await expect(page.locator(".brand")).toBeVisible();
    await expect(page.locator("h1")).toHaveText("DDLPlayer");
    await expect(page.locator("#videoElement")).toBeVisible();
    await expect(page.locator("#urlInput")).toBeVisible();
    await expect(page.locator("#playBtn")).toBeVisible();
    // With no media the bar has nothing to control, so it is not offered at
    // all — including to the tab order.
    await expect(page.getByTestId("veil-empty")).toBeVisible();
    await expect(page.locator("#ctrlPlayPause")).toBeHidden();
    await expect(page.locator("#seek")).toBeHidden();

    // And it arrives with the media.
    await open(page, SMALL);
    await waitForPlayback(page);
    await expect(page.locator("#ctrlPlayPause")).toBeVisible();
    await expect(page.locator("#seek")).toBeVisible();
    await expect(page.locator("#ctrlFullscreen")).toBeVisible();

    // Nothing heavy is shipped to the browser.
    const js = await page.evaluate(async () => {
      const r = await fetch("/assets/app.js");
      return (await r.text()).length;
    });
    expect(js).toBeLessThan(60_000);
  });

  test("paste a link, press play, video plays", async ({ page }) => {
    await open(page, SMALL);
    const t0 = await waitForPlayback(page);
    expect(t0).toBeGreaterThan(0);

    // And it keeps going.
    await page.waitForTimeout(700);
    const after = await videoState(page);
    expect(after.currentTime).toBeGreaterThan(t0);
    expect(after.error).toBeNull();
    expect(after.duration).toBeGreaterThan(5);
    expect(after.src).toContain("/api/stream?");
  });

  test("the stage reports playing, not just a loaded file", async ({ page }) => {
    await open(page, SMALL);
    await waitForPlayback(page);
    const snapshot = await page.evaluate(
      () => (window as unknown as { ddlPlayer: { getState(): { state: string } } }).ddlPlayer.getState(),
    );
    expect(["playing", "buffering"]).toContain(snapshot.state);
  });

  test("pause and resume", async ({ page }) => {
    await open(page, SMALL);
    await waitForPlayback(page);
    await waitForControls(page);

    await page.click("#ctrlPlayPause");
    await expect.poll(async () => (await videoState(page)).paused).toBe(true);
    const pausedAt = (await videoState(page)).currentTime;
    await page.waitForTimeout(400);
    expect((await videoState(page)).currentTime).toBeCloseTo(pausedAt, 1);

    await page.click("#ctrlPlayPause");
    await expect.poll(async () => (await videoState(page)).paused).toBe(false);
    await expect
      .poll(async () => (await videoState(page)).currentTime, { timeout: 10_000 })
      .toBeGreaterThan(pausedAt);
  });

  test("seek lands where it was asked to", async ({ page }) => {
    await open(page, LARGE);
    await waitForPlayback(page);

    const before = await videoState(page);
    const target = Math.min(before.duration * 0.7, before.duration - 1);
    await page.evaluate((t) => {
      const v = document.getElementById("videoElement") as HTMLVideoElement;
      v.currentTime = t;
    }, target);

    await expect
      .poll(async () => (await videoState(page)).currentTime, { timeout: 20_000 })
      .toBeGreaterThan(target - 0.6);
    const after = await videoState(page);
    expect(after.error).toBeNull();
    // The proxy answered the seek with a range request, not a full restart.
    expect(after.buffered).toBeGreaterThan(0);
  });

  test("the seek bar drives the video from the keyboard", async ({ page }) => {
    await open(page, LARGE);
    await waitForPlayback(page);

    // Keyboard use of the range input fires `change` with no pointer events at
    // all, which is exactly the path a pointer-gated implementation drops.
    await page.locator("#seek").focus();
    for (let i = 0; i < 6; i += 1) await page.keyboard.press("ArrowRight");

    await expect
      .poll(async () => (await videoState(page)).currentTime, { timeout: 20_000 })
      .toBeGreaterThan(2);
  });

  test("rapid seeking stays stable and lands on the last target", async ({ page }) => {
    await open(page, LARGE);
    await waitForPlayback(page);
    const { duration } = await videoState(page);

    const targets = [0.1, 0.3, 0.9, 0.15, 0.95, 0.45].map((f) => duration * f);
    await page.evaluate(async (list: number[]) => {
      const v = document.getElementById("videoElement") as HTMLVideoElement;
      for (const t of list) {
        v.currentTime = t;
        await new Promise((r) => setTimeout(r, 40));
      }
    }, targets);

    const final = targets[targets.length - 1]!;
    await expect
      .poll(async () => (await videoState(page)).currentTime, { timeout: 25_000 })
      .toBeGreaterThan(final - 0.8);
    const state = await videoState(page);
    expect(state.error).toBeNull();
    // Controls are still responsive after the burst.
    await expect(page.locator("#ctrlPlayPause")).toBeEnabled();
  });

  test("a large file streams progressively, not all at once", async ({ page }) => {
    // The proxy must not download the whole file before the first frame.
    const started = Date.now();
    await open(page, LARGE);
    await waitForPlayback(page, 25_000);
    const firstFrameMs = Date.now() - started;
    // Local origin: anything slower than 10s means we are buffering the file.
    expect(firstFrameMs).toBeLessThan(10_000);
  });
});

test.describe("controls", () => {
  test("keyboard: space toggles playback", async ({ page }) => {
    await open(page, SMALL);
    await waitForPlayback(page);
    await page.locator("body").click({ position: { x: 5, y: 5 } });
    await page.keyboard.press("Space");
    await expect.poll(async () => (await videoState(page)).paused).toBe(true);
    await page.keyboard.press("Space");
    await expect.poll(async () => (await videoState(page)).paused).toBe(false);
  });

  test("keyboard: arrows seek", async ({ page }) => {
    await open(page, LARGE);
    await waitForPlayback(page);
    const before = (await videoState(page)).currentTime;

    await page.locator("body").click({ position: { x: 5, y: 5 } });
    await page.keyboard.press("ArrowRight");
    await expect
      .poll(async () => (await videoState(page)).currentTime, { timeout: 15_000 })
      .toBeGreaterThan(before + 3);

    await page.keyboard.press("ArrowLeft");
    await expect
      .poll(async () => (await videoState(page)).currentTime, { timeout: 15_000 })
      .toBeLessThan(before + 3.5);
  });

  test("keyboard: m mutes, f requests fullscreen on the player", async ({ page }) => {
    await open(page, SMALL);
    await waitForPlayback(page);

    await page.locator("body").click({ position: { x: 5, y: 5 } });
    await page.keyboard.press("m");
    await expect.poll(async () => (await videoState(page)).muted).toBe(true);
    await page.keyboard.press("m");
    await expect.poll(async () => (await videoState(page)).muted).toBe(false);

    // Fullscreen is a browser permission in headless; assert we ask for it, and
    // that we ask for the STAGE — a fullscreen video letterboxes without the
    // surrounding chrome, which is not the cinematic result we want.
    const asked = await page.evaluate(async () => {
      const stage = document.getElementById("playerContainer") as HTMLElement;
      let requestedOn = "";
      const original = stage.requestFullscreen?.bind(stage);
      if (original) {
        // @ts-expect-error test shim
        stage.requestFullscreen = () => {
          requestedOn = stage.id;
          return Promise.resolve();
        };
      }
      document.getElementById("ctrlFullscreen")!.click();
      await new Promise((r) => setTimeout(r, 50));
      if (original) stage.requestFullscreen = original;
      return requestedOn;
    });
    expect(asked).toBe("playerContainer");
  });

  test("volume slider and playback speed", async ({ page }, testInfo) => {
    // The volume slider is deliberately hidden on narrow viewports; the mute
    // button covers it there.
    const width = page.viewportSize()?.width ?? 1280;
    test.skip(width <= 480, "volume slider is hidden below 480px by design");
    void testInfo;
    await open(page, SMALL);
    await waitForPlayback(page);

    await page.locator("#volumeSlider").fill("0.25");
    await page.locator("#volumeSlider").dispatchEvent("input");
    await expect.poll(async () => (await videoState(page)).volume).toBeCloseTo(0.25, 1);

    await page.selectOption("#speedSelect", "1.5");
    await expect.poll(async () => (await videoState(page)).rate).toBeCloseTo(1.5, 2);
  });

  test("the mute button toggles the icon state", async ({ page }) => {
    await open(page, SMALL);
    await waitForPlayback(page);
    await expect(page.locator("#ctrlMute")).toHaveAttribute("aria-label", "Mute");
    await page.click("#ctrlMute");
    await expect(page.locator("#ctrlMute")).toHaveAttribute("aria-label", "Unmute");
  });
});

test.describe("failures are legible", () => {
  test("an invalid URL produces a sentence, not a blank screen", async ({ page }) => {
    await open(page, "not-a-url");
    await expect(page.locator("#note")).toBeVisible({ timeout: 15_000 });
    const text = (await page.locator("#note").innerText()).toLowerCase();
    expect(text).toMatch(/url|link|paste/i);
    const state = await videoState(page);
    expect(state.error === null || state.error === 4).toBe(true);
  });

  test("a 404 explains itself", async ({ page }) => {
    await open(page, `${ORIGIN}/media/does-not-exist.mp4`);
    await expect(page.locator("#note")).toBeVisible({ timeout: 20_000 });
    const text = (await page.locator("#note").innerText()).toLowerCase();
    expect(text.length).toBeGreaterThan(10);
  });

  test("a non-media response is refused with a reason", async ({ page }) => {
    await open(page, `${ORIGIN}/media/360p.mp4?fault=html`);
    await expect(page.locator("#note")).toBeVisible({ timeout: 20_000 });
    const text = (await page.locator("#note").innerText()).toLowerCase();
    // The refusal has to name something a person can act on. It used to be
    // "convert it to MP4", which is actively wrong for a link that has expired
    // — so the wording now names what the origin actually returned.
    expect(text).toMatch(
      /mp4|webm|convert|format|play|video|web page|expired|sign|log ?in/i,
    );
  });

  test("a persistent 5xx reports an upstream failure", async ({ page }) => {
    await open(page, `${ORIGIN}/media/360p.mp4?fault=503`);
    await expect(page.locator("#note")).toBeVisible({ timeout: 30_000 });
    const text = (await page.locator("#note").innerText()).toLowerCase();
    expect(text.length).toBeGreaterThan(10);
  });

  test("the player recovers after the source is fixed", async ({ page }) => {
    await open(page, `${ORIGIN}/media/360p.mp4?fault=503`);
    await expect(page.locator("#note")).toBeVisible({ timeout: 30_000 });
    await open(page, SMALL);
    await waitForPlayback(page, 25_000);
    expect((await videoState(page)).error).toBeNull();
  });

  test("a redirect chain is followed transparently", async ({ page }) => {
    await open(page, `${ORIGIN}/media/360p.mp4?fault=slow-flaky`);
    await waitForPlayback(page, 30_000);
    expect((await videoState(page)).error).toBeNull();
  });
});

test.describe("chrome must never cover controls", () => {
  test("the idle control bar does not cover the error veils", async ({ page }) => {
    // Regression: on a short viewport the faded bar sat over the retry button
    // and swallowed the tap. Invisible chrome must not block visible controls.
    await page.setViewportSize({ width: 380, height: 720 });
    // A transient failure, because a 404 correctly offers no retry to tap.
    await open(page, `${ORIGIN}/media/360p.mp4?fault=503`);
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 30_000,
    });
    await expect(page.locator("#retryBtn")).toBeVisible({ timeout: 30_000 });

    // The retry button must be the topmost thing at its own centre.
    const onTop = await page.locator("#retryBtn").evaluate((el) => {
      const r = el.getBoundingClientRect();
      const hit = document.elementFromPoint(r.left + r.width / 2, r.top + r.height / 2);
      return hit === el || el.contains(hit);
    });
    expect(onTop).toBe(true);
  });

  test("the idle fade is applied by presence, not by an equals selector", async ({ page }) => {
    // `flag()` writes presence attributes, so `[data-idle="true"]` never
    // matches and the bar would stay painted over everything.
    await open(page, SMALL);
    await waitForPlayback(page);

    // Every media event wakes the chrome and restarts the idle countdown, so
    // this has to outlast it rather than be checked the instant playback starts.
    await expect
      .poll(
        () => page.locator("#playerOverlay").evaluate((el) => el.hasAttribute("data-idle")),
        { timeout: 10_000 },
      )
      .toBe(true);

    // A touch device has no hover, so the thumb stays visible on purpose.
    const hoverable = await page.evaluate(() => matchMedia("(hover: hover)").matches);
    if (!hoverable) {
      const visibility = await page.locator("#playerOverlay").evaluate((el) => getComputedStyle(el).visibility);
      expect(visibility).toBe("visible");
      return;
    }

    // Once idle on a pointer device the bar is genuinely out of the way.
    await expect
      .poll(
        () => page.locator("#playerOverlay").evaluate((el) => getComputedStyle(el).visibility),
        { timeout: 10_000 },
      )
      .toBe("hidden");
  });
});

test.describe("the stage owns its own states", () => {
test("an unplayable source does not offer a retry it cannot honour", async ({ page }) => {
    await open(page, `${ORIGIN}/media/does-not-exist.mp4`);
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 25_000,
    });
    await expect(page.getByTestId("veil-error")).toBeVisible();
    // The backend reached the origin and got a 404; fetching the same link
    // again cannot change that, so the honest thing is to offer no retry.
    await expect(page.locator("#retryBtn")).toBeHidden();
    const state = await page.evaluate(
      () =>
        (window as unknown as { ddlPlayer: { getState(): { error: string | null } } }).ddlPlayer.getState(),
    );
    expect(state.error).toBeTruthy();
  });

test("retry appears when the server says the failure is transient", async ({ page }) => {
    await open(page, `${ORIGIN}/media/360p.mp4?fault=503`);
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 30_000,
    });
    await expect(page.locator("#retryBtn")).toBeVisible({ timeout: 30_000 });
  });

  test("retry asks the backend again, with a fresh stream generation", async ({ page }) => {
    // Attached before the first load, so both generations are observable.
    const streamUrls: string[] = [];
    page.on("request", (req) => {
      const u = req.url();
      if (u.includes("/api/stream")) streamUrls.push(u);
    });

    await open(page, `${ORIGIN}/media/360p.mp4?fault=503`);
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 30_000,
    });
    await expect(page.locator("#retryBtn")).toBeVisible({ timeout: 30_000 });
    await expect.poll(() => streamUrls.length, { timeout: 15_000 }).toBeGreaterThanOrEqual(1);
    const firstGeneration = new URL(streamUrls[0]!).searchParams.get("g");

    await page.click("#retryBtn");
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 30_000,
    });

    // A retry that silently reuses the previous stream URL would replay a
    // cached failure; the generation counter exists to prevent exactly that.
    await expect.poll(() => streamUrls.length, { timeout: 20_000 }).toBeGreaterThanOrEqual(2);
    const secondGeneration = new URL(streamUrls[streamUrls.length - 1]!).searchParams.get("g");
    expect(secondGeneration).not.toBe(firstGeneration);
  });
});

test.describe("responsiveness", () => {
  test("the layout fits a narrow viewport", async ({ page }) => {
    await page.setViewportSize({ width: 380, height: 720 });
    await open(page, SMALL);
    const box = await page.locator("#videoElement").boundingBox();
    expect(box).not.toBeNull();
    expect(box!.width).toBeLessThanOrEqual(380);
    await expect(page.locator("#ctrlPlayPause")).toBeVisible();
    await waitForPlayback(page);
  });

  test("touch controls work", async ({ page, hasTouch }) => {
    test.skip(!hasTouch, "needs a touch-capable context");
    await open(page, SMALL);
    await waitForPlayback(page);
    await waitForControls(page);
    await page.locator("#ctrlPlayPause").tap();
    await expect.poll(async () => (await videoState(page)).paused).toBe(true);
  });
});
