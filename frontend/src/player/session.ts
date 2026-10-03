/**
 * Playback session identity.
 *
 * The browser owns seek state; the server owns cancellation. All the server
 * needs is a stable session id and a generation that only ever increases, so a
 * request belonging to an abandoned load can never overwrite the live one.
 *
 * The id travels in the URL because a native `<video src>` cannot carry
 * headers, and adding fetch + MediaSource just to set one would be the wrong
 * trade for a few hundred milliseconds of startup.
 */

const HEX = "0123456789abcdef";

function randomId(bytes: number): string {
  const buf = new Uint8Array(bytes);
  crypto.getRandomValues(buf);
  let out = "";
  for (const b of buf) out += HEX[b >> 4]! + HEX[b & 15]!;
  return out;
}

export class Session {
  readonly id: string = randomId(8);
  #generation = 0;

  get generation(): number {
    return this.#generation;
  }

  /** Advance: the previous source load is now obsolete. */
  next(): number {
    this.#generation += 1;
    return this.#generation;
  }
}