/**
 * The player controller.
 *
 * Owns the state machine, the network, and every listener on the video element.
 * Views are told what to show; they never decide anything themselves.
 *
 * Two rules shape this file:
 *   1. Nothing about *starting* playback waits on the probe. The media request
 *      goes out immediately and the probe is told whatever it finds.
 *   2. Every task has an owner. Abandoning a load cancels its probe, and
 *      stale async work compares its epoch and bows out.
 */

import { probe, report, streamUrl } from "./api";
import { fromMedia, fromThrown, validateInput } from "./errors";
import { Session } from "./session";
import type { PlayerErrorView, PlayerSnapshot, PlayerState, ProbeView } from "./types";

import { Player } from "../components/Player";
import { PlayerControls } from "../components/PlayerControls";
import { MediaInfo } from "../components/MediaInfo";
import { StatusMessage, UrlInput } from "../components/UrlInput";
import { container, duration, resolution } from "../utils/format";

const SEEK_STEP = 5;
const VOLUME_STEP = 0.05;
/** Double-tap window and slop, and the thirds that seek. */
const TAP_MS = 300;
const TAP_SLOP = 50;
const DOUBLE_TAP_SEEK = 10;

export class DdlPlayer {
  readonly session = new Session();

  readonly #player = new Player();
  readonly #controls = new PlayerControls();
  readonly #input = new UrlInput();
  readonly #status = new StatusMessage();
  readonly #media = new MediaInfo();

  #state: PlayerState = "empty";
  #probe: AbortController | null = null;
  #loadStartedAt = 0;
  #playReported = false;
  #seekStartedAt = 0;
  #stallStartedAt = 0;
  #lastUrl = "";
  #lastError: PlayerErrorView | null = null;
  #format = "—";
  /** Bumped on every load; stale async work compares against it and bows out. */
  #epoch = 0;
  #listeners = new Set<() => void>();
  #lastTap = { at: 0, x: 0 };

  constructor() {
    this.#wire();
  }

  #video(): HTMLVideoElement {
    return this.#player.video;
  }

  // ------------------------------------------------------------------ wiring

  #wire(): void {
    const video = this.#video();

    this.#input.bind({
      onSubmit: (url) => void this.#load(url),
      onInput: (_url, valid) => this.#input.setValid(valid),
    });

    this.#player.bind({
      onRetry: () => void this.#retry(),
      onActivity: () => this.#controls.wake(),
      onToggle: () => void this.toggle(),
    });

    this.#controls.bind({
      onToggle: () => void this.toggle(),
      onSeek: (fraction) => this.#seekToFraction(fraction),
      onScrub: () => undefined,
      onVolume: (value) => {
        video.volume = value;
        video.muted = value === 0;
      },
      onMute: () => {
        video.muted = !video.muted;
      },
      onRate: (value) => {
        video.playbackRate = value;
      },
      onFullscreen: () => void this.#player.toggleFullscreen(),
      onPictureInPicture: () => void this.#player.togglePictureInPicture(),
      onActivity: () => undefined,
    });

    // ---- media element events, the only source of playback truth ----
    video.addEventListener("play", () => {
      this.#setState(video.paused ? "paused" : "playing");
      this.#controls.setPlaying(true);
      this.#controls.wake();
      if (!this.#playReported) {
        this.#playReported = true;
        report("play", performance.now() - this.#loadStartedAt);
      }
    });

    video.addEventListener("pause", () => {
      // A `pause` caused by an aborted load or a failed source is not the viewer
      // pausing — and it is delivered asynchronously, so an unconditional
      // handler here overwrites whatever the load actually decided.
      if (this.#state !== "playing" && this.#state !== "buffering") return;
      if (video.ended) return;
      this.#setState("paused");
      this.#controls.setPlaying(false);
      // Paused is a state you may walk away from; keep the chrome visible.
      this.#controls.wake();
    });

    video.addEventListener("loadedmetadata", () => {
      this.#controls.setClock(duration(video.currentTime), duration(video.duration));
      this.#renderMedia();
    });

    video.addEventListener("durationchange", () => {
      this.#controls.setClock(duration(video.currentTime), duration(video.duration));
      this.#renderMedia();
    });

    video.addEventListener("timeupdate", () => {
      this.#controls.setClock(duration(video.currentTime), duration(video.duration));
      this.#controls.timeline.sync(video.currentTime, video.duration);
      this.#publish();
    });

    video.addEventListener("progress", () => this.#controls.timeline.syncBuffered(video));

    video.addEventListener("seeking", () => {
      this.#seekStartedAt = performance.now();
      this.#controls.wake();
    });

    video.addEventListener("seeked", () => {
      if (this.#seekStartedAt > 0) {
        report("seek", performance.now() - this.#seekStartedAt);
        this.#seekStartedAt = 0;
      }
    });

    video.addEventListener("waiting", () => {
      if (this.#state === "playing") {
        this.#stallStartedAt = performance.now();
        this.#setState("buffering");
      }
      this.#controls.wake();
    });

    video.addEventListener("playing", () => {
      this.#setState("playing");
      this.#controls.setPlaying(true);
      if (this.#stallStartedAt > 0) {
        report("stall", performance.now() - this.#stallStartedAt);
        this.#stallStartedAt = 0;
      }
    });

    // `canplay` is the first moment the source is genuinely playable. Anything
    // earlier is a guess, so this is the only event allowed to end `connecting`.
    video.addEventListener("canplay", () => {
      if (video.paused && (this.#state === "connecting" || this.#state === "buffering")) {
        this.#setState("paused");
      }
    });

    video.addEventListener("ended", () => {
      this.#setState("completed");
      this.#controls.setPlaying(false);
      this.#controls.rest();
    });

    video.addEventListener("volumechange", () => {
      this.#controls.setVolume(video.volume, video.muted);
    });

    video.addEventListener("enterpictureinpicture", () => this.#controls.wake());

    video.addEventListener("error", () => this.#mediaFailed());

    // ---- gestures ----
    this.#player.root.addEventListener("touchend", (e) => this.#onTouchEnd(e));

    // ---- keyboard ----
    window.addEventListener("keydown", (e) => this.#onKey(e));

    document.addEventListener("fullscreenchange", () => {
      const active = document.fullscreenElement === this.#player.root;
      this.#controls.setFullscreen(active);
      this.#controls.wake();
    });

    this.#controls.setFullscreen(false);
    this.#controls.setVolume(video.volume, video.muted);
    this.#controls.setPlaying(false);
    this.#controls.setClock("0:00", "0:00");
    this.#controls.enablePictureInPicture(
      "pictureInPictureEnabled" in document && !video.disablePictureInPicture,
    );
    this.#controls.rest();
    this.#media.clear();
    this.#setState("empty");
  }

  // ------------------------------------------------------------------ loading

  /**
   * Load a source. The media request is issued first and unconditionally; the
   * probe runs beside it and is treated as information, never as a gate.
   */
  async #load(raw: string): Promise<void> {
    const invalid = validateInput(raw);
    if (invalid) {
      this.#input.setValid(false);
      this.#status.error(invalid.message, invalid.action);
      this.#input.focus();
      return;
    }

    const url = raw.trim();
    this.#lastUrl = url;
    this.#lastError = null;
    this.#format = "—";

    // A new load invalidates everything before it.
    this.#epoch += 1;
    const epoch = this.#epoch;
    this.#probe?.abort();

    const video = this.#video();
    video.pause();
    video.removeAttribute("src");
    video.load();

    this.#status.clear();
    this.#input.setValid(true);
    this.#input.setBusy(true);
    this.#controls.timeline.reset();
    this.#media.clear();
    this.#playReported = false;
    this.#loadStartedAt = performance.now();
    this.#setState("connecting");

    // The media request goes out now. This is the whole point of the product.
    video.src = streamUrl(url, this.session.id, this.session.next());
    video.load();

    // The button says Play, so pressing it starts playback. If the browser
    // refuses on autoplay-policy grounds that is not a broken source, and the
    // stage is left ready for the viewer to press play themselves.
    void this.#autoplay(epoch);

    void this.#probeBeside(url, epoch);
  }

  /** Start playback after a load. Distinguishes "blocked" from "broken". */
  async #autoplay(epoch: number): Promise<void> {
    const video = this.#video();
    let refusal: unknown;
    try {
      await video.play();
      return;
    } catch (e) {
      if (epoch !== this.#epoch) return;
      refusal = e;
    }
    // `play()` refusing is not proof the source is broken — it is also what an
    // autoplay policy, a pause() or a competing load() looks like. The media
    // element's own `error` event is the authority on that, and if it has not
    // fired then nothing here is wrong with the source.
    if (video.error) {
      this.#mediaFailed();
      return;
    }
    // play() and the media error race, and whichever loses must not overwrite
    // the other's verdict. A source already known to be broken stays broken.
    if (this.#state === "error" || this.#state === "empty") return;
    const failure = fromThrown(refusal);
    this.#status.info(failure.message, failure.action);
    this.#controls.wake();
    this.#setState("paused");
  }

  /** Probe in parallel. A failure here is information; playback may still work. */
  async #probeBeside(url: string, epoch: number): Promise<void> {
    const controller = new AbortController();
    this.#probe = controller;
    try {
      const view: ProbeView = await probe(url, controller.signal);
      if (epoch !== this.#epoch) return;
      this.#input.setBusy(false);

      if (!view.streamable) {
        const message = view.reason ?? view.warning ?? "This media cannot be played here.";
        this.#status.error(message, view.warning ?? undefined);
        // Remember the server's own reason. It knows why; the browser will only
        // report a generic media code, and the error veil should show the truth.
        this.#lastError = {
          message,
          detail: view.contentType ?? "",
          action: "Check the link points at a playable video file.",
          // The server reached the source and judged the media itself. Asking
          // again would fetch the same bytes and reach the same verdict.
          retryable: false,
        };
        this.#format = container(view.container);
        // The media element may already have failed with a bare media code that
        // carries no reason and offers no retry. The server knows better, and
        // it just answered, so let its reason replace the browser's shrug.
        if (this.#state === "error") this.#fail(this.#lastError);
        return;
      }
      if (view.warning) this.#status.info(view.warning);
      this.#format = container(view.container);
      this.#renderMedia();
    } catch (e) {
      if (epoch !== this.#epoch || controller.signal.aborted) return;
      this.#input.setBusy(false);
      // A probe that fails has still told us something about the source: the
      // backend either rejected it or could not be reached. The browser will
      // only ever report a bare media code for the same situation, so let the
      // server's reason — and its view of whether retrying helps — stand.
      const failure = fromThrown(e);
      this.#lastError = failure;
      this.#status.error(failure.message, failure.detail);
      if (this.#state === "error") this.#fail(failure);
    } finally {
      if (this.#probe === controller) this.#probe = null;
    }
  }

  async #retry(): Promise<void> {
    if (this.#lastUrl) await this.#load(this.#lastUrl);
  }

  // ------------------------------------------------------------------ actions

  async toggle(): Promise<void> {
    const video = this.#video();
    this.#controls.wake();
    if (video.ended) {
      // Replay from the top rather than resuming a finished video.
      video.currentTime = 0;
    }
    if (video.paused) {
      try {
        await video.play();
      } catch (e) {
        // Same reasoning as #autoplay: a refusal from play() is only proof of a
        // broken source when the media element itself says so.
        if (video.error) {
          this.#mediaFailed();
          return;
        }
        if (this.#state === "error") return;
        const failure = fromThrown(e);
        this.#status.info(failure.message, failure.action);
      }
    } else {
      video.pause();
    }
  }

  #seekToFraction(fraction: number): void {
    const video = this.#video();
    const total = video.duration;
    if (!Number.isFinite(total) || total <= 0) return;
    video.currentTime = Math.min(total - 0.05, Math.max(0, fraction * total));
  }

  #seekBy(seconds: number): void {
    const video = this.#video();
    const total = video.duration;
    if (!Number.isFinite(total) || total <= 0) return;
    video.currentTime = Math.min(total, Math.max(0, video.currentTime + seconds));
    this.#controls.wake();
  }

  /** Double-tap the left or right third to jump; the middle is left alone. */
  #onTouchEnd(e: TouchEvent): void {
    if (this.#state !== "playing" && this.#state !== "paused") return;
    const touch = e.changedTouches[0];
    if (!touch) return;

    const now = performance.now();
    const isRepeat = now - this.#lastTap.at < TAP_MS && Math.abs(touch.clientX - this.#lastTap.x) < TAP_SLOP;
    if (!isRepeat) {
      this.#lastTap = { at: now, x: touch.clientX };
      return;
    }
    this.#lastTap = { at: 0, x: 0 };

    const rect = this.#player.root.getBoundingClientRect();
    const where = (touch.clientX - rect.left) / rect.width;
    if (where < 0.35) {
      this.#seekBy(-DOUBLE_TAP_SEEK);
      this.#player.flashSeek(`−${DOUBLE_TAP_SEEK}s`);
    } else if (where > 0.65) {
      this.#seekBy(DOUBLE_TAP_SEEK);
      this.#player.flashSeek(`+${DOUBLE_TAP_SEEK}s`);
    }
  }

  #onKey(e: KeyboardEvent): void {
    const target = e.target as HTMLElement | null;
    const typing =
      target !== null &&
      (target.tagName === "INPUT" ||
        target.tagName === "SELECT" ||
        target.tagName === "TEXTAREA");

    if (typing) {
      // Escape leaves the field; everything else belongs to what is focused.
      if (e.key === "Escape") target.blur();
      return;
    }

    switch (e.key) {
      case " ":
      case "k":
        e.preventDefault();
        void this.toggle();
        break;
      case "ArrowLeft":
        e.preventDefault();
        this.#seekBy(-SEEK_STEP);
        break;
      case "ArrowRight":
        e.preventDefault();
        this.#seekBy(SEEK_STEP);
        break;
      case "j":
        this.#seekBy(-10);
        break;
      case "l":
        this.#seekBy(10);
        break;
      case "m":
        this.#video().muted = !this.#video().muted;
        break;
      case "f":
        void this.#player.toggleFullscreen();
        break;
      case "ArrowUp":
        e.preventDefault();
        this.#nudgeVolume(VOLUME_STEP);
        break;
      case "ArrowDown":
        e.preventDefault();
        this.#nudgeVolume(-VOLUME_STEP);
        break;
      case "0":
        this.#video().currentTime = 0;
        break;
      case "/":
        e.preventDefault();
        this.#input.focus();
        break;
      default:
        this.#controls.wake();
    }
  }

  #nudgeVolume(delta: number): void {
    const video = this.#video();
    video.volume = Math.min(1, Math.max(0, video.volume + delta));
    video.muted = video.volume === 0;
  }

  // -------------------------------------------------------------------- state

  #setState(state: PlayerState, error?: PlayerErrorView): void {
    if (state === this.#state && !error) return;
    this.#state = state;
    this.#player.setState(state, error);
    // Interactivity follows the state machine rather than being sprinkled
    // through the load path, so the two cannot drift apart.
    const usable =
      state === "playing" || state === "paused" || state === "buffering" || state === "completed";
    this.#controls.setInteractive(usable);
    if (state === "empty" || state === "error") this.#controls.rest();
    this.#publish();
  }

  /**
   * The single place a source that will not play becomes an error view.
   *
   * Both the media element's `error` event and a refused `play()` land here.
   * They race — whichever arrives last must not overwrite the other's verdict —
   * so they share one decision: if the server has already explained this failure,
   * its explanation is the one shown, because it knows more than a media code.
   */
  #mediaFailed(): void {
    this.#fail(this.#lastError ?? fromMedia(this.#video().error?.code));
  }

  #fail(failure: PlayerErrorView): void {
    this.#state = "error";
    this.#lastError = failure;
    this.#player.setState("error", failure);
    this.#controls.setInteractive(false);
    this.#controls.rest();
    this.#status.error(failure.message, failure.detail);
    report("error");
    this.#publish();
  }

  // ------------------------------------------------------------------- render

  #renderMedia(): void {
    const video = this.#video();
    this.#media.render({
      resolution: resolution(video.videoWidth, video.videoHeight),
      format: this.#format,
      duration: duration(video.duration),
    });
  }

  // ------------------------------------------------------------- public entry

  /** Used by `?url=` in the shell. */
  async loadFromQuery(url: string): Promise<void> {
    this.#input.value = url;
    await this.#load(url);
  }

  /**
   * Presentation only: force a state so the design can be reviewed without
   * breaking a real source. Never used by the playback path.
   */
  preview(state: PlayerState, error?: PlayerErrorView): void {
    if (state === "empty") {
      this.#media.clear();
      this.#controls.rest();
      this.#setState("empty");
      return;
    }
    this.#lastUrl = this.#lastUrl || "https://cdn.example.com/media/feature-film.mkv";
    this.#format = "MP4";
    this.#media.render({ resolution: "1080p", format: this.#format, duration: "52:13" });
    this.#controls.setPlaying(state === "playing" || state === "buffering");
    this.#controls.setClock("4:12", "52:13");
    this.#controls.timeline.sync(252, 3133);
    this.#controls.timeline.syncBuffered(this.#video());
    this.#setState(state, error);
    this.#controls.wake();
  }

  // ------------------------------------------------------- observable state

  /**
   * What the player believes, as plain data.
   *
   * Deliberately read from the same fields the state machine writes, so a test
   * (or a person in a console) reads the app's own account of itself rather
   * than reconstructing it from the DOM.
   */
  getState(): PlayerSnapshot {
    const video = this.#video();
    return {
      state: this.#state,
      url: this.#lastUrl,
      error: this.#lastError?.message ?? null,
      paused: video.paused,
      currentTime: video.currentTime,
      duration: Number.isFinite(video.duration) ? video.duration : 0,
      volume: video.volume,
      muted: video.muted,
      rate: video.playbackRate,
      fullscreen: document.fullscreenElement === this.#player.root,
    };
  }

  /** Subscribe to snapshot changes. Returns the unsubscribe function. */
  subscribe(listener: () => void): () => void {
    this.#listeners.add(listener);
    return () => this.#listeners.delete(listener);
  }

  #publish(): void {
    for (const listener of this.#listeners) listener();
  }
}