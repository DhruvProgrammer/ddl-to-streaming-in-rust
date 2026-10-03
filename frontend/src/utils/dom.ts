/**
 * The smallest DOM helpers that make the rest readable.
 *
 * No framework: the interface is one video element and roughly twenty nodes
 * around it, so a 40-line `h()` costs less than a runtime and produces markup
 * you can read in devtools.
 */

type Child = Node | string | number | null | undefined | false;

export interface Attrs {
  class?: string;
  text?: string;
  html?: string;
  [key: string]: string | number | boolean | null | undefined | EventListener;
}

const SVG_NS = "http://www.w3.org/2000/svg";
const SVG_TAGS = new Set(["svg", "path", "circle", "g", "rect", "line", "use"]);

/** Create an element. `on*` keys become listeners; `false`/`null` are dropped. */
export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  apply(el, attrs);
  append(el, children);
  return el;
}

function apply(el: Element, attrs: Attrs): void {
  for (const [key, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue;
    if (key.startsWith("on") && typeof value === "function") {
      el.addEventListener(key.slice(2).toLowerCase(), value as EventListener);
    } else if (key === "class") {
      el.className = String(value);
    } else if (key === "text") {
      el.textContent = String(value);
    } else if (key === "html") {
      el.innerHTML = String(value);
    } else if (value === true) {
      el.setAttribute(key, "");
    } else {
      el.setAttribute(key, String(value));
    }
  }
}

function append(el: Element, children: Child[]): void {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    el.append(typeof child === "string" || typeof child === "number"
      ? document.createTextNode(String(child))
      : child);
  }
}

/** SVG needs its own namespace or the browser renders nothing. */
export function svg(tag: string, attrs: Attrs = {}, ...children: Child[]): SVGElement {
  const el = document.createElementNS(SVG_NS, tag) as SVGElement;
  for (const [key, value] of Object.entries(attrs)) {
    if (value === null || value === undefined || value === false) continue;
    el.setAttribute(key, String(value));
  }
  for (const child of children) {
    if (typeof child === "string") el.append(document.createTextNode(child));
    else if (child) el.append(child as Node);
  }
  return el;
}

/** Anything we may need to hide or flag. SVG carries `hidden` too. */
type Toggleable = HTMLElement | SVGElement;

export function show(el: Toggleable, visible: boolean): void {
  // The `hidden` attribute rather than the IDL property, because `hidden` is a
  // global attribute and SVG elements carry it too.
  if (visible) el.removeAttribute("hidden");
  else el.setAttribute("hidden", "");
}

/** Toggle a data attribute; used for the small number of stateful elements. */
export function flag(el: Toggleable, name: string, on: boolean): void {
  if (on) el.setAttribute(name, "");
  else el.removeAttribute(name);
}

export function $<T extends Element = HTMLElement>(root: ParentNode, sel: string): T;
export function $<T extends Element = HTMLElement>(sel: string): T;
export function $<T extends Element = HTMLElement>(a: ParentNode | string, b?: string): T {
  const root = b === undefined ? document : (a as ParentNode);
  const sel = b === undefined ? (a as string) : b;
  const el = root.querySelector<T>(sel);
  if (!el) throw new Error(`missing element: ${sel}`);
  return el;
}

export function isSVG(tag: string): boolean {
  return SVG_TAGS.has(tag);
}

/** Trailing-edge debounce, for anything that could trigger a request. */
export function debounce<A extends unknown[]>(
  fn: (...args: A) => void,
  ms: number,
): (...args: A) => void {
  let timer: ReturnType<typeof setTimeout> | undefined;
  return (...args: A) => {
    if (timer !== undefined) clearTimeout(timer);
    timer = setTimeout(() => fn(...args), ms);
  };
}