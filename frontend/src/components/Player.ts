/**
 * The stage: the video element and the state veils that sit over it.
 *
 * There is exactly one truth — `PlayerState` — and every visible state is a
 * function of it. No element decides its own visibility independently.
 */

import { $, flag, show } from "../utils/dom";
import type { PlayerErrorView, PlayerState } from "../player/types";

export interface PlayerEvents {
  onRetry: () => void;
  onActivity: () => void;
  onToggle: () => void;
}

/** States where there is no usable media, so the chrome has nothing to say. */
const BARE: ReadonlySet<PlayerState> = new Set<PlayerState>(["empty", "error", "completed"]);

export class Player {
  readonly #container: HTMLElement;
  readonly #wrapper: HTMLElement;
  readonly video: HTMLVideoElement;
  readonly #centerPlay: HTMLButtonElement;
  readonly #statusBar: HTMLElement;
  readonly #empty: HTMLElement;
  readonly #busy: HTMLElement;
  readonly #busyTitle: HTMLElement;
  readonly #error: HTMLElement;
  readonly #errorDetail: HTMLElement;
  readonly #retry: HTMLButtonElement;
  readonly #completed: HTMLElement;
  #events: PlayerEvents | null = null;
  #state: PlayerState = "empty";

  constructor() {
    this.#container = $("#playerContainer");
    this.#wrapper = $("#playerWrapper");
    this.video = $<HTMLVideoElement>("#videoElement");
    this.#centerPlay = $<HTMLButtonElement>("#centerPlay");
    this.#statusBar = $("#statusBar");
    this.#empty = $("#stateEmpty");
    this.#busy = $("#stateLoading");
    this.#busyTitle = $("#loadingText");
    this.#error = $("#stateError");
    this.#errorDetail = $("#errorSubtitle");
    this.#retry = $<HTMLButtonElement>("#retryBtn");
    this.#completed = $("#stateCompleted");

    this.#retry.addEventListener("click", () => this.#events?.onRetry());
    this.#centerPlay.addEventListener("click", () => this.#events?.onToggle());
    this.#wrapper.addEventListener("pointermove", () => this.#events?.onActivity());
    this.#wrapper.addEventListener("pointerdown", () => this.#events?.onActivity());
  }

  bind(events: PlayerEvents): void {
    this.#events = events;
  }

  get state(): PlayerState {
    return this.#state;
  }

  get root(): HTMLElement {
    return this.#container;
  }

  get hasMedia(): boolean {
    return !BARE.has(this.#state);
  }

  /**
   * The single state transition. `error` is passed only for that state.
   */
  setState(state: PlayerState, error?: PlayerErrorView): void {
    this.#state = state;
    this.#container.dataset.state = state;
    this.#wrapper.setAttribute("data-paused", String(state === "paused"));

    flag(this.#container, "data-empty", state === "empty");
    flag(this.#container, "data-has-video", this.hasMedia);

    show(this.#empty, state === "empty");
    show(this.#busy, state === "connecting" || state === "buffering");
    show(this.#error, state === "error");
    show(this.#completed, state === "completed");

    const busy = state === "connecting" || state === "buffering";
    flag(this.#statusBar, "data-active", busy);
    if (busy) {
      this.#busyTitle.textContent =
        state === "buffering" ? "Buffering..." : "Connecting to source...";
    }

    if (state === "error" && error) {
      this.#errorDetail.textContent = [error.detail, error.action].filter(Boolean).join(" — ");
      show(this.#retry, error.retryable);
      // Put the keyboard where the only useful action is.
      if (error.retryable) queueMicrotask(() => this.#retry.focus({ preventScroll: true }));
    }

    // The veils are decorative for assistive technology; the live region in
    // the shell carries the announcement.
    for (const el of [this.#empty, this.#busy, this.#error, this.#completed]) {
      el.setAttribute("aria-hidden", "true");
    }
  }

  /** Transient "+10s" / "−10s", for the double-tap gesture. */
  flashSeek(label: string): void {
    const el = document.createElement("div");
    el.className = "seek-flash";
    el.textContent = label;
    this.#wrapper.appendChild(el);
    el.addEventListener("animationend", () => el.remove(), { once: true });
  }

  async toggleFullscreen(): Promise<void> {
    try {
      if (document.fullscreenElement) await document.exitFullscreen();
      else await this.#container.requestFullscreen();
    } catch {
      /* Fullscreen can be refused (iOS Safari, permissions policy). Not fatal. */
    }
  }

  async togglePictureInPicture(): Promise<boolean> {
    const video = this.video;
    if (!("pictureInPictureEnabled" in document) || video.disablePictureInPicture) {
      return false;
    }
    try {
      if (document.pictureInPictureElement) {
        await document.exitPictureInPicture();
        return false;
      }
      await video.requestPictureInPicture();
      return true;
    } catch {
      return false;
    }
  }
}