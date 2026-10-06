/**
 * Extensionless direct-download links, in a real browser.
 *
 * The proxy decides playability from HTTP evidence and container bytes, never
 * from the URL. These flows exist because that is the requirement most easily
 * broken by a well-meaning change: a `video/mp4` string check is one line and
 * silently makes every real DDL unplayable.
 *
 * The fixture origin answers these faults on any pathname, because a genuine
 * DDL is `/download/37334`, not `/media/360p.mp4`.
 */

import { expect, test, type Page } from "@playwright/test";

const ORIGIN = process.env.DDL_ORIGIN ?? "http://127.0.0.1:9000";

/** A signed/temporary link: no extension, token in the query. */
const TOKEN = "supersecret-do-not-echo";

async function open(page: Page, url: string): Promise<void> {
  await page.goto("/");
  await page.fill("#urlInput", url);
  await page.click("#playBtn");
}

/** Resolve once frames are actually advancing, not merely buffered. */
async function waitForPlayback(page: Page, timeout = 30_000): Promise<number> {
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
                  `networkState=${v.networkState} error=${v.error?.code ?? "none"})`,
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

function state(page: Page) {
  return page.evaluate(() => {
    const v = document.getElementById("videoElement") as HTMLVideoElement;
    return {
      currentTime: v.currentTime,
      duration: v.duration,
      paused: v.paused,
      error: v.error?.code ?? null,
      src: v.currentSrc,
    };
  });
}

function snapshot(page: Page) {
  return page.evaluate(
    () =>
      (
        window as unknown as {
          ddlPlayer: { getState(): { state: string; error: string | null } };
        }
      ).ddlPlayer.getState(),
  );
}

test.describe("extensionless DDLs play", () => {
  test("octet-stream plus real MP4 bytes, on a path with no extension", async ({ page }) => {
    // The canonical case: the URL says nothing, the type says nothing useful,
    // and only the bytes can answer.
    await open(page, `${ORIGIN}/download/37334?fault=octet`);
    const t0 = await waitForPlayback(page);
    expect(t0).toBeGreaterThan(0);
    const s = await state(page);
    expect(s.error).toBeNull();
    // It went through the proxy, not straight to the origin.
    expect(s.src).toContain("/api/stream?");
    expect(await page.locator("#infoFormat").innerText()).toBe("MP4");
  });

  test("an origin that declares no Content-Type at all", async ({ page }) => {
    await open(page, `${ORIGIN}/get?fault=no-ct&id=12345`);
    await waitForPlayback(page);
    expect((await state(page)).error).toBeNull();
  });

  test("an ftyp box behind a leading free box is still identified as MP4", async ({ page }) => {
    // Legal ISO-BMFF: the signature is not at offset 4.
    //
    // This asserts *identification*, not playback, and the difference is the
    // point: the fixture splices an 8-byte `free` box onto the front of a
    // finished file, which shifts the sample tables in `stco` by eight bytes
    // and makes the result genuinely undecodable. A real muxer that emits
    // `free` writes it before computing those offsets. So this file is
    // correctly identified and correctly fails to decode, and the thing under
    // test is the identification.
    await page.goto("/");
    const probe = await page.evaluate(async () => {
      const r = await fetch("/api/probe", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ url: "http://127.0.0.1:9000/download/37334?fault=leading-free" }),
      });
      return (await r.json()) as Record<string, unknown>;
    });

    expect(probe["streamable"]).toBe(true);
    expect(probe["container"]).toBe("mp4");
    expect(probe["evidence"]).toBe("magic-bytes");
    // A URL-derived verdict would be impossible here: this path has no
    // extension and the origin declared a generic type.
    expect(probe["origin_content_type"]).toBe("application/octet-stream");

    // And the identification reaches the interface.
    await open(page, `${ORIGIN}/download/37334?fault=leading-free`);
    await expect(page.locator("#infoFormat")).toHaveText("MP4", { timeout: 25_000 });
  });

  test("a Content-Disposition filename identifies a body with no magic", async ({ page }) => {
    // Nothing to sniff and no extension: the origin naming its own file is the
    // only remaining evidence, so the format label comes from it.
    await open(page, `${ORIGIN}/dl/37334?fault=attachment&head=truncated&name=Movie.mp4`);
    await expect(page.locator("#infoFormat")).toHaveText("MP4", { timeout: 25_000 });
  });

  test("a signed link with no extension and a token in the query", async ({ page }) => {
    await open(page, `${ORIGIN}/file?fault=octet&token=${TOKEN}&expires=1780000000&sig=abc%3D`);
    await waitForPlayback(page);
    expect((await state(page)).error).toBeNull();
  });

  test("an extensionless URL redirecting to an extension-bearing CDN path", async ({ page }) => {
    await open(page, `${ORIGIN}/movie.mp4?fault=cdnr`);
    await waitForPlayback(page);
    expect((await state(page)).error).toBeNull();
  });

  test("the token never reaches the page", async ({ page }) => {
    const seen: string[] = [];
    page.on("request", (r) => seen.push(r.url()));
    await open(page, `${ORIGIN}/file?fault=octet&token=${TOKEN}`);
    await waitForPlayback(page);

    // The proxy must keep the query out of anything the browser can read: the
    // address bar, storage, and the DOM.
    expect(page.url()).not.toContain(TOKEN);
    const leaked = await page.evaluate((token) => {
      const haystack = [
        document.cookie,
        localStorage.getItem("url") ?? "",
        sessionStorage.getItem("url") ?? "",
        document.body.innerText,
      ].join(" ");
      return haystack.includes(token);
    }, TOKEN);
    expect(leaked).toBe(false);
    // And the media request carries it only as a query parameter to our own
    // API, which is where it belongs.
    expect(seen.some((u) => u.includes("/api/stream?"))).toBe(true);
  });
});

test.describe("the URL never rescues a body that is not media", () => {
  test("an HTML error page behind a .mp4 path is refused, not believed", async ({ page }) => {
    // The trap: a working-looking extension on a link that has expired. If any
    // code falls back to the extension here, the viewer is told to convert a
    // file that was never the problem.
    await open(page, `${ORIGIN}/download/37334?fault=octet&name=movie.mp4`);
    await waitForPlayback(page);
    // Sanity: the good case really does play, so the negative test below means
    // something.
    expect((await state(page)).error).toBeNull();

    await open(page, `${ORIGIN}/dl/movie.mp4?fault=html-as-binary`);
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 25_000,
    });
    await expect(page.locator("#infoFormat")).not.toHaveText("MP4");
    const note = (await page.locator("#note").innerText()).toLowerCase();
    expect(note.length).toBeGreaterThan(10);
  });

  test("an expired signed link returning a login page says so", async ({ page }) => {
    await open(page, `${ORIGIN}/file?token=expired&fault=expired-200`);
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 25_000,
    });
    const note = (await page.locator("#note").innerText()).toLowerCase();
    // The copy must point at the link, not at the viewer's file format.
    expect(note).toMatch(/web page|link|expired|sign/i);
    expect(note).not.toMatch(/convert .* to mp4/i);
  });

  test("a JSON error envelope behind a 200 is not media", async ({ page }) => {
    await open(page, `${ORIGIN}/dl/movie.mp4?fault=json-error`);
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 25_000,
    });
  });

  test("a lying Content-Type does not make an HTML page playable", async ({ page }) => {
    await open(page, `${ORIGIN}/download/37334?fault=lies-mp4`);
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 25_000,
    });
  });

  test("a content-encoded body is refused by name, not called a bad format", async ({ page }) => {
    await open(page, `${ORIGIN}/download/37334?fault=gzip`);
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 25_000,
    });
    const detail = (await page.locator("#errorSubtitle").innerText()).toLowerCase();
    expect(detail).toMatch(/compress|encod/i);
  });

  test("an HLS manifest is not promised as playable", async ({ page }) => {
    // Its segments would be fetched from the origin directly, outside the
    // proxy, so a signed link would 403. Saying so beats claiming and failing.
    await open(page, `${ORIGIN}/hls/37334?fault=hls`);
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 25_000,
    });
  });

  test("an empty body is reported as empty", async ({ page }) => {
    await open(page, `${ORIGIN}/download/37334?fault=empty`);
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 25_000,
    });
    const detail = (await page.locator("#errorSubtitle").innerText()).toLowerCase();
    expect(detail).toMatch(/empty|no video/i);
  });
});

test.describe("probe and stream agree", () => {
  test("a probe that says playable is followed by a stream that plays", async ({ page }) => {
    const streamStatuses: number[] = [];
    page.on("response", (r) => {
      if (r.url().includes("/api/stream")) streamStatuses.push(r.status());
    });

    await open(page, `${ORIGIN}/download/37334-abcdef?fault=octet`);
    await waitForPlayback(page);

    // The specific regression: the probe said yes and the stream endpoint
    // answered 415 for the same URL, so nothing ever played.
    expect(streamStatuses.length).toBeGreaterThan(0);
    expect(streamStatuses.every((s) => s !== 415)).toBe(true);
    expect((await snapshot(page)).error).toBeNull();
  });

  test("a probe that says unplayable is not followed by a stream that plays", async ({ page }) => {
    const streamStatuses: number[] = [];
    page.on("response", (r) => {
      if (r.url().includes("/api/stream")) streamStatuses.push(r.status());
    });

    await open(page, `${ORIGIN}/download/37334-html?fault=html-as-binary`);
    await expect(page.locator("#playerContainer")).toHaveAttribute("data-state", "error", {
      timeout: 25_000,
    });
    expect(streamStatuses.every((s) => s < 200 || s >= 300)).toBe(true);
  });
});