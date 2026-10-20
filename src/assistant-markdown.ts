// A small, safe Markdown subset for Assistant answers.
//
// Model output is untrusted. It never becomes HTML here: `parseMarkdown` turns text into a
// tree of plain data (no DOM), and `renderMarkdown` builds elements from that tree with
// createElement + textContent only. Raw HTML, images and unsupported syntax stay literal text.
// Links render as inert buttons and open in the system browser only for http(s) URLs.
// Pipe tables (the GitHub style) are supported; their cells are inline text like any paragraph.

import { open as openExternal } from "@tauri-apps/api/shell";

export type MdInline =
  | { type: "text"; text: string }
  | { type: "code"; text: string }
  | { type: "strong"; children: MdInline[] }
  | { type: "em"; children: MdInline[] }
  /** `openable` is true only for http(s) URLs; anything else is shown as plain, inert text. */
  | { type: "link"; children: MdInline[]; url: string; openable: boolean };

export type MdListItem = {
  inline: MdInline[];
  /** One nested list level. */
  sublist: Extract<MdBlock, { type: "list" }> | null;
};

export type MdBlock =
  | { type: "paragraph"; inline: MdInline[] }
  | { type: "heading"; level: number; inline: MdInline[] }
  /** `closed` is false for a fence still being streamed (or never closed). */
  | { type: "code"; lang: string; text: string; closed: boolean }
  | { type: "list"; ordered: boolean; start: number; items: MdListItem[] }
  | { type: "quote"; blocks: MdBlock[] }
  /** `align`, `header` and every row have one entry per column. */
  | { type: "table"; align: MdTableAlign[]; header: MdInline[][]; rows: MdInline[][][] }
  | { type: "hr" };

export type MdTableAlign = "left" | "center" | "right" | null;

type ListBlock = Extract<MdBlock, { type: "list" }>;

const MAX_INLINE_DEPTH = 4;
const MAX_BLOCK_DEPTH = 3;
/** Wider tables, and rows beyond the limit, stay ordinary text: a table is for reading, not storing. */
const MAX_TABLE_COLUMNS = 24;
const MAX_TABLE_ROWS = 500;
/** How far ahead a closing emphasis marker is searched for. Keeps unmatched markers cheap. */
const EMPHASIS_WINDOW = 1000;
/** Longest link target that is parsed; longer ones stay literal text. */
const MAX_URL_LENGTH = 2048;
const OPENABLE_URL = /^https?:\/\//i;

const FENCE_OPEN = /^ {0,3}(`{3,}|~{3,})[ \t]*([^\s`]*)/;
const HEADING = /^ {0,3}(#{1,6})(?:[ \t]+(.*?))?[ \t]*$/;
const HR = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
const QUOTE = /^ {0,3}>[ \t]?(.*)$/;
const LIST_ITEM = /^( *)([-*+]|\d{1,9}[.)])[ \t]+(.*)$/;

// ---------------------------------------------------------------------------
// Inline
// ---------------------------------------------------------------------------

const BARE_URL = /https?:\/\/[^\s<>"'`]+/iy;

function isWordChar(char: string | undefined): boolean {
  return char !== undefined && /[\p{L}\p{N}]/u.test(char);
}

function isSpace(char: string | undefined): boolean {
  return char === undefined || /\s/.test(char);
}

/** Trailing punctuation that belongs to the sentence, not the URL. */
function trimUrlEnd(url: string): string {
  let end = url.length;
  while (end > 0) {
    const last = url[end - 1];
    if (".,;:!?*_~'\"]}".includes(last)) {
      end -= 1;
    } else if (last === ")") {
      const body = url.slice(0, end);
      const opens = body.split("(").length - 1;
      const closes = body.split(")").length - 1;
      if (closes > opens) end -= 1;
      else break;
    } else {
      break;
    }
  }
  return url.slice(0, end);
}

/** Position of the `]` that closes the `[` at `open`, or -1. No nesting. */
function findLinkTextEnd(src: string, open: number): number {
  for (let i = open + 1; i < src.length; i += 1) {
    const char = src[i];
    if (char === "\\") i += 1;
    else if (char === "]") return i;
    else if (char === "[" || char === "\n") return -1;
  }
  return -1;
}

/** Parses `(url "optional title")` at `start` (the `(`). Returns the URL and the index after `)`. */
function parseLinkTarget(src: string, start: number): { url: string; end: number } | null {
  if (src[start] !== "(") return null;
  let depth = 1;
  let i = start + 1;
  while (i < src.length && /[ \t]/.test(src[i])) i += 1;
  const urlStart = i;
  const scanEnd = Math.min(src.length, urlStart + MAX_URL_LENGTH);
  for (; i < scanEnd; i += 1) {
    const char = src[i];
    if (char === "\\") {
      i += 1;
    } else if (char === "(") {
      depth += 1;
    } else if (char === ")") {
      depth -= 1;
      if (depth === 0) break;
    } else if (char === "\n") {
      return null;
    } else if (/\s/.test(char) && depth === 1) {
      break;
    }
  }
  if (i >= scanEnd) return null;
  const rawUrl = src.slice(urlStart, i);
  if (src[i] !== ")") {
    // Whitespace ended the URL: skip an optional title, then require `)`.
    const close = src.indexOf(")", i);
    if (close === -1 || src.slice(i, close).includes("\n")) return null;
    i = close;
  }
  let url = rawUrl;
  if (url.startsWith("<") && url.endsWith(">")) url = url.slice(1, -1);
  return { url, end: i + 1 };
}

/**
 * Finds the closing `marker` for an emphasis span opening at `from`; -1 when there is none.
 * `searched` remembers, per marker, how far a failed search already looked, so a run of
 * unmatched markers is scanned once instead of once per marker.
 */
function findEmphasisEnd(src: string, from: number, marker: string, searched: Map<string, number>): number {
  const limit = Math.min(src.length, from + EMPHASIS_WINDOW);
  let at = Math.max(from, searched.get(marker) ?? 0);
  while (at < limit) {
    const found = src.indexOf(marker, at);
    if (found === -1 || found >= limit) {
      searched.set(marker, found === -1 ? src.length : limit);
      return -1;
    }
    if (src[found - 1] === "\\") {
      at = found + 1;
      continue;
    }
    const before = src[found - 1];
    const after = src[found + marker.length];
    const underscore = marker[0] === "_";
    // A closer follows text, not a space, and for `_` does not sit inside a word.
    if (found > from && !isSpace(before) && !(underscore && isWordChar(after))) {
      // A single marker followed by itself is half of a longer run, not a closer.
      if (marker.length === 1 && after === marker) {
        at = found + 2;
        continue;
      }
      return found;
    }
    at = found + marker.length;
  }
  searched.set(marker, limit);
  return -1;
}

function pushText(out: MdInline[], text: string): void {
  if (!text) return;
  const last = out[out.length - 1];
  if (last && last.type === "text") last.text += text;
  else out.push({ type: "text", text });
}

function parseInline(src: string, depth = 0, allowLinks = true): MdInline[] {
  const out: MdInline[] = [];
  // Backtick run lengths already known to have no closer, so stray backticks stay linear.
  const unclosed = new Set<number>();
  const searched = new Map<string, number>();
  let i = 0;
  while (i < src.length) {
    const char = src[i];

    if (char === "\\" && i + 1 < src.length && /[\\`*_{}[\]()#+\-.!<>~|]/.test(src[i + 1])) {
      pushText(out, src[i + 1]);
      i += 2;
      continue;
    }

    if (char === "`") {
      let run = 1;
      while (src[i + run] === "`") run += 1;
      const fence = "`".repeat(run);
      let close = unclosed.has(run) ? -1 : src.indexOf(fence, i + run);
      while (close !== -1 && src[close + run] === "`") {
        // The closing run must be exactly as long as the opening one.
        let longer = run;
        while (src[close + longer] === "`") longer += 1;
        close = src.indexOf(fence, close + longer);
      }
      if (close !== -1) {
        let code = src.slice(i + run, close);
        if (code.length > 2 && code.startsWith(" ") && code.endsWith(" ") && code.trim()) code = code.slice(1, -1);
        out.push({ type: "code", text: code });
        i = close + run;
      } else {
        unclosed.add(run);
        pushText(out, fence);
        i += run;
      }
      continue;
    }

    if (char === "!" && src[i + 1] === "[") {
      // Images are never loaded: keep the source (alt and URL) as visible text.
      const textEnd = findLinkTextEnd(src, i + 1);
      const target = textEnd === -1 ? null : parseLinkTarget(src, textEnd + 1);
      if (target) {
        pushText(out, src.slice(i, target.end));
        i = target.end;
      } else {
        pushText(out, "!");
        i += 1;
      }
      continue;
    }

    if (char === "[" && allowLinks) {
      const textEnd = findLinkTextEnd(src, i);
      const target = textEnd === -1 ? null : parseLinkTarget(src, textEnd + 1);
      if (target && target.url) {
        const label = src.slice(i + 1, textEnd);
        const children = depth < MAX_INLINE_DEPTH ? parseInline(label, depth + 1, false) : [{ type: "text" as const, text: label }];
        out.push({ type: "link", children, url: target.url, openable: OPENABLE_URL.test(target.url) });
        i = target.end;
        continue;
      }
    }

    if ((char === "h" || char === "H") && allowLinks && !isWordChar(src[i - 1])) {
      BARE_URL.lastIndex = i;
      const match = BARE_URL.exec(src);
      if (match) {
        const url = trimUrlEnd(match[0]);
        if (url.length > "https://".length) {
          out.push({ type: "link", children: [{ type: "text", text: url }], url, openable: true });
          i += url.length;
          continue;
        }
      }
    }

    if ((char === "*" || char === "_") && depth < MAX_INLINE_DEPTH) {
      const double = src[i + 1] === char;
      const marker = double ? char + char : char;
      const afterOpen = src[i + marker.length];
      const opens =
        !isSpace(afterOpen) && afterOpen !== char && (char === "*" || !isWordChar(src[i - 1]));
      if (opens) {
        const end = findEmphasisEnd(src, i + marker.length, marker, searched);
        if (end !== -1) {
          const children = parseInline(src.slice(i + marker.length, end), depth + 1, allowLinks);
          out.push({ type: double ? "strong" : "em", children });
          i = end + marker.length;
          continue;
        }
      }
      // Unmatched marker (for example a half-written `**bold` while streaming): literal text.
      pushText(out, marker);
      i += marker.length;
      continue;
    }

    // Plain run: take everything up to the next character that could start syntax.
    let next = i + 1;
    while (next < src.length && !"\\`![*_hH".includes(src[next])) next += 1;
    pushText(out, src.slice(i, next));
    i = next;
  }
  return out;
}

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

function startsBlock(line: string): boolean {
  return FENCE_OPEN.test(line) || (HEADING.test(line) && /^ {0,3}#/.test(line)) || HR.test(line) || QUOTE.test(line) || LIST_ITEM.test(line);
}

function parseBlocks(lines: string[], depth: number): MdBlock[] {
  const blocks: MdBlock[] = [];
  let i = 0;
  while (i < lines.length) {
    const line = lines[i];

    if (!line.trim()) {
      i += 1;
      continue;
    }

    const fence = FENCE_OPEN.exec(line);
    if (fence) {
      const marker = fence[1];
      const body: string[] = [];
      let closed = false;
      i += 1;
      while (i < lines.length) {
        const trimmed = lines[i].trim();
        if (trimmed.length >= marker.length && trimmed[0] === marker[0] && /^(`+|~+)$/.test(trimmed)) {
          closed = true;
          i += 1;
          break;
        }
        body.push(lines[i]);
        i += 1;
      }
      blocks.push({ type: "code", lang: fence[2], text: body.join("\n"), closed });
      continue;
    }

    if (HR.test(line)) {
      blocks.push({ type: "hr" });
      i += 1;
      continue;
    }

    const heading = /^ {0,3}#/.test(line) ? HEADING.exec(line) : null;
    if (heading) {
      const text = (heading[2] ?? "").replace(/[ \t]+#+$/, "");
      blocks.push({ type: "heading", level: heading[1].length, inline: parseInline(text) });
      i += 1;
      continue;
    }

    if (QUOTE.test(line)) {
      const inner: string[] = [];
      while (i < lines.length) {
        const quoted = QUOTE.exec(lines[i]);
        if (!quoted) break;
        inner.push(quoted[1]);
        i += 1;
      }
      blocks.push({
        type: "quote",
        blocks: depth < MAX_BLOCK_DEPTH ? parseBlocks(inner, depth + 1) : [{ type: "paragraph", inline: [{ type: "text", text: inner.join("\n") }] }],
      });
      continue;
    }

    if (LIST_ITEM.test(line)) {
      const consumed = parseList(lines, i);
      blocks.push(consumed.block);
      i = consumed.next;
      continue;
    }

    const table = parseTable(lines, i);
    if (table) {
      blocks.push(table.block);
      i = table.next;
      continue;
    }

    const paragraph: string[] = [];
    while (i < lines.length && lines[i].trim() && (paragraph.length === 0 || !(startsBlock(lines[i]) || startsTable(lines, i)))) {
      paragraph.push(lines[i].replace(/^[ \t]+/, ""));
      i += 1;
    }
    blocks.push({ type: "paragraph", inline: parseInline(paragraph.join("\n")) });
  }
  return blocks;
}

const TABLE_DELIMITER_CELL = /^:?-+:?$/;

/**
 * Splits one table row into its cell texts. Cells end at a pipe that is not escaped; one pipe at
 * each end of the row is decoration. `\|` is a literal pipe inside a cell.
 */
function splitTableRow(line: string): string[] {
  let src = line.trim();
  if (src.startsWith("|")) src = src.slice(1);
  const trailingPipe = src.endsWith("|") && !src.endsWith("\\|");
  const cells: string[] = [];
  let cell = "";
  for (let i = 0; i < src.length; i += 1) {
    const char = src[i];
    if (char === "\\" && src[i + 1] === "|") {
      cell += "|";
      i += 1;
    } else if (char === "|") {
      cells.push(cell.trim());
      cell = "";
    } else {
      cell += char;
    }
  }
  if (!trailingPipe) cells.push(cell.trim());
  return cells;
}

/** The column alignments of a delimiter row (`| --- | :-: |`), or null when `line` is not one. */
function parseTableDelimiter(line: string): MdTableAlign[] | null {
  if (!line.includes("|") || !/^[\s|:-]+$/.test(line) || !line.includes("-")) return null;
  const cells = splitTableRow(line);
  if (cells.length === 0 || cells.length > MAX_TABLE_COLUMNS) return null;
  const align: MdTableAlign[] = [];
  for (const cell of cells) {
    if (!TABLE_DELIMITER_CELL.test(cell)) return null;
    const left = cell.startsWith(":");
    const right = cell.endsWith(":");
    align.push(left && right ? "center" : right ? "right" : left ? "left" : null);
  }
  return align;
}

/** A header row followed by a delimiter row with the same number of columns. */
function tableHead(lines: string[], from: number): { header: string[]; align: MdTableAlign[] } | null {
  const head = lines[from];
  if (from + 1 >= lines.length || !head.includes("|")) return null;
  const align = parseTableDelimiter(lines[from + 1]);
  if (!align) return null;
  const header = splitTableRow(head);
  return header.length === align.length ? { header, align } : null;
}

function startsTable(lines: string[], from: number): boolean {
  return tableHead(lines, from) !== null;
}

function parseTable(lines: string[], from: number): { block: MdBlock; next: number } | null {
  const head = tableHead(lines, from);
  if (!head) return null;
  const columns = head.align.length;
  const rows: MdInline[][][] = [];
  let i = from + 2;
  // Rows continue until a blank line, a line without a pipe, or another block. A row still being
  // streamed may have fewer cells than the header: it is padded, and extra cells are dropped.
  while (i < lines.length && rows.length < MAX_TABLE_ROWS && lines[i].trim() && lines[i].includes("|") && !startsBlock(lines[i])) {
    const cells = splitTableRow(lines[i]);
    const row: MdInline[][] = [];
    for (let column = 0; column < columns; column += 1) row.push(parseInline(cells[column] ?? ""));
    rows.push(row);
    i += 1;
  }
  return {
    block: { type: "table", align: head.align, header: head.header.map((cell) => parseInline(cell)), rows },
    next: i,
  };
}

function newList(marker: string): ListBlock {
  const ordered = /\d/.test(marker);
  return { type: "list", ordered, start: ordered ? parseInt(marker, 10) : 1, items: [] };
}

/** Parses a list starting at `from`. Items one level deeper become the previous item's sublist. */
function parseList(lines: string[], from: number): { block: ListBlock; next: number } {
  const first = LIST_ITEM.exec(lines[from])!;
  const baseIndent = first[1].length;
  const list = newList(first[2]);
  let i = from;
  let current: MdListItem | null = null;
  let currentText: string[] = [];
  let subText: { marker: string; lines: string[] }[] = [];

  const finishItem = () => {
    if (!current) return;
    current.inline = parseInline(currentText.join("\n"));
    if (subText.length > 0) {
      const sub = newList(subText[0].marker);
      for (const entry of subText) {
        sub.items.push({ inline: parseInline(entry.lines.join("\n")), sublist: null });
      }
      current.sublist = sub;
    }
    list.items.push(current);
    current = null;
    currentText = [];
    subText = [];
  };

  while (i < lines.length) {
    const line = lines[i];
    if (!line.trim()) {
      // A blank line continues the list only when another list item follows.
      let peek = i + 1;
      while (peek < lines.length && !lines[peek].trim()) peek += 1;
      const following = peek < lines.length ? LIST_ITEM.exec(lines[peek]) : null;
      if (following && following[1].length >= baseIndent && (/\d/.test(following[2]) === list.ordered || following[1].length > baseIndent + 1)) {
        i = peek;
        continue;
      }
      break;
    }
    const item = LIST_ITEM.exec(line);
    if (item && !HR.test(line)) {
      const indent = item[1].length;
      if (indent <= baseIndent + 1) {
        if (/\d/.test(item[2]) !== list.ordered && indent === baseIndent) break;
        finishItem();
        current = { inline: [], sublist: null };
        currentText = [item[3]];
      } else if (current) {
        // Deeper than the parent item: one nested level; anything deeper is flattened into it.
        subText.push({ marker: item[2], lines: [item[3]] });
      } else {
        break;
      }
      i += 1;
      continue;
    }
    if (!current || startsBlock(line) || !/^[ \t]/.test(line)) {
      // An unindented line that is not an item ends the list (and a fence/heading/quote does too).
      break;
    }
    // Indented continuation text.
    if (subText.length > 0) subText[subText.length - 1].lines.push(line.trim());
    else currentText.push(line.trim());
    i += 1;
  }
  finishItem();
  return { block: list, next: i };
}

export function parseMarkdown(src: string): MdBlock[] {
  return parseBlocks(src.replace(/\r\n?/g, "\n").split("\n"), 0);
}

// ---------------------------------------------------------------------------
// Plain text (Speak and Copy)
// ---------------------------------------------------------------------------

function inlineToPlain(nodes: MdInline[]): string {
  let out = "";
  for (const node of nodes) {
    if (node.type === "text" || node.type === "code") out += node.text;
    else out += inlineToPlain(node.children);
  }
  return out;
}

function listToPlain(list: ListBlock, indent: string): string {
  const lines: string[] = [];
  list.items.forEach((item, index) => {
    const label = list.ordered ? `${list.start + index}. ` : "";
    lines.push(indent + label + inlineToPlain(item.inline));
    if (item.sublist) lines.push(listToPlain(item.sublist, indent + "  "));
  });
  return lines.join("\n");
}

function blocksToPlain(blocks: MdBlock[]): string {
  const parts: string[] = [];
  for (const block of blocks) {
    switch (block.type) {
      case "paragraph":
      case "heading":
        parts.push(inlineToPlain(block.inline));
        break;
      case "code":
        parts.push(block.text);
        break;
      case "list":
        parts.push(listToPlain(block, ""));
        break;
      case "quote":
        parts.push(blocksToPlain(block.blocks));
        break;
      case "table":
        parts.push([block.header, ...block.rows].map((row) => row.map(inlineToPlain).join(" | ")).join("\n"));
        break;
      case "hr":
        break;
    }
  }
  return parts.join("\n\n");
}

/** Readable text without Markdown markers: links become their text, code blocks stay as text. */
export function markdownToPlainText(src: string): string {
  return blocksToPlain(parseMarkdown(src)).trim();
}

// ---------------------------------------------------------------------------
// Rendering (DOM, text nodes only)
// ---------------------------------------------------------------------------

function renderInline(nodes: MdInline[], parent: Node): void {
  for (const node of nodes) {
    switch (node.type) {
      case "text":
        parent.appendChild(document.createTextNode(node.text));
        break;
      case "code": {
        const el = document.createElement("code");
        el.className = "md-code-inline";
        el.textContent = node.text;
        parent.appendChild(el);
        break;
      }
      case "strong": {
        const el = document.createElement("strong");
        renderInline(node.children, el);
        parent.appendChild(el);
        break;
      }
      case "em": {
        const el = document.createElement("em");
        renderInline(node.children, el);
        parent.appendChild(el);
        break;
      }
      case "link":
        parent.appendChild(renderLink(node));
        break;
    }
  }
}

function renderLink(node: Extract<MdInline, { type: "link" }>): HTMLElement {
  if (!node.openable) {
    // Not http(s): show what the model wrote, but make it neither clickable nor navigable.
    const span = document.createElement("span");
    span.className = "md-link-inert";
    span.title = node.url;
    renderInline(node.children, span);
    return span;
  }
  const button = document.createElement("button");
  button.type = "button";
  button.className = "md-link";
  button.title = node.url;
  renderInline(node.children, button);
  const url = node.url;
  button.addEventListener("click", (event) => {
    event.preventDefault();
    if (!OPENABLE_URL.test(url)) return;
    openExternal(url).catch(() => {
      /* The system browser could not be started; nothing else to do. */
    });
  });
  return button;
}

function renderList(list: ListBlock): HTMLElement {
  const el = list.ordered ? document.createElement("ol") : document.createElement("ul");
  if (el instanceof HTMLOListElement && list.start !== 1) el.start = list.start;
  for (const item of list.items) {
    const li = document.createElement("li");
    renderInline(item.inline, li);
    if (item.sublist) li.appendChild(renderList(item.sublist));
    el.appendChild(li);
  }
  return el;
}

function renderTable(block: Extract<MdBlock, { type: "table" }>): HTMLElement {
  // A wide table scrolls sideways inside its own box instead of stretching the conversation.
  const wrap = document.createElement("div");
  wrap.className = "md-table-wrap";
  const table = document.createElement("table");
  table.className = "md-table";
  const fill = (row: HTMLTableRowElement, cells: MdInline[][], tag: "th" | "td") => {
    cells.forEach((inline, column) => {
      const cell = document.createElement(tag);
      if (tag === "th") cell.scope = "col";
      const align = block.align[column];
      if (align) cell.style.textAlign = align;
      renderInline(inline, cell);
      row.appendChild(cell);
    });
  };
  const head = table.createTHead().insertRow();
  fill(head, block.header, "th");
  const body = table.createTBody();
  for (const cells of block.rows) fill(body.insertRow(), cells, "td");
  wrap.appendChild(table);
  return wrap;
}

function renderBlock(block: MdBlock): HTMLElement {
  switch (block.type) {
    case "paragraph": {
      const el = document.createElement("p");
      renderInline(block.inline, el);
      return el;
    }
    case "heading": {
      // Chat answers sit under the page's h1/h2: start at h3.
      const el = document.createElement(`h${Math.min(6, block.level + 2)}`);
      el.className = "md-heading";
      renderInline(block.inline, el);
      return el;
    }
    case "code": {
      const pre = document.createElement("pre");
      pre.className = "md-code";
      const code = document.createElement("code");
      if (block.lang) code.dataset.lang = block.lang.slice(0, 32);
      code.textContent = block.text;
      pre.appendChild(code);
      return pre;
    }
    case "list":
      return renderList(block);
    case "quote": {
      const el = document.createElement("blockquote");
      for (const child of block.blocks) el.appendChild(renderBlock(child));
      return el;
    }
    case "table":
      return renderTable(block);
    case "hr":
      return document.createElement("hr");
  }
}

/** Replaces the children of `container` with the rendered blocks. Builds DOM only; never HTML. */
export function renderMarkdown(blocks: MdBlock[], container: HTMLElement): void {
  const fragment = document.createDocumentFragment();
  for (const block of blocks) fragment.appendChild(renderBlock(block));
  container.replaceChildren(fragment);
}
