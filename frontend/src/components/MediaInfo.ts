/**
 * The compact media row under the player.
 *
 * Deliberately a row of values, not a card. It answers "what am I watching and
 * how big is it" in one line, and gets out of the way.
 */

import { $, show } from "../utils/dom";

export interface MediaView {
  /** e.g. `1080p`, or `—` before the picture has loaded. */
  resolution: string;
  /** e.g. `MP4`. */
  format: string;
  /** e.g. `1:52:13`. */
  duration: string;
}

const IDLE: MediaView = { resolution: "—", format: "—", duration: "—" };

export class MediaInfo {
  readonly #root: HTMLElement;
  readonly #resolution: HTMLElement;
  readonly #format: HTMLElement;
  readonly #duration: HTMLElement;

  constructor() {
    this.#root = $("#mediaInfo");
    this.#resolution = $("#infoResolution");
    this.#format = $("#infoFormat");
    this.#duration = $("#infoDuration");
  }

  render(view: MediaView): void {
    this.#resolution.textContent = view.resolution;
    this.#format.textContent = view.format;
    this.#duration.textContent = view.duration;
    // A row of dashes is noise; show it only once there is something to say.
    show(this.#root, Object.values(view).some((v) => v && v !== "—"));
  }

  clear(): void {
    this.render(IDLE);
    show(this.#root, false);
  }
}