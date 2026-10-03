/**
 * State preview.
 *
 * A streaming interface has states that are hard to reach on demand — a stalled
 * origin, a 403, the moment before the first byte. Reviewing them should not
 * require breaking something, so `?state=…` renders one directly.
 *
 * This is a design-review affordance, not a feature of the player: with no
 * parameter it does nothing at all, and playback never consults it.
 */

import type { DdlPlayer } from "./controller";
import type { PlayerErrorView, PlayerState } from "./types";

const STATES: Record<string, PlayerState> = {
  empty: "empty",
  loading: "connecting",
  connecting: "connecting",
  buffering: "buffering",
  playing: "playing",
  paused: "paused",
  completed: "completed",
  error: "error",
};

const SAMPLE_ERROR: PlayerErrorView = {
  message: "This source does not support byte-range requests.",
  detail: "HTTP 416",
  action: "Seeking will restart the download instead of jumping.",
  retryable: true,
};

const ALTERNATE_ERROR: PlayerErrorView = {
  message: "The source refused access.",
  detail: "HTTP 403",
  action: "The link may have expired or require a referrer.",
  retryable: true,
};

export function applyDemoState(player: DdlPlayer): void {
  const key = new URLSearchParams(location.search).get("state")?.toLowerCase();
  if (!key) return;

  const state = STATES[key];
  if (!state) return;

  const error =
    state === "error"
      ? new URLSearchParams(location.search).get("error") === "range"
        ? SAMPLE_ERROR
        : ALTERNATE_ERROR
      : undefined;

  player.preview(state, error);
}

/** Every state this module can render, for the review UI and the tests. */
export const PREVIEW_STATES = Object.keys(STATES);