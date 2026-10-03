/**
 * The control bar.
 *
 * Only what a viewer actually reaches for: play, timeline, clock, volume,
 * speed, picture-in-picture, fullscreen. Everything else is a distraction
 * around the one thing they came to do.
 */

import { $, flag, show } from "../utils/dom";
import { Timeline } from "./Timeline";

export interface ControlsEvents {
  onToggle: () => void;
  onSeek: (fraction: number) => void;
  onScrub: (active: boolean) => void;
  onVolume: (value: number) => void;
  onMute: () => void;
  onRate: (value: number) => void;
  onFullscreen: () => void;
  onPictureInPicture: () => void;
  onActivity: () => void;
}

/** How long the pointer may rest before the chrome gets out of the way. */
const IDLE_MS = 2600;

export class PlayerControls {
  readonly #overlay: HTMLElement;
  readonly #play: HTMLButtonElement;
  readonly #iPlay: SVGElement;
  readonly #iPause: SVGElement;
  readonly #mute: HTMLButtonElement;
  readonly #iVol: SVGElement;
  readonly #iMuted: SVGElement;
  readonly #volume: HTMLInputElement;
  readonly #rate: HTMLSelectElement;
  readonly #full: HTMLButtonElement;
  readonly #iEnter: SVGElement;
  readonly #iExit: SVGElement;
  readonly #pip: HTMLButtonElement;
  readonly #time: HTMLElement;
  readonly #duration: HTMLElement;
  readonly timeline: Timeline;

  #events: ControlsEvents | null = null;
  #idleTimer: ReturnType<typeof setTimeout> | undefined;

  constructor() {
    this.#overlay = $("#playerOverlay");
    this.#play = $<HTMLButtonElement>("#ctrlPlayPause");
    this.#iPlay = this.#play.querySelector(".icon-play") as SVGElement;
    this.#iPause = this.#play.querySelector(".icon-pause") as SVGElement;
    this.#mute = $<HTMLButtonElement>("#ctrlMute");
    this.#iVol = this.#mute.querySelector(".icon-volume") as SVGElement;
    this.#iMuted = this.#mute.querySelector(".icon-muted") as SVGElement;
    this.#volume = $<HTMLInputElement>("#volumeSlider");
    this.#rate = $<HTMLSelectElement>("#speedSelect");
    this.#full = $<HTMLButtonElement>("#ctrlFullscreen");
    this.#iEnter = this.#full.querySelector(".icon-expand") as SVGElement;
    this.#iExit = this.#full.querySelector(".icon-compress") as SVGElement;
    this.#pip = $<HTMLButtonElement>("#ctrlPip");
    this.#time = $("#currentTime");
    this.#duration = $("#duration");
    this.timeline = new Timeline();

    this.#play.addEventListener("click", () => this.#events?.onToggle());
    this.#volume.addEventListener("input", () =>
      this.#events?.onVolume(Number(this.#volume.value)),
    );
    this.#mute.addEventListener("click", () => this.#events?.onMute());
    this.#rate.addEventListener("change", () => this.#events?.onRate(Number(this.#rate.value)));
    this.#full.addEventListener("click", () => this.#events?.onFullscreen());
    this.#pip.addEventListener("click", () => this.#events?.onPictureInPicture());

    // Any pointer movement over the chrome counts as activity, and so does the
    // stage itself, which the controller reports.
    for (const el of [this.#overlay]) {
      el.addEventListener("pointermove", () => this.wake());
      el.addEventListener("pointerdown", () => this.wake());
    }
  }

  bind(events: ControlsEvents): void {
    this.#events = events;
    this.timeline.bind({ onSeek: events.onSeek, onScrub: events.onScrub });
  }

  /** The overlay element, for state the controller owns directly. */
  get root(): HTMLElement {
    return this.#overlay;
  }

  /** Show the chrome and restart the idle countdown. */
  wake(): void {
    flag(this.#overlay, "data-idle", false);
    this.#events?.onActivity();
    if (this.#idleTimer !== undefined) clearTimeout(this.#idleTimer);
    this.#idleTimer = setTimeout(() => flag(this.#overlay, "data-idle", true), IDLE_MS);
  }

  /** Park the chrome immediately, e.g. when there is nothing loaded. */
  rest(): void {
    if (this.#idleTimer !== undefined) clearTimeout(this.#idleTimer);
    flag(this.#overlay, "data-idle", true);
  }

  setPlaying(playing: boolean): void {
    show(this.#iPause, playing);
    show(this.#iPlay, !playing);
    this.#play.setAttribute("aria-label", playing ? "Pause" : "Play");
    this.#play.title = playing ? "Pause (Space)" : "Play (Space)";
    // Chrome stays put while paused: an idle-fading pause button is hostile.
    flag(this.#overlay, "data-paused", !playing);
  }

  setClock(current: string, total: string): void {
    this.#time.textContent = current;
    this.#duration.textContent = total;
  }

  setVolume(volume: number, muted: boolean): void {
    this.#volume.value = String(volume);
    show(this.#iMuted, muted || volume === 0);
    show(this.#iVol, !muted && volume > 0);
    this.#mute.setAttribute("aria-label", muted ? "Unmute" : "Mute");
    this.#mute.title = muted ? "Unmute (M)" : "Mute (M)";
  }

  setFullscreen(active: boolean): void {
    show(this.#iExit, active);
    show(this.#iEnter, !active);
    this.#full.setAttribute("aria-label", active ? "Exit fullscreen" : "Fullscreen");
  }

  /** PiP only exists where the browser implements it. */
  enablePictureInPicture(supported: boolean): void {
    show(this.#pip, supported);
  }

  setInteractive(enabled: boolean): void {
    this.timeline.setDisabled(!enabled);
    for (const el of [this.#play, this.#mute, this.#rate, this.#full, this.#pip]) {
      el.disabled = !enabled;
    }
  }
}