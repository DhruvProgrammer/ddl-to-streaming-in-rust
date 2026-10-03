/**
 * The player state model.
 *
 * Exactly one state at a time, and every visual state in the interface is
 * derived from it. There is no second source of truth for "is it loading".
 */

export type PlayerState =
  | "empty"
  | "connecting"
  | "buffering"
  | "playing"
  | "paused"
  | "completed"
  | "error";

export interface PlayerErrorView {
  /** One sentence, no jargon. */
  message: string;
  /** Secondary, technical, optional. Never a stack trace. */
  detail: string;
  /** What the viewer can do about it. */
  action: string;
  /** Whether offering a retry is honest. */
  retryable: boolean;
}

/** Plain-data view of what the player believes. Never a live object. */
export interface PlayerSnapshot {
  state: PlayerState;
  url: string;
  error: string | null;
  paused: boolean;
  currentTime: number;
  duration: number;
  volume: number;
  muted: boolean;
  rate: number;
  fullscreen: boolean;
}

export interface MediaView {
  /** e.g. `1080p`, or `—` before the picture has loaded. */
  resolution: string;
  /** e.g. `MP4`. */
  format: string;
  /** e.g. `1:52:13`. */
  duration: string;
}

export interface ProbeView {
  streamable: boolean;
  contentType: string | null;
  contentLength: number | null;
  rangeSupported: boolean;
  container: string;
  warning: string | null;
  reason: string | null;
}