/**
 * Turning failures into something a person can act on.
 *
 * Rules: one sentence first, technical detail second, never a stack trace, and
 * never the word "error" shouted at anybody.
 */

import { RequestFailure, type ApiError } from "./api";
import type { PlayerErrorView } from "./types";

/** Our own codes, where we know better wording than the server's default. */
const BY_CODE: Record<string, { message: string; action: string }> = {
  RANGE_NOT_SUPPORTED: {
    message: "This source does not support byte-range requests.",
    action: "Seeking will restart the download instead of jumping.",
  },
  MEDIA_NOT_SUPPORTED: {
    message: "This browser cannot play that container.",
    action: "MP4 (H.264) and WebM work everywhere.",
  },
  INVALID_CONTENT_TYPE: {
    message: "The source did not return a media file.",
    action: "Check the link points at the video, not a download page.",
  },
  HTTP_403: {
    message: "The source refused access.",
    action: "The link may have expired or require a referrer.",
  },
  HTTP_404: {
    message: "That file is not there any more.",
    action: "Check the link still points at a file.",
  },
  RATE_LIMITED: {
    message: "The source is rate limiting requests.",
    action: "Wait a moment, then try again.",
  },
  TOO_MANY_REDIRECTS: {
    message: "The link redirected too many times.",
    action: "It is probably part of a redirect loop.",
  },
  UNSUPPORTED_PROTOCOL: {
    message: "Only http and https links can be played.",
    action: "Check the link starts with http:// or https://",
  },
  INVALID_URL: {
    message: "That does not look like a valid URL.",
    action: "Paste the full link, including https://",
  },
};

/** `MediaError` codes, which are numbers and explain nothing on their own. */
const MEDIA: Record<number, { message: string; action: string }> = {
  1: { message: "Loading this source was cancelled.", action: "Press play to try again." },
  2: {
    message: "The connection dropped mid-stream.",
    action: "The player will reconnect; check the source if it keeps happening.",
  },
  3: { message: "The source sent a corrupted response.", action: "Try a different mirror." },
  4: {
    message: "This browser cannot play that format.",
    action: "Convert it to MP4 (H.264) or WebM.",
  },
};

export function fromApi(e: ApiError): PlayerErrorView {
  const known = BY_CODE[e.code];
  const detailBits: string[] = [];
  if (e.status) detailBits.push(`HTTP ${e.status}`);
  if (e.code && !known) detailBits.push(e.code);
  return {
    message: known?.message ?? e.message,
    // The server wrote this action for *this* failure — it knows whether the
    // link expired, whether the origin compresses its downloads, whether it
    // refused a range. Our table has generic wording for the same code, and
    // substituting it is how a viewer is told to convert a file that was never
    // the problem. Ours is only the fallback.
    action: e.user_action || known?.action || "Check the source, then try again.",
    detail: detailBits.join(" · "),
    retryable: e.retryable || known === undefined,
  };
}

export function fromMedia(code: number | undefined | null): PlayerErrorView {
  const known = code !== undefined && code !== null ? MEDIA[code] : undefined;
  return {
    message: known?.message ?? "Playback stopped unexpectedly.",
    action: known?.action ?? "Press play to try again.",
    detail: known ? `MediaError ${code}` : "",
    retryable: code !== 3 && code !== 4,
  };
}

export function fromThrown(e: unknown): PlayerErrorView {
  if (e instanceof RequestFailure) return fromApi(e.detail);
  if (e instanceof DOMException && e.name === "NotAllowedError") {
    return {
      message: "The browser blocked autoplay.",
      action: "Press play to start.",
      detail: "",
      retryable: true,
    };
  }
  return {
    message: "Playback could not start.",
    action: "Press play, and check the source link if it still will not run.",
    detail: "",
    retryable: true,
  };
}

/** Inline validation, before a request is spent on a typo. */
export function validateInput(raw: string): PlayerErrorView | null {
  const value = raw.trim();
  if (!value) {
    return {
      message: "Paste a direct video URL first.",
      action: "It should start with http:// or https://",
      detail: "",
      retryable: false,
    };
  }
  let parsed: URL;
  try {
    parsed = new URL(value);
  } catch {
    return {
      message: "That does not look like a valid URL.",
      action: "Paste the full link, including https://",
      detail: "",
      retryable: false,
    };
  }
  if (parsed.protocol !== "http:" && parsed.protocol !== "https:") {
    return {
      message: "Only http and https links can be played.",
      action: `This link uses ${parsed.protocol.replace(":", "")}.`,
      detail: "",
      retryable: false,
    };
  }
  return null;
}