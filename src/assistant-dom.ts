// Small DOM and storage helpers shared by the Assistant page modules. Everything is built with
// createElement and text nodes: no markup strings, so model output can never become HTML.

export function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  className = "",
  ...children: (Node | string | null | false)[]
): HTMLElementTagNameMap[K] {
  const element = document.createElement(tag);
  if (className) element.className = className;
  for (const child of children) {
    if (child === null || child === false) continue;
    element.append(typeof child === "string" ? document.createTextNode(child) : child);
  }
  return element;
}

export function button(
  label: string | Node,
  className: string,
  onClick: () => void,
): HTMLButtonElement {
  const element = h("button", className, label);
  element.type = "button";
  element.addEventListener("click", onClick);
  return element;
}

const SVG_NS = "http://www.w3.org/2000/svg";

/** A 24x24 stroke icon built without markup strings. */
export function svgIcon(paths: string[], circles: [number, number, number][] = [], size = 16): SVGElement {
  const svg = document.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", "icon");
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", "1.75");
  svg.setAttribute("stroke-linecap", "round");
  svg.setAttribute("stroke-linejoin", "round");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("focusable", "false");
  for (const d of paths) {
    const path = document.createElementNS(SVG_NS, "path");
    path.setAttribute("d", d);
    svg.appendChild(path);
  }
  for (const [cx, cy, r] of circles) {
    const circle = document.createElementNS(SVG_NS, "circle");
    circle.setAttribute("cx", String(cx));
    circle.setAttribute("cy", String(cy));
    circle.setAttribute("r", String(r));
    svg.appendChild(circle);
  }
  return svg;
}

export const slidersIcon = () =>
  svgIcon(["M21 4h-7", "M10 4H3", "M21 12h-9", "M8 12H3", "M21 20h-5", "M12 20H3", "M14 2v4", "M8 10v4", "M16 18v4"]);
export const closeIcon = () => svgIcon(["M18 6 6 18", "M6 6l12 12"]);
export const paperclipIcon = () =>
  svgIcon(["M21 11.5l-8.6 8.6a5 5 0 0 1-7.1-7.1l8.6-8.6a3.4 3.4 0 0 1 4.8 4.8l-8.6 8.6a1.7 1.7 0 0 1-2.4-2.4l7.9-7.9"]);

export function errorText(error: unknown): string {
  if (typeof error === "string") return error;
  if (error instanceof Error) return error.message;
  return String(error);
}

export function readStored(key: string): string | null {
  try {
    return window.localStorage.getItem(key);
  } catch {
    return null;
  }
}

export function writeStored(key: string, value: string): void {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    // Storage can be blocked; the choice then only lasts until the app closes.
  }
}

export function removeStored(key: string): void {
  try {
    window.localStorage.removeItem(key);
  } catch {
    // As above.
  }
}

/** A collapsible panel section (`<details>`), open by default. */
export function panelSection(title: string, ...body: (Node | null | false)[]): HTMLDetailsElement {
  const section = h("details", "assistant-panel-section", h("summary", "assistant-panel-summary", title), ...body);
  section.open = true;
  return section;
}

/** Megabytes, at least 1, for download sizes and progress. */
export function megabytes(bytes: number): string {
  return String(Math.max(1, Math.round(bytes / (1024 * 1024))));
}
