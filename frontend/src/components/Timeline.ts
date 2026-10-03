/**
 * The scrub bar.
 *
 * The track, the buffered range, the played range and the thumb are drawn as
 * spans, because a range input cannot express them. The *control* is a native
 * `<input type="range">` laid invisibly over the top: it is what makes pointer
 * drag, touch, keyboard and assistive technology work without reimplementing
 * any of them, and it is the only thing that receives pointer events.
 */

import { $ } from "../utils/dom";

export interface TimelineEvents {
  /** Fraction 0..1 of where the user committed a seek. */
  onSeek: (fraction: number) => void;
  /** True while the user is dragging, so the controller can stop following. */
  onScrub: (active: boolean) => void;
}

export class Timeline {
  readonly #input: HTMLInputElement;
  readonly #played: HTMLElement;
  readonly #buffered: HTMLElement;
  readonly #thumb: HTMLElement;
  #events: TimelineEvents | null = null;
  #scrubbing = false;

  constructor() {
    this.#input = $<HTMLInputElement>("#seek");
    this.#played = $("#timelineProgress");
    this.#buffered = $("#timelineBuffered");
    this.#thumb = $("#timelineThumb");

    // `change` is the reliable commit signal: it fires on pointer release AND
    // on keyboard use of the range input, whereas pointer events never fire for
    // anyone not using a pointer. Gating on a prior pointerdown would leave
    // keyboard and assistive-technology seeking silently broken.
    const commit = (): void => {
      this.#endScrub();
      this.#events?.onSeek(Number(this.#input.value) / 1000);
    };

    this.#input.addEventListener("pointerdown", () => this.#beginScrub());
    this.#input.addEventListener("pointerup", commit);
    this.#input.addEventListener("pointercancel", () => this.#endScrub());
    this.#input.addEventListener("change", commit);
  }

  bind(events: TimelineEvents): void {
    this.#events = events;
  }

  get scrubbing(): boolean {
    return this.#scrubbing;
  }

  #beginScrub(): void {
    this.#scrubbing = true;
    this.#events?.onScrub(true);
  }

  #endScrub(): void {
    if (!this.#scrubbing) return;
    this.#scrubbing = false;
    this.#events?.onScrub(false);
  }

  /** Follow playback. Ignored while the user is dragging the thumb. */
  sync(current: number, total: number): void {
    if (this.#scrubbing) return;
    const fraction = total > 0 ? Math.min(1, Math.max(0, current / total)) : 0;
    this.#input.value = String(Math.round(fraction * 1000));
    this.#paint(fraction);
  }

  /** Draw the buffered ranges. `TimeRanges` is live; read it immediately. */
  syncBuffered(video: HTMLVideoElement): void {
    const total = video.duration;
    if (!Number.isFinite(total) || total <= 0 || video.buffered.length === 0) {
      this.#buffered.style.inlineSize = "0%";
      return;
    }
    // With one contiguous range we can draw exactly; with gaps, cover the
    // furthest point reached, which is what a viewer perceives as "loaded".
    let end = 0;
    for (let i = 0; i < video.buffered.length; i += 1) {
      if (video.buffered.start(i) <= 0.01) end = Math.max(end, video.buffered.end(i));
    }
    this.#buffered.style.inlineSize = `${Math.min(1, end / total) * 100}%`;
  }

  setDisabled(disabled: boolean): void {
    this.#input.disabled = disabled;
  }

  reset(): void {
    this.#input.value = "0";
    this.#paint(0);
    this.#buffered.style.inlineSize = "0%";
  }

  #paint(fraction: number): void {
    const pct = `${fraction * 100}%`;
    this.#played.style.inlineSize = pct;
    this.#thumb.style.insetInlineStart = pct;
  }
}