/** Presentation-only formatting. No logic, no side effects. */

/** `0:07`, `12:41`, `1:02:03`. Returns `0:00` for anything unusable. */
export function duration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "0:00";
  const total = Math.floor(seconds);
  const s = total % 60;
  const m = Math.floor(total / 60) % 60;
  const h = Math.floor(total / 3600);
  const mm = h > 0 ? String(m).padStart(2, "0") : String(m);
  return h > 0
    ? `${h}:${mm}:${String(s).padStart(2, "0")}`
    : `${mm}:${String(s).padStart(2, "0")}`;
}

/** Human byte size. `null` when unknown — we never invent one. */
/** Best-effort resolution label from the media itself. */
export function resolution(width: number, height: number): string {
  if (!width || !height) return "—";
  const tall = height > width;
  const short = tall ? width : height;
  const known: Record<number, string> = {
    144: "144p",
    240: "240p",
    360: "360p",
    480: "480p",
    540: "540p",
    720: "720p",
    1080: "1080p",
    1440: "1440p",
    2160: "2160p",
  };
  const exact = known[short];
  if (exact) return exact;
  // Snap to the nearest common tier rather than lying about it.
  const tiers = Object.keys(known).map(Number);
  const nearest = tiers.reduce((a, b) => (Math.abs(b - short) < Math.abs(a - short) ? b : a));
  return short >= nearest ? known[nearest]! : `${short}p`;
}

export function container(label: string | null | undefined): string {
  if (!label) return "—";
  return label.trim().toUpperCase();
}

/** Origin, shown without its query string: tokens live in those. */
export function safeOrigin(raw: string): string {
  try {
    const u = new URL(raw);
    return `${u.protocol}//${u.host}${u.pathname}`;
  } catch {
    return "—";
  }
}