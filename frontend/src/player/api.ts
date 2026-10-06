/**
 * The only network surface the interface needs.
 *
 * `VITE_API_URL` moves the backend without a rebuild; when it is absent the
 * app is same-origin, which is how it is served in production.
 */

import type { ProbeView } from "./types";

const BASE = (import.meta.env.VITE_API_URL ?? "").replace(/\/$/, "");

const ENDPOINT = {
  probe: `${BASE}/api/probe`,
  stream: `${BASE}/api/stream`,
  events: `${BASE}/api/client-events`,
};

/** Wire shape of a probe. Only the fields the interface actually renders. */
interface ProbeWire {
  streamable: boolean;
  content_type: string | null;
  origin_content_type?: string | null;
  content_length: number | null;
  range_supported: boolean;
  container: string;
  warning: string | null;
  reason: string | null;
}

/** Structured failure from the proxy, in the shape its error model defines. */
export interface ApiError {
  code: string;
  message: string;
  reason?: string;
  retryable: boolean;
  user_action: string;
  status: number;
}

export class RequestFailure extends Error {
  readonly detail: ApiError;

  constructor(detail: ApiError) {
    super(detail.message);
    this.name = "RequestFailure";
    this.detail = detail;
  }
}

async function toFailure(res: Response): Promise<RequestFailure> {
  let wire: Partial<ApiError> = {};
  try {
    wire = (await res.json()) as Partial<ApiError>;
  } catch {
    /* a non-JSON error body is itself information */
  }
  return new RequestFailure({
    code: wire.code ?? "UNKNOWN_ERROR",
    message: wire.message ?? "The source could not be reached.",
    ...(wire.reason ? { reason: wire.reason } : {}),
    retryable: wire.retryable === true,
    // The server knows why this particular failure happened; keep its wording
    // unless it sent none.
    user_action: wire.user_action ?? "",
    status: wire.status ?? res.status,
  });
}

/**
 * Inspect a source. This runs *in parallel* with the media request, never in
 * front of it: nothing about starting playback may wait on a probe.
 */
export async function probe(url: string, signal: AbortSignal): Promise<ProbeView> {
  const res = await fetch(ENDPOINT.probe, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ url }),
    signal,
  });
  if (!res.ok) throw await toFailure(res);

  const w = (await res.json()) as ProbeWire;
  return {
    streamable: w.streamable,
    contentType: w.content_type,
    originContentType: w.origin_content_type ?? w.content_type,
    contentLength: w.content_length,
    rangeSupported: w.range_supported,
    container: w.container,
    warning: w.warning ?? null,
    reason: w.reason ?? null,
  };
}

/** The media URL. Session and generation are query parameters by necessity. */
export function streamUrl(url: string, session: string, generation: number): string {
  const p = new URLSearchParams({ url, s: session, g: String(generation) });
  return `${ENDPOINT.stream}?${p.toString()}`;
}

type EventName = "play" | "seek" | "stall" | "error";

/**
 * Report a client-side event. Fire-and-forget and deliberately tiny: this is
 * observability, never a control path, and it must not be able to delay or
 * fail playback.
 */
export function report(name: EventName, ms?: number): void {
  const payload = ms === undefined ? { name } : { name, ms: Math.round(ms) };
  try {
    void fetch(ENDPOINT.events, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(payload),
      keepalive: true,
    }).catch(() => undefined);
  } catch {
    /* telemetry must never throw */
  }
}