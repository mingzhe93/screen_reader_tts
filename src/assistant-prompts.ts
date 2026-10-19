// The panel's System prompt section: one prompt for chat and one for voice conversation,
// loaded with `assistant_get_prompts` and saved with `assistant_set_prompts` (null resets a
// prompt to its default). Rust applies the saved chat prompt to every chat request.

import { invoke } from "@tauri-apps/api/tauri";
import { button, errorText, h } from "./assistant-dom";
import { makeConfirmButton } from "./confirm-button";
import type { AssistantPrompts } from "./types";

export const MAX_PROMPT_CHARS = 8000;

type Tab = "chat" | "voice";

export type PromptsSection = {
  element: HTMLElement;
  /** Loads the saved prompts; a failure is shown in the section and retried on the next call. */
  load: () => Promise<void>;
};

export function createPromptsSection(options: { log: (message: string, level?: "info" | "error") => void }): PromptsSection {
  let prompts: AssistantPrompts | null = null;
  const drafts: Record<Tab, string> = { chat: "", voice: "" };
  let tab: Tab = "chat";
  let busy = false;
  let loadingNow = false;
  let message = "";
  let messageIsError = false;

  const tabs = h("div", "assistant-tabs");
  tabs.setAttribute("role", "tablist");
  tabs.setAttribute("aria-label", "System prompt");
  const tabButtons: Record<Tab, HTMLButtonElement> = {
    chat: button("Chat", "assistant-tab", () => selectTab("chat")),
    voice: button("Voice", "assistant-tab", () => selectTab("voice")),
  };
  for (const [name, tabButton] of Object.entries(tabButtons)) {
    tabButton.setAttribute("role", "tab");
    tabButton.id = `assistant-prompt-tab-${name}`;
    tabButton.setAttribute("aria-controls", "assistant-prompt-input");
    tabs.appendChild(tabButton);
  }
  tabs.addEventListener("keydown", (event) => {
    if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
    event.preventDefault();
    selectTab(tab === "chat" ? "voice" : "chat");
    tabButtons[tab].focus();
  });

  const input = h("textarea", "assistant-prompt-input");
  input.id = "assistant-prompt-input";
  input.rows = 7;
  input.maxLength = MAX_PROMPT_CHARS;
  input.setAttribute("role", "tabpanel");
  const voiceNote = h("p", "caption", "Used by voice conversation. Thinking is controlled separately in the Voice section.");
  const counter = h("span", "caption assistant-counter");
  const saveBtn = button("Save", "btn btn-primary btn-sm", () => void save(tab, drafts[tab]));
  const resetBtn = h("button", "btn btn-sm");
  resetBtn.type = "button";
  makeConfirmButton(resetBtn, {
    label: "Reset to default",
    confirmLabel: "Reset this prompt?",
    busyLabel: "Resetting...",
    onConfirm: () => save(tab, null),
  });
  const messageEl = h("p", "caption");
  messageEl.setAttribute("role", "status");
  const element = h(
    "div",
    "assistant-section-body",
    tabs,
    input,
    voiceNote,
    h("div", "assistant-prompt-actions", saveBtn, resetBtn, h("span", "toolbar-spacer"), counter),
    messageEl,
  );

  input.addEventListener("input", () => {
    drafts[tab] = input.value;
    if (!messageIsError) message = "";
    paint();
  });

  function selectTab(next: Tab): void {
    tab = next;
    input.value = drafts[tab];
    if (!messageIsError) message = "";
    paint();
  }

  function saved(which: Tab): string {
    return prompts ? prompts[which] : "";
  }

  function paint(): void {
    for (const [name, tabButton] of Object.entries(tabButtons) as [Tab, HTMLButtonElement][]) {
      tabButton.setAttribute("aria-selected", String(name === tab));
      tabButton.tabIndex = name === tab ? 0 : -1;
      tabButton.classList.toggle("is-active", name === tab);
    }
    input.setAttribute("aria-labelledby", tabButtons[tab].id);
    input.disabled = !prompts || busy;
    input.placeholder = prompts ? "No system prompt" : "";
    voiceNote.classList.toggle("is-hidden", tab !== "voice");
    counter.textContent = `${drafts[tab].length} / ${MAX_PROMPT_CHARS}`;
    const changed = !!prompts && drafts[tab] !== saved(tab);
    saveBtn.disabled = !prompts || busy || !changed;
    resetBtn.disabled = !prompts || busy;
    const shown = loadingNow && !prompts ? "Loading the saved prompts..." : changed && !message ? "Not saved yet." : message;
    messageEl.textContent = shown;
    messageEl.className = messageIsError && message ? "assistant-error" : "caption";
  }

  /** Saves one prompt (null resets it); the other keeps its saved value (null if it is the default). */
  async function save(which: Tab, value: string | null): Promise<void> {
    if (!prompts || busy) return;
    const other: Tab = which === "chat" ? "voice" : "chat";
    const otherDefault = other === "chat" ? prompts.chat_default : prompts.voice_default;
    const otherValue = prompts[other] === otherDefault ? null : prompts[other];
    const args = which === "chat" ? { chat: value, voice: otherValue } : { chat: otherValue, voice: value };
    busy = true;
    message = "";
    messageIsError = false;
    paint();
    try {
      prompts = await invoke<AssistantPrompts>("assistant_set_prompts", args);
      drafts[which] = prompts[which];
      if (tab === which) input.value = drafts[which];
      message = value === null ? "Reset to the default." : "Saved.";
    } catch (error) {
      message = errorText(error);
      messageIsError = true;
      options.log(`Saving the system prompt failed: ${message}`, "error");
    } finally {
      busy = false;
      paint();
    }
  }

  async function load(): Promise<void> {
    if (prompts || loadingNow) return;
    loadingNow = true;
    paint();
    try {
      prompts = await invoke<AssistantPrompts>("assistant_get_prompts");
      drafts.chat = prompts.chat;
      drafts.voice = prompts.voice;
      input.value = drafts[tab];
      message = "";
      messageIsError = false;
    } catch (error) {
      message = `Could not load the system prompts: ${errorText(error)}`;
      messageIsError = true;
      options.log(message, "error");
    } finally {
      loadingNow = false;
      paint();
    }
  }

  paint();
  return { element, load };
}
