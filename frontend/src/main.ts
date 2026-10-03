/**
 * Entry point.
 *
 * The markup is already in the document, so this file only binds behaviour.
 * Nothing is created, appended or hydrated: the first paint is the final
 * layout, which is why there is no loading state to hide behind.
 */

import "./styles/tokens.css";
import "./styles/base.css";
import "./styles/app.css";
import "./styles/player.css";

import { DdlPlayer } from "./player/controller";
import { applyDemoState } from "./player/demo";

const player = new DdlPlayer();

// Expose for the browser tests and for anyone poking at it in a console.
// Read-only in practice; there is no public API contract here.
Object.assign(window, { ddlPlayer: player });

// `?url=` makes a link shareable: hand it to the real player.
const incoming = new URLSearchParams(location.search).get("url");
if (incoming) {
  void player.loadFromQuery(incoming);
}

// `?state=` renders one of the interface states on demand, so the design can be
// reviewed without breaking a source. Off unless asked for.
applyDemoState(player);