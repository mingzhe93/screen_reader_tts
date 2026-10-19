// Tools UI: untrusted fragments/results are always text nodes; sources never embed resources.
import { invoke } from "@tauri-apps/api/tauri";
import { button, errorText, h, readStored, svgIcon, writeStored } from "./assistant-dom";
import type { AssistantChatDonePayload, AssistantModelInfo, AssistantThinkingBudget, AssistantToolCallPayload, AssistantToolLimits, AssistantToolResultPayload, AssistantToolSource, AssistantToolsSettings } from "./types";

import * as availableTools from "./tools/available-tools";
import type { SettingsControl, ToolSettingsContext, ToolUiDefinition } from "./tools/types";
import { CONSENT_KEY, consentToTools } from "./tools/consent";
import { confirmAction } from "./confirm-dialog";
export { TOOLS_CONSENT, consentToTools } from "./tools/consent";
export const SMALL_MODEL_WARNING = "Tool use has not been verified for this model and may be unreliable. Continue?";
export const DEFAULT_TOOL_LIMITS: AssistantToolLimits = { max_rounds: 5, search_timeout_secs: 8, fetch_timeout_secs: 10, weather_timeout_secs: 5, code_timeout_secs: 60 };
let limits = { ...DEFAULT_TOOL_LIMITS };
export function toolLimits(): AssistantToolLimits { return { ...limits }; }

/** HTTP(S) links without URL credentials only. Navigation is user initiated, never fetched by the UI. */
export function sourceHref(value: string): string | null {
  try {
    const url = new URL(value);
    return (url.protocol === "https:" || url.protocol === "http:") && !url.username && !url.password ? url.href : null;
  } catch { return null; }
}
export function historyWithNote(text: string, note: string | undefined): string {
  return [text.trim(), note?.trim()].filter(Boolean).join("\n\n");
}

/** Only terminal metadata is authoritative for history; visible preambles stay in the transcript. */
export function finalAnswer(rounds: Map<number, string>, payload?: Pick<AssistantChatDonePayload, "final_text" | "final_round">): string {
  if (typeof payload?.final_text === "string") return payload.final_text;
  const round = payload?.final_round ?? Math.max(0, ...rounds.keys());
  return rounds.get(round) ?? "";
}

export type ToolEntry = {
  round: number; index: number; id?: string; name: string; args: string;
  phase: string; status?: string; summary?: string; output?: string;
};
/** Indexed fragments can precede both the call id and name. Never concatenate full arguments twice. */
export class ToolTranscript {
  entries: ToolEntry[] = [];
  sources: AssistantToolSource[] = [];
  call(payload: AssistantToolCallPayload): void {
    let entry = this.entries.find((item) => item.round === payload.round && item.index === payload.call_index);
    if (!entry) {
      entry = { round: payload.round, index: payload.call_index, name: "Tool", args: "", phase: payload.phase };
      this.entries.push(entry);
    }
    if (payload.call_id) entry.id = payload.call_id;
    if (payload.name) entry.name = payload.name;
    entry.args = typeof payload.arguments === "string" ? payload.arguments : entry.args + (payload.delta ?? "");
    entry.phase = payload.phase;
  }
  result(payload: AssistantToolResultPayload): void {
    let entry = this.entries.find((item) => item.round === payload.round && item.id === payload.call_id);
    if (!entry) {
      // Defensive for an event missed during registration, without merging two parallel calls.
      entry = { round: payload.round, index: this.entries.length, id: payload.call_id, name: payload.name, args: "", phase: "done" };
      this.entries.push(entry);
    }
    Object.assign(entry, { phase: "done", status: payload.status, summary: payload.summary, output: payload.output });
    this.addSources(payload.sources);
  }
  /** An answer that ended early drops the results of its unfinished calls; mark those calls stopped. */
  settlePending(): void {
    for (const entry of this.entries) {
      if (entry.phase !== "done") Object.assign(entry, { phase: "done", status: "cancelled", summary: entry.summary ?? "Stopped" });
    }
  }
  addSources(sources: AssistantToolSource[] = []): void {
    for (const source of sources) {
      const index = this.sources.findIndex((old) => old.n === source.n);
      if (index < 0) this.sources.push(source); else this.sources[index] = source;
    }
    this.sources.sort((a, b) => a.n - b.n);
  }
}

export function renderSources(sources: AssistantToolSource[]): HTMLElement {
  const list = h("ol", "assistant-tool-sources");
  list.setAttribute("aria-label", "Sources");
  for (const source of sources) {
    const href = sourceHref(source.url);
    const label = `[${source.n}] ${source.title || source.url}`;
    if (!href) { list.appendChild(h("li", "", label)); continue; }
    const link = h("a", "", label);
    link.href = href;
    link.target = "_blank";
    link.rel = "noopener noreferrer";
    link.title = source.url;
    list.appendChild(h("li", "", link));
  }
  return list;
}
export function renderToolTranscript(model: ToolTranscript, into: HTMLElement): void {
  const nodes: HTMLElement[] = model.entries.map((entry) => {
    const detail = h("details", "assistant-tool-chip");
    detail.dataset.status = entry.status ?? entry.phase;
    detail.appendChild(h("summary", "", `${entry.name} · ${entry.status ?? entry.phase}`));
    if (entry.args) detail.appendChild(h("pre", "assistant-tool-arguments", entry.args));
    if (entry.summary) detail.appendChild(h("p", "assistant-tool-summary", entry.summary));
    if (typeof entry.output === "string") detail.appendChild(h("pre", "assistant-tool-output", entry.output));
    return detail;
  });
  if (model.sources.length) nodes.push(renderSources(model.sources));
  // Preserve explicitly opened details while replacing their text after a fragment.
  const open = Array.from(into.querySelectorAll("details")).map((node) => node.open);
  into.replaceChildren(...nodes);
  Array.from(into.querySelectorAll("details")).forEach((node, index) => { node.open = open[index] ?? false; });
}

export const TOOL_CATALOGUE: readonly ToolUiDefinition[] = Object.values(availableTools).sort((a, b) => a.order - b.order);
export type ToolName = string;
const ALL_TOOLS = TOOL_CATALOGUE.map((tool) => tool.name);

/** Migrate the old mode-wide boolean once; an explicit empty selection stays off. */
export function savedToolSelection(mode: "chat" | "voice"): ToolName[] {
  if (readStored(CONSENT_KEY) !== "true") return [];
  const stored = readStored(`voicereader.assistant.tools.selection.${mode}`);
  if (stored !== null) {
    try {
      const names: unknown = JSON.parse(stored);
      return Array.isArray(names) ? ALL_TOOLS.filter((name) => names.includes(name)) : [];
    } catch { return []; }
  }
  return readStored(`voicereader.assistant.tools.${mode}`) === "true" ? [...ALL_TOOLS] : [];
}

/** One mode's capabilities; provider and timeout settings remain shared across modes. */
export function createToolsToggle(mode: "chat" | "voice", model: () => AssistantModelInfo | null) {
  const modeTitle = mode === "chat" ? "Chat" : "Voice";
  let selected = new Set<ToolName>(savedToolSelection(mode));
  let busy = false;
  let opened = false;
  const message = h("p", "assistant-error is-hidden");
  message.setAttribute("role", "status");
  const status = h("p", "caption assistant-tools-status");
  status.setAttribute("role", "status");
  const count = h("span", "assistant-tools-count");
  const icon = svgIcon(["m14.5 6.5 3 3M6 20l-2-2L14 8a5 5 0 0 1 6-6l-3 3 2 2 3-3a5 5 0 0 1-6 6L6 20Z"]);
  const toggle = button(h("span", "assistant-tools-button-content", icon, "Tools", count), "btn btn-ghost btn-sm assistant-tools-toggle", () => open(!opened));
  const panel = h("div", "assistant-tools-popover is-hidden");
  panel.id = `assistant-tools-${mode}-menu`;
  panel.setAttribute("role", "dialog");
  panel.setAttribute("aria-label", `${modeTitle} tools`);
  toggle.setAttribute("aria-controls", panel.id);
  toggle.setAttribute("aria-haspopup", "dialog");
  const close = button("×", "btn btn-ghost btn-sm assistant-tools-close", () => open(false));
  close.setAttribute("aria-label", "Close tools");
  const master = h("input", "assistant-tool-switch"); master.type = "checkbox";
  master.setAttribute("aria-label", `All ${modeTitle} tools`);
  const total = h("span", "caption");
  const filter = h("input", "assistant-tools-filter"); filter.type = "search"; filter.placeholder = "Filter tools…";
  filter.setAttribute("aria-label", "Filter tools");
  const list = h("div", "assistant-tools-list");
  const empty = h("p", "caption is-hidden", "No matching tools.");
  const switches = new Map<ToolName, HTMLInputElement>();
  const rows = new Map<ToolName, HTMLElement>();
  const numberFields = new Map<keyof AssistantToolLimits, HTMLInputElement>();
  const settingsControls = new Map<SettingsControl, () => boolean>();
  const refreshers: ((settings: AssistantToolsSettings) => void)[] = [];
  const element = h("span", "assistant-tools-control", toggle, panel);

  function persist(): void {
    writeStored(`voicereader.assistant.tools.selection.${mode}`, JSON.stringify(ALL_TOOLS.filter((name) => selected.has(name))));
    writeStored(`voicereader.assistant.tools.${mode}`, String(selected.size > 0));
  }
  function paint(): void {
    count.textContent = selected.size ? String(selected.size) : "Off";
    toggle.classList.toggle("is-active", selected.size > 0);
    toggle.setAttribute("aria-label", `${modeTitle} tools: ${selected.size} enabled`);
    toggle.setAttribute("aria-expanded", String(opened));
    toggle.title = `Choose tools for ${modeTitle} (${selected.size} of ${ALL_TOOLS.length} enabled)`;
    master.checked = selected.size === ALL_TOOLS.length;
    master.indeterminate = selected.size > 0 && !master.checked;
    master.setAttribute("aria-checked", master.indeterminate ? "mixed" : String(master.checked));
    master.disabled = busy;
    total.textContent = `${selected.size} of ${ALL_TOOLS.length} enabled`;
    for (const [name, control] of switches) { control.checked = selected.has(name); control.disabled = busy; }
    for (const [control, enabled] of settingsControls) control.disabled = busy || !enabled();
  }
  function open(value: boolean): void {
    opened = value; panel.classList.toggle("is-hidden", !value); paint();
    if (!value) { toggle.focus(); return; }
    position();
    filter.focus();
    void refreshSettings();
  }
  function position(): void {
    if (!opened) return;
    const rect = toggle.getBoundingClientRect();
    const width = Math.min(440, window.innerWidth - 24);
    panel.style.width = `${width}px`;
    panel.style.left = `${Math.max(12, Math.min(rect.left, window.innerWidth - width - 12))}px`;
    if (rect.top >= 300) {
      panel.style.top = "auto"; panel.style.bottom = `${window.innerHeight - rect.top + 8}px`;
      panel.style.maxHeight = `${Math.min(640, rect.top - 20)}px`;
    } else {
      panel.style.bottom = "auto"; panel.style.top = `${rect.bottom + 8}px`;
      panel.style.maxHeight = `${Math.max(160, Math.min(640, window.innerHeight - rect.bottom - 20))}px`;
    }
  }
  async function change(next: Set<ToolName>): Promise<void> {
    if (busy) return;
    const adding = [...next].some((name) => !selected.has(name));
    if (adding && !(await consentToTools())) { paint(); return; }
    if (adding && !model()?.tools && !(await confirmAction(SMALL_MODEL_WARNING, "Use anyway"))) { paint(); return; }
    message.textContent = ""; message.classList.add("is-hidden");
    busy = true; paint();
    try {
      for (const tool of TOOL_CATALOGUE) {
        if (next.has(tool.name) && !selected.has(tool.name) && tool.onEnable) {
          status.textContent = `Preparing ${tool.title}…`; await tool.onEnable();
        }
      }
      selected = next; persist(); status.textContent = "";
    } catch (error) {
      message.textContent = `Tools could not start: ${errorText(error)}`;
      message.classList.remove("is-hidden"); status.textContent = "";
    } finally { busy = false; paint(); }
  }
  master.addEventListener("change", () => void change(new Set(master.checked ? ALL_TOOLS : [])));
  filter.addEventListener("input", () => {
    let visible = 0;
    for (const tool of TOOL_CATALOGUE) {
      const match = `${tool.title} ${tool.description}`.toLowerCase().includes(filter.value.toLowerCase());
      rows.get(tool.name)!.classList.toggle("is-hidden", !match); visible += Number(match);
    }
    empty.classList.toggle("is-hidden", visible > 0);
  });
  async function action(fn: () => Promise<string>): Promise<void> {
    if (busy) return; busy = true; paint(); status.textContent = "Working…";
    try { status.textContent = await fn(); await refreshSettings(); }
    catch (error) { status.textContent = errorText(error); }
    finally { busy = false; paint(); }
  }
  function limitField(name: keyof AssistantToolLimits, title: string, max: number): HTMLElement {
    const input = h("input"); input.type = "number"; input.min = "1"; input.max = String(max); input.step = "1"; input.value = String(limits[name]);
    input.setAttribute("aria-label", title); numberFields.set(name, input); settingsControls.set(input, () => true);
    const save = button("Save", "btn btn-sm", () => void action(async () => {
      if (!input.checkValidity() || !/^\d+$/.test(input.value)) { input.reportValidity(); throw new Error("Enter a valid whole number."); }
      const next = { ...limits, [name]: Number(input.value) };
      await invoke("assistant_set_tool_limits", { limits: next }); limits = next;
      return `${title} saved.`;
    }));
    settingsControls.set(save, () => true);
    return h("div", "assistant-tool-limit", h("label", "assistant-field-label", title, input), save);
  }
  const settingsContext: ToolSettingsContext = {
    run: action,
    register: (control, enabled = () => true) => { settingsControls.set(control, enabled); },
    onRefresh: (listener) => { refreshers.push(listener); },
    notice: (text) => { status.textContent = text; },
    limits: toolLimits,
    setLimits: (next) => { limits = next; },
  };
  for (const tool of TOOL_CATALOGUE) {
    const control = h("input", "assistant-tool-switch"); control.type = "checkbox";
    control.setAttribute("aria-label", `${tool.title} for ${modeTitle}`); control.setAttribute("role", "switch");
    control.addEventListener("change", () => {
      const next = new Set(selected); if (control.checked) next.add(tool.name); else next.delete(tool.name);
      void change(next);
    });
    switches.set(tool.name, control);
    const options = h("details", "assistant-tool-options", h("summary", "", "Settings"));
    options.appendChild(tool.createSettings(settingsContext));
    const row = h("div", "assistant-tool-option",
      h("label", "assistant-tool-choice", h("span", "", h("span", "assistant-tool-name", tool.title), h("span", "caption", tool.description)), control), options);
    rows.set(tool.name, row); list.appendChild(row);
  }
  const advanced = h("details", "assistant-tool-options assistant-tools-advanced", h("summary", "", "Advanced"), limitField("max_rounds", "Tool rounds per turn", 20));
  panel.append(
    h("div", "assistant-tools-heading", h("h3", "", `Tools · ${modeTitle}`), close),
    h("label", "assistant-tools-master", h("span", "", h("strong", "", "All tools"), total), master),
    h("p", "caption", `Tool choices apply to ${modeTitle}. The other mode has its own switches. Changes apply to the next message. Provider and limits are shared.`),
    filter, list, empty, advanced, status, message);
  async function refreshSettings(): Promise<void> {
    try {
      const settings = await invoke<AssistantToolsSettings>("assistant_tools_settings"); limits = { ...settings.limits };
      for (const [name, input] of numberFields) input.value = String(limits[name]);
      for (const listener of refreshers) listener(settings);
      paint();
    } catch (error) { status.textContent = `Could not load settings: ${errorText(error)}`; }
  }
  document.addEventListener("pointerdown", (event) => { if (opened && !element.contains(event.target as Node)) open(false); });
  element.addEventListener("keydown", (event) => { if (event.key === "Escape" && opened) { event.preventDefault(); open(false); } });
  window.addEventListener("resize", position);
  persist(); paint();
  return { element, button: toggle, message, enabled: () => selected.size > 0, selection: () => ALL_TOOLS.filter((name) => selected.has(name)), close: () => open(false) };
}

/** The largest thinking budget the backend accepts (`ThinkingBudget::validate`). */
export const MAX_THINKING_TOKENS = 1_048_576;
function validTokens(value: string): boolean {
  return /^\d+$/.test(value) && Number(value) > 0 && Number(value) <= MAX_THINKING_TOKENS;
}
export function parseThinkingBudget(value: string | null): AssistantThinkingBudget {
  if (value === "unlimited") return { mode: "unlimited" };
  if (value && validTokens(value)) return { mode: "tokens", tokens: Number(value) };
  return { mode: "auto" };
}
export function createThinkingLimit(mode: "chat" | "voice") {
  const key = `voicereader.assistant.thinkingBudget.${mode}`;
  let budget = parseThinkingBudget(readStored(key));
  const select = h("select");
  select.setAttribute("aria-label", `${mode === "chat" ? "Chat" : "Voice"} thinking limit`);
  const presets = [256, 512, 1024, 2048, 4096, 8192];
  for (const [value, label] of [["auto", "Auto"], ...presets.map((n) => [String(n), n.toLocaleString()]), ["unlimited", "Unlimited"], ["custom", "Custom"]]) {
    const option = h("option", "", label); option.value = value; select.appendChild(option);
  }
  const custom = h("input"); custom.type = "number"; custom.min = "1"; custom.max = String(MAX_THINKING_TOKENS); custom.step = "1";
  custom.setAttribute("aria-label", `${mode} custom thinking tokens`);
  custom.value = budget.mode === "tokens" ? String(budget.tokens) : "1024";
  select.value = budget.mode === "tokens" ? presets.includes(budget.tokens) ? String(budget.tokens) : "custom" : budget.mode;
  const note = h("p", "caption", `Auto: ${mode === "voice" ? "about 15 seconds, estimated from this model’s recent speed" : "unlimited thinking"}. A token limit resets each round. With thinking on, the response cap rises to at least the budget + 1,024 tokens, within the context limit.`);
  const invalid = h("p", "assistant-error is-hidden");
  const element = h("div", "assistant-field", h("label", "assistant-field-label", "Thinking limit", select), custom, invalid, note);
  function save(): void {
    custom.classList.toggle("is-hidden", select.value !== "custom");
    if (select.value === "custom") {
      const valid = validTokens(custom.value);
      const message = valid ? "" : `Enter a whole number from 1 to ${MAX_THINKING_TOKENS.toLocaleString()}. The previous limit still applies.`;
      custom.setCustomValidity(message);
      invalid.textContent = message; invalid.classList.toggle("is-hidden", valid);
      if (!valid) { custom.reportValidity(); return; }
      budget = { mode: "tokens", tokens: Number(custom.value) };
    } else { budget = parseThinkingBudget(select.value); invalid.classList.add("is-hidden"); }
    writeStored(key, budget.mode === "tokens" ? String(budget.tokens) : budget.mode);
  }
  custom.classList.toggle("is-hidden", select.value !== "custom");
  select.addEventListener("change", save); custom.addEventListener("input", save);
  return { element, value: () => ({ ...budget }) as AssistantThinkingBudget };
}

export async function loadToolSettings(): Promise<void> {
  const settings = await invoke<AssistantToolsSettings>("assistant_tools_settings");
  limits = { ...settings.limits };
}


