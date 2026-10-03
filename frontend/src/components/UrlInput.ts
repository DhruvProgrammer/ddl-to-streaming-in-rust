/**
 * The source input.
 *
 * Binds to markup that already exists in the document, so there is no hydration
 * step, no flash of empty UI, and no layout shift when JavaScript arrives.
 */

import { $, flag, show } from "../utils/dom";

export interface UrlInputEvents {
  onSubmit: (url: string) => void;
  /** Fires as the user types, for live validity feedback only. */
  onInput: (url: string, valid: boolean) => void;
}

export class UrlInput {
  readonly #form: HTMLFormElement;
  readonly #field: HTMLElement;
  readonly #input: HTMLInputElement;
  readonly #button: HTMLButtonElement;
  #events: UrlInputEvents | null = null;

  constructor() {
    this.#form = $("#load");
    this.#field = $("#load .field");
    this.#input = $<HTMLInputElement>("#urlInput");
    this.#button = $<HTMLButtonElement>("#playBtn");

    this.#form.addEventListener("submit", (e) => {
      e.preventDefault();
      const value = this.#input.value.trim();
      // Native validation is off (we word our own errors), so guard here too.
      if (value) this.#events?.onSubmit(value);
    });

    this.#input.addEventListener("input", () => {
      const value = this.#input.value.trim();
      this.#events?.onInput(value, isPlausible(value));
    });

    // A pasted link should not require a click to be accepted.
    this.#input.addEventListener("paste", () => {
      queueMicrotask(() => {
        const value = this.#input.value.trim();
        this.#events?.onInput(value, isPlausible(value));
      });
    });
  }

  bind(events: UrlInputEvents): void {
    this.#events = events;
  }

  get value(): string {
    return this.#input.value.trim();
  }

  set value(v: string) {
    this.#input.value = v;
  }

  focus(): void {
    this.#input.focus();
    this.#input.select();
  }

  setValid(valid: boolean): void {
    const filled = this.#input.value.trim().length > 0;
    flag(this.#field, "data-valid", valid && filled);
    flag(this.#field, "data-invalid", !valid && filled);
    this.#input.setAttribute("aria-invalid", String(!valid));
  }

  setBusy(busy: boolean): void {
    flag(this.#field, "data-busy", busy);
    this.#button.disabled = busy;
    this.#button.setAttribute("aria-busy", String(busy));
  }
}

/** Shape check only. Whether the URL is *playable* is the server's answer. */
export function isPlausible(raw: string): boolean {
  if (!raw) return false;
  try {
    const u = new URL(raw);
    return (u.protocol === "http:" || u.protocol === "https:") && u.host.length > 0;
  } catch {
    return false;
  }
}

/** One line under the composer: what happened, and what to do about it. */
export class StatusMessage {
  readonly #el: HTMLElement;

  constructor() {
    this.#el = $("#note");
  }

  info(message: string, action?: string): void {
    this.#render(message, action, "info");
  }

  error(message: string, detail?: string): void {
    this.#render(message, detail, "error");
  }

  clear(): void {
    show(this.#el, false);
    this.#el.textContent = "";
  }

  #render(message: string, detail: string | undefined, tone: "info" | "error"): void {
    this.#el.textContent = "";
    const strong = document.createElement("b");
    strong.textContent = message;
    this.#el.append(strong);
    if (detail) this.#el.append(document.createTextNode(` ${detail}`));
    this.#el.dataset.tone = tone;
    show(this.#el, true);
  }
}