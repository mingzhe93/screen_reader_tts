// The Assistant page, laid out like LM Studio inside the app shell: a compact top bar (model
// picker, status, Load/Eject, Chat | Voice, panel toggle), the conversation filling the page
// with the composer pinned at the bottom, and a collapsible settings panel on the right.
// Push-to-talk and voice mode are in src/assistant-voice.ts (rules: docs/assistant/conversation.md).
// The backend runs in Rust (see docs/assistant/ui.md for the command and event contract).
// Downloads and installs live on the Models page (src/assistant-models-page.ts).
// Pages are built with createElement and textContent only: model output is never parsed as HTML.

import { invoke } from "@tauri-apps/api/tauri";
import { listen } from "@tauri-apps/api/event";
import { IMAGE_ACCEPT, MAX_ATTACHED_IMAGES, imageFilesOf, prepareImages } from "./assistant-images";
import { markdownToPlainText, parseMarkdown, renderMarkdown } from "./assistant-markdown";
import { copyToClipboard, memoryFit } from "./assistant-format";
import { closeIcon, errorText, h, button, panelSection, paperclipIcon, readStored, slidersIcon, writeStored } from "./assistant-dom";
import { ensureHardware, noteBackendInstalled, onHardware } from "./assistant-hardware";
import { createPromptsSection } from "./assistant-prompts";
import { createSamplingSection, samplingFor } from "./assistant-sampling";
import { createMic } from "./assistant-mic";
import { createPushToTalk, createVoiceMode } from "./assistant-voice";
import type {
  AssistantChatDeltaPayload,
  AssistantChatDonePayload,
  AssistantChatErrorPayload,
  AssistantChatMessage,
  AssistantChatStartResult,
  AssistantHardware,
  AssistantModelInfo,
  AssistantStatus,
} from "./types";

const MODEL_KEY = "voicereader.assistant.model";
const CTX_KEY = "voicereader.assistant.ctx";
const THINK_KEY = "voicereader.assistant.think";
const PANEL_KEY = "voicereader.assistant.panel";

const CONTEXT_CHOICES = [
  { tokens: 8192, label: "8K" },
  { tokens: 16384, label: "16K" },
  { tokens: 32768, label: "32K" },
  { tokens: 65536, label: "64K" },
  { tokens: 131072, label: "128K" },
  { tokens: 262144, label: "256K" },
];
const DEFAULT_CTX = 32768;
const TIER_LABELS = { small: "Small", mid: "Mid", "mid-high": "Mid-high", large: "Large" } as const;
const STOPPED_SUFFIX = "\n\n[stopped by user]";
const NEAR_BOTTOM_PX = 80;
/** Below this window width the panel overlays the conversation (matches the CSS media query). */
const NARROW_QUERY = "(max-width: 1000px)";

type AssistantMode = "chat" | "voice";

type AssistantOptions = {
  log: (message: string, level?: "info" | "error") => void;
  isPageActive: () => boolean;
};

type TurnStatus = "streaming" | "done" | "stopped" | "error";

type Turn = {
  role: "user" | "assistant";
  text: string;
  reasoning: string;
  images: string[];
  status: TurnStatus;
  error: string;
  /** Short caption under an answer (older messages left out, cut off by the length limit). */
  note: string;
  view: TurnView;
  frame: number;
  renderedText: string;
  renderedReasoning: string;
};

type TurnView = {
  root: HTMLElement;
  thinking?: HTMLDetailsElement;
  thinkingText?: HTMLElement;
  answer?: HTMLElement;
  noteEl?: HTMLElement;
  errorEl?: HTMLElement;
  actions?: HTMLElement;
  actionNote?: HTMLElement;
};

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

function storedContext(): number {
  const value = Number(readStored(CTX_KEY));
  return CONTEXT_CHOICES.some((choice) => choice.tokens === value) ? value : DEFAULT_CTX;
}

function contextLabel(tokens: number | null): string {
  const choice = CONTEXT_CHOICES.find((entry) => entry.tokens === tokens);
  return choice ? choice.label : tokens ? `${Math.round(tokens / 1024)}K` : "";
}

function modelOptionLabel(model: AssistantModelInfo): string {
  return `${model.label} · ${TIER_LABELS[model.tier] ?? model.tier}`;
}

/** The stored panel state; without one, open on a wide window. */
function storedPanelOpen(): boolean {
  const stored = readStored(PANEL_KEY);
  if (stored === "open") return true;
  if (stored === "closed") return false;
  try {
    return !window.matchMedia(NARROW_QUERY).matches;
  } catch {
    return true;
  }
}

function isNarrow(): boolean {
  try {
    return window.matchMedia(NARROW_QUERY).matches;
  } catch {
    return false;
  }
}

function openModelsPage(): void {
  document.querySelector<HTMLButtonElement>('.nav-item[data-nav="models"]')?.click();
}

// ---------------------------------------------------------------------------
// Page
// ---------------------------------------------------------------------------

export async function initAssistant(options: AssistantOptions): Promise<void> {
  const { log } = options;
  const root = document.querySelector<HTMLDivElement>("#assistant-root");
  const section = document.querySelector<HTMLElement>('.page[data-page="assistant"]');
  if (!root || !section) return;

  let status: AssistantStatus | null = null;
  let statusError = "";
  let hardware: AssistantHardware | null = null;
  let loading = false;
  /** Set with `loading` while an Eject runs, so the status says what is happening. */
  let unloading = false;
  let loadError = "";
  let ctxSize = storedContext();
  let think = readStored(THINK_KEY) !== "false";
  let chosenModel = readStored(MODEL_KEY);
  let panelOpen = storedPanelOpen();
  let mode: AssistantMode = "chat";

  const turns: Turn[] = [];
  let requestId: string | null = null;
  /** Bumped by New chat, so work started in an earlier conversation cannot touch this one. */
  let conversation = 0;
  let activeTurn: Turn | null = null;
  let streaming = false;
  let attached: string[] = [];
  let composerError = "";

  // -- Top bar ----------------------------------------------------------------

  const modelSelect = h("select", "assistant-model-select");
  modelSelect.setAttribute("aria-label", "Model");
  modelSelect.title = "Model. Choosing another one loads it.";
  const noModelEl = h("span", "assistant-no-model", "No Assistant model installed");
  const openModelsTopBtn = button("Open Models", "btn btn-primary btn-sm", openModelsPage);
  const pill = h("span", "assistant-pill");
  pill.setAttribute("role", "status");
  const loadToggle = button("Load", "btn btn-sm assistant-load-toggle", () => void toggleLoad());
  const chatModeBtn = button("Chat", "assistant-mode-btn assistant-mode-chat", () => setMode("chat"));
  const voiceModeBtn = button("Voice", "assistant-mode-btn assistant-mode-voice", () => setMode("voice"));
  const voiceModeWrap = h("span", "assistant-mode-wrap", voiceModeBtn);
  const modeSwitch = h("div", "assistant-mode-switch", chatModeBtn, voiceModeWrap);
  modeSwitch.setAttribute("role", "group");
  modeSwitch.setAttribute("aria-label", "Mode");
  const panelBtn = h("button", "btn btn-ghost btn-sm assistant-panel-toggle", slidersIcon());
  panelBtn.type = "button";
  panelBtn.setAttribute("aria-controls", "assistant-panel");
  const topbar = h(
    "div",
    "assistant-topbar",
    h("div", "assistant-topbar-model", modelSelect, noModelEl, openModelsTopBtn, pill),
    h("div", "assistant-topbar-actions", loadToggle, modeSwitch, panelBtn),
  );
  const noticeEl = h("p", "assistant-error assistant-notice is-hidden");
  noticeEl.setAttribute("role", "alert");

  // -- Conversation -----------------------------------------------------------

  const stateTitle = h("h2", "card-title");
  const stateText = h("p", "empty-text");
  const openModelsStateBtn = button("Open Models", "btn btn-primary", openModelsPage);
  const stateEl = h("div", "assistant-state is-hidden", h("article", "card empty-state", stateTitle, stateText, openModelsStateBtn));

  const messagesEl = h("div", "assistant-messages");
  messagesEl.setAttribute("aria-live", "polite");
  const emptyEl = h("p", "assistant-empty", "Ask anything. The conversation stays on this computer and is cleared when you close VoiceReader.");
  messagesEl.appendChild(emptyEl);
  const thumbsEl = h("div", "assistant-thumbs is-hidden");
  const input = h("textarea", "assistant-input");
  input.rows = 2;
  input.placeholder = "Message the Assistant (Enter to send, Shift+Enter for a new line)";
  input.setAttribute("aria-label", "Message");
  const fileInput = h("input");
  fileInput.type = "file";
  fileInput.accept = IMAGE_ACCEPT;
  fileInput.multiple = true;
  fileInput.className = "is-hidden";
  const attachBtn = h("button", "btn btn-ghost btn-sm", paperclipIcon(), "Attach");
  attachBtn.type = "button";
  attachBtn.title = `Attach up to ${MAX_ATTACHED_IMAGES} images (PNG, JPEG or WebP). You can also paste or drop an image.`;
  /** The push-to-talk button goes here once the microphone session exists (below). */
  const pttSlot = h("span", "assistant-ptt-slot");
  const sendBtn = button("Send", "btn btn-primary btn-sm", () => void send());
  const stopBtn = button("Stop", "btn btn-quiet-danger btn-sm is-hidden", () => void stopStreaming());
  const newChatBtn = button("New chat", "btn btn-ghost btn-sm", () => void newChat());
  const composerHint = h("p", "caption assistant-hint");
  const composerErrorEl = h("p", "assistant-error is-hidden");
  composerErrorEl.setAttribute("role", "alert");
  const composer = h(
    "div",
    "assistant-composer",
    thumbsEl,
    input,
    h("div", "assistant-composer-row", attachBtn, pttSlot, newChatBtn, h("span", "toolbar-spacer"), stopBtn, sendBtn),
    fileInput,
  );
  const composerWrap = h("div", "assistant-composer-wrap", composer, composerErrorEl, composerHint);
  const main = h("div", "assistant-main", stateEl, messagesEl, composerWrap);

  // -- Right panel ------------------------------------------------------------

  const ctxSelect = h("select", "assistant-ctx-select");
  ctxSelect.id = "assistant-ctx";
  for (const choice of CONTEXT_CHOICES) {
    const option = h("option", "", `${choice.label} tokens`);
    option.value = String(choice.tokens);
    ctxSelect.appendChild(option);
  }
  const ctxLabel = h("label", "assistant-field-label", "Context length");
  ctxLabel.htmlFor = ctxSelect.id;
  const reloadBtn = button("Reload now", "btn btn-sm", () => void loadSelected());
  const ctxNote = h("p", "caption");
  const fitLine = h("p", "assistant-fit");
  const fitDetail = h("p", "caption");
  const fitEl = h("div", "assistant-fit-block", fitLine, fitDetail);
  const modelBody = h(
    "div",
    "assistant-section-body",
    h("div", "assistant-field", ctxLabel, h("div", "assistant-ctx-row", ctxSelect, reloadBtn), ctxNote),
    fitEl,
  );

  const prompts = createPromptsSection({ log });

  const thinkBox = h("input");
  thinkBox.type = "checkbox";
  thinkBox.checked = think;
  const thinkLabel = h("label", "assistant-think", thinkBox, "Think before answering");
  const reasoningBody = h(
    "div",
    "assistant-section-body",
    thinkLabel,
    h("p", "caption", "Chat only. The model reasons first: slower, but better on hard questions. Voice conversation always answers without thinking."),
  );

  const sampling = createSamplingSection();
  const reasoningSection = panelSection("Reasoning", reasoningBody);
  // Filled in once the voice controller exists (below).
  const voiceSection = panelSection("Voice");
  voiceSection.classList.add("is-hidden");

  const panelClose = h("button", "btn btn-ghost btn-sm assistant-panel-close", closeIcon());
  panelClose.type = "button";
  panelClose.title = "Hide settings";
  panelClose.setAttribute("aria-label", "Hide settings");
  const panel = h(
    "aside",
    "assistant-panel",
    h("div", "assistant-panel-head", h("h2", "card-title", "Settings"), panelClose),
    panelSection("Model", modelBody),
    panelSection("System prompt", prompts.element),
    reasoningSection,
    voiceSection,
    panelSection("Sampling", sampling.element),
  );
  panel.id = "assistant-panel";
  panel.setAttribute("aria-label", "Assistant settings");

  const shell = h("div", "assistant-shell", topbar, noticeEl, h("div", "assistant-body", main, panel));
  root.replaceChildren(shell);

  // -- Voice: one microphone session for push-to-talk and voice mode ----------

  const mic = await createMic(log);
  const ptt = createPushToTalk({
    mic,
    log,
    input,
    onDraftChanged: () => resizeInput(),
    ownsMic: () => mode === "chat",
  });
  pttSlot.appendChild(ptt.element);
  composerWrap.insertBefore(ptt.errorEl, composerErrorEl);
  composerWrap.insertBefore(ptt.statusEl, ptt.errorEl);
  const voice = await createVoiceMode({
    mic,
    log,
    unavailableReason: voiceUnavailableReason,
    sampling: () => requestSampling(),
    onNewChat: () => void newChat(),
  });
  main.insertBefore(voice.view, composerWrap);
  voiceSection.appendChild(voice.panelBody);

  // -- Derived state ----------------------------------------------------------

  function installedModels(): AssistantModelInfo[] {
    return status?.models.filter((model) => model.installed) ?? [];
  }

  /** The model that Load uses: the stored choice if it is installed, otherwise the running or first installed one. */
  function selectedModelId(): string | null {
    const installed = installedModels();
    if (chosenModel && installed.some((model) => model.id === chosenModel)) return chosenModel;
    const running = status?.server.model_id;
    if (running && installed.some((model) => model.id === running)) return running;
    return installed[0]?.id ?? null;
  }

  function selectedModel(): AssistantModelInfo | null {
    const id = selectedModelId();
    return status?.models.find((model) => model.id === id) ?? null;
  }

  function platformSupported(): boolean {
    return !!status?.platform_supported;
  }

  function serverState(): AssistantStatus["server"]["state"] {
    return status?.server.state ?? "stopped";
  }

  function isReady(): boolean {
    return serverState() === "ready";
  }

  function busy(): boolean {
    return loading || serverState() === "starting";
  }

  /** True when the page has nothing to run: no backend or no installed model. */
  function nothingInstalled(): boolean {
    return !!status && (!status.backend_installed || installedModels().length === 0);
  }

  /** The selected model's trained context limit, or null when no model is selected. */
  function maxContext(): number | null {
    return selectedModel()?.max_context ?? null;
  }

  /** The context the next load uses: the setting, capped at the selected model's limit. */
  function effectiveCtx(): number {
    const limit = maxContext();
    return limit ? Math.min(ctxSize, limit) : ctxSize;
  }

  // -- Rendering: top bar and notices -----------------------------------------

  function pillState(): { text: string; tone: "ok" | "busy" | "idle" | "warn" | "error" } {
    const state = serverState();
    if (unloading) return { text: "Unloading…", tone: "busy" };
    if (busy()) return { text: "Loading model…", tone: "busy" };
    if (state === "ready") {
      const device = status?.server.device_label;
      return { text: device ? `Ready · ${device}` : "Ready", tone: "ok" };
    }
    if (state === "in_use_elsewhere") return { text: "In use in another window", tone: "warn" };
    if (loadError) return { text: loadError, tone: "error" };
    if (state === "error") return { text: status?.server.error || "The model stopped unexpectedly", tone: "error" };
    return { text: "Stopped", tone: "idle" };
  }

  function renderModelOptions(): void {
    const installed = installedModels();
    const wanted = installed.map((model) => `${model.id}\u0000${modelOptionLabel(model)}`).join("\u0001");
    if (modelSelect.dataset.options !== wanted) {
      modelSelect.replaceChildren(
        ...installed.map((model) => {
          const option = h("option", "", modelOptionLabel(model));
          option.value = model.id;
          return option;
        }),
      );
      modelSelect.dataset.options = wanted;
    }
    modelSelect.value = selectedModelId() ?? "";
  }

  function renderTopbar(): void {
    const supported = !!status?.platform_supported;
    topbar.classList.toggle("is-hidden", !supported);
    noticeEl.textContent = statusError;
    noticeEl.classList.toggle("is-hidden", !statusError || !status);
    if (!status || !supported) return;

    const empty = nothingInstalled();
    for (const el of [modelSelect, pill, loadToggle]) el.classList.toggle("is-hidden", empty);
    noModelEl.classList.toggle("is-hidden", !empty);
    openModelsTopBtn.classList.toggle("is-hidden", !empty);
    renderModelOptions();
    const state = serverState();
    modelSelect.disabled = busy() || state === "in_use_elsewhere";

    const tone = pillState();
    pill.textContent = tone.text;
    pill.title = tone.text;
    pill.className = `assistant-pill ${tone.tone}${empty ? " is-hidden" : ""}`;

    // One button: Eject while a server runs (or failed), Load otherwise.
    const eject = state === "ready";
    loadToggle.textContent = unloading ? "Ejecting…" : busy() ? "Loading…" : eject ? "Eject" : "Load";
    loadToggle.classList.toggle("btn-primary", !eject && !busy());
    loadToggle.title = eject ? "Unload the model and free its memory" : "Load the selected model";
    loadToggle.disabled = busy() || state === "in_use_elsewhere" || (!eject && !selectedModelId());

    chatModeBtn.setAttribute("aria-pressed", String(mode === "chat"));
    voiceModeBtn.setAttribute("aria-pressed", String(mode === "voice"));
    chatModeBtn.classList.toggle("is-active", mode === "chat");
    voiceModeBtn.classList.toggle("is-active", mode === "voice");
    chatModeBtn.title = "Chat: type or dictate, and read the answers";
    voiceModeBtn.title = "Voice conversation: talk, and hear the answers spoken";
  }

  function renderPanelState(): void {
    shell.classList.toggle("is-panel-open", panelOpen);
    panelBtn.setAttribute("aria-expanded", String(panelOpen));
    panelBtn.title = panelOpen ? "Hide settings" : "Show settings";
    panelBtn.setAttribute("aria-label", panelBtn.title);
    panelBtn.classList.toggle("is-active", panelOpen);
  }

  /** The conversation area's empty states: unsupported platform, status error, nothing installed. */
  function renderState(): void {
    let title = "";
    let text = "";
    let showButton = false;
    if (!status && statusError) {
      title = "The Assistant could not start";
      text = statusError;
    } else if (!status) {
      text = "Checking the Assistant…";
    } else if (!status.platform_supported) {
      title = "The Assistant is not available on this computer";
      text = "The Assistant needs Windows (x64) or a Mac with Apple Silicon.";
    } else if (nothingInstalled() && turns.length === 0) {
      title = "No Assistant model installed";
      text = status.backend_installed
        ? "Download a model on the Models page to start chatting. Everything runs on this computer."
        : "Install the llama.cpp backend and download a model on the Models page to start chatting. Everything runs on this computer.";
      showButton = true;
    }
    const show = !!(title || text);
    const voiceMode = mode === "voice";
    stateEl.classList.toggle("is-hidden", !show);
    messagesEl.classList.toggle("is-hidden", show || voiceMode);
    composerWrap.classList.toggle("is-hidden", show || voiceMode);
    voice.view.classList.toggle("is-hidden", show || !voiceMode);
    // Voice never thinks, so its panel shows the Voice section instead of Reasoning.
    reasoningSection.classList.toggle("is-hidden", voiceMode);
    voiceSection.classList.toggle("is-hidden", !voiceMode);
    stateTitle.textContent = title;
    stateTitle.classList.toggle("is-hidden", !title);
    stateText.textContent = text;
    openModelsStateBtn.classList.toggle("is-hidden", !showButton);
    panel.classList.toggle("is-hidden", !status?.platform_supported);
  }

  // -- Rendering: panel -------------------------------------------------------

  function renderModelSection(): void {
    ctxSelect.value = String(ctxSize);
    const model = selectedModel();
    const limit = maxContext();
    const modelLabel = model?.label ?? "This model";
    for (const option of Array.from(ctxSelect.options)) {
      const over = limit !== null && Number(option.value) > limit;
      option.disabled = over && Number(option.value) !== ctxSize;
      option.title = over ? `${modelLabel} supports up to ${contextLabel(limit)} tokens.` : "";
    }
    const capped = limit !== null && ctxSize > limit;
    const mismatch = isReady() && !!status && status.server.ctx_size !== effectiveCtx();
    reloadBtn.classList.toggle("is-hidden", !mismatch);
    reloadBtn.disabled = busy();
    const notes = ["Applies the next time the model loads."];
    if (capped) notes.push(`${modelLabel} supports up to ${contextLabel(limit)} tokens, so it loads with ${contextLabel(limit)}.`);
    if (mismatch && status) notes.push(`The loaded model uses ${contextLabel(status.server.ctx_size)} tokens.`);
    ctxNote.textContent = notes.join(" ");
    renderContextWarning(model);
  }

  /** The memory fit (plan §5.3) for the selected model at the context it will load with. */
  function renderContextWarning(model: AssistantModelInfo | null): void {
    if (!model || !model.memory) {
      fitEl.classList.add("is-hidden");
      return;
    }
    fitEl.classList.remove("is-hidden");
    if (!hardware) {
      fitLine.className = "assistant-fit";
      fitLine.textContent = "Checking this computer's memory…";
      fitDetail.textContent = "";
      return;
    }
    const fit = memoryFit(model.memory, effectiveCtx(), hardware);
    fitLine.className = `assistant-fit fit-${fit.level}`;
    fitLine.textContent = `${fit.icon} ${fit.label} at ${contextLabel(effectiveCtx())}`;
    fitDetail.textContent = fit.detail + (model.memory.measured ? "" : " The estimate is calculated, not measured.");
  }

  function renderPanel(): void {
    renderPanelState();
    renderModelSection();
    sampling.setModel(selectedModel());
  }

  // -- Rendering: composer ----------------------------------------------------

  function canSend(): boolean {
    return isReady() && !streaming;
  }

  function renderComposer(): void {
    sendBtn.disabled = !canSend();
    sendBtn.classList.toggle("is-hidden", streaming);
    stopBtn.classList.toggle("is-hidden", !streaming);
    attachBtn.disabled = attached.length >= MAX_ATTACHED_IMAGES;
    composerErrorEl.textContent = composerError;
    composerErrorEl.classList.toggle("is-hidden", !composerError);
    let hint = "";
    if (nothingInstalled()) hint = "Install a model on the Models page to keep chatting.";
    else if (serverState() === "in_use_elsewhere") hint = "Another VoiceReader window is using the model.";
    else if (!isReady() && !busy()) hint = "Load the model to start chatting.";
    composerHint.textContent = hint;
    composerHint.classList.toggle("is-hidden", !hint);
    thumbsEl.classList.toggle("is-hidden", attached.length === 0);
    thumbsEl.replaceChildren(
      ...attached.map((url, index) => {
        const img = h("img", "assistant-thumb");
        img.src = url;
        img.alt = `Attached image ${index + 1}`;
        const remove = h("button", "assistant-thumb-remove", "×");
        remove.type = "button";
        remove.title = "Remove image";
        remove.setAttribute("aria-label", `Remove image ${index + 1}`);
        remove.addEventListener("click", () => {
          attached = attached.filter((_, i) => i !== index);
          renderComposer();
        });
        return h("div", "assistant-thumb-wrap", img, remove);
      }),
    );
  }

  function renderAll(): void {
    renderTopbar();
    renderState();
    renderPanel();
    renderComposer();
    voice.refresh();
  }

  /** Why voice mode cannot talk right now, or "" when the model is ready. */
  function voiceUnavailableReason(): string {
    if (!status || !status.platform_supported) return "The Assistant is not available.";
    if (nothingInstalled()) return "Install a model on the Models page to talk.";
    if (serverState() === "in_use_elsewhere") return "Another VoiceReader window is using the model.";
    if (unloading) return "Unloading…";
    if (busy()) return "Loading model…";
    if (!isReady()) return "Load the model to start talking.";
    return "";
  }

  /** Voice mode listens only while its page is the visible one. */
  function syncVoiceEntered(): void {
    voice.setEntered(mode === "voice" && options.isPageActive());
  }

  function nearBottom(): boolean {
    return messagesEl.scrollHeight - messagesEl.scrollTop - messagesEl.clientHeight < NEAR_BOTTOM_PX;
  }

  function scrollToEnd(): void {
    messagesEl.scrollTop = messagesEl.scrollHeight;
  }

  // -- Mode and panel ---------------------------------------------------------

  /**
   * Switches between chat and voice conversation. Each mode keeps its own conversation.
   * Entering voice mode opens the microphone; leaving it cancels any answer and closes it.
   */
  function setMode(next: AssistantMode): void {
    if (next === mode) return;
    mode = next;
    shell.classList.toggle("is-voice-mode", mode === "voice");
    ptt.cancel();
    syncVoiceEntered();
    renderAll();
    if (mode === "chat" && turns.length > 0) scrollToEnd();
  }

  function setPanelOpen(open: boolean, focus = false): void {
    panelOpen = open;
    writeStored(PANEL_KEY, open ? "open" : "closed");
    renderPanelState();
    if (focus) (open ? panelClose : panelBtn).focus();
  }

  // -- Messages ---------------------------------------------------------------

  function createTurn(role: Turn["role"], text: string, images: string[]): Turn {
    const turn: Turn = {
      role,
      text,
      reasoning: "",
      images,
      status: role === "assistant" ? "streaming" : "done",
      error: "",
      note: "",
      view: { root: h("article", `assistant-msg assistant-msg-${role}`) },
      frame: 0,
      renderedText: "",
      renderedReasoning: "",
    };
    const view = turn.view;
    if (role === "user") {
      const bubble = h("div", "assistant-bubble");
      if (text) bubble.appendChild(h("p", "assistant-user-text", text));
      if (images.length > 0) {
        bubble.appendChild(
          h(
            "div",
            "assistant-thumbs",
            ...images.map((url, index) => {
              const img = h("img", "assistant-thumb");
              img.src = url;
              img.alt = `Attached image ${index + 1}`;
              return img;
            }),
          ),
        );
      }
      view.root.appendChild(bubble);
    } else {
      view.thinkingText = h("div", "assistant-thinking-text");
      view.thinking = h("details", "assistant-thinking is-hidden", h("summary", "", "Thinking…"), view.thinkingText);
      view.answer = h("div", "assistant-answer md");
      view.noteEl = h("p", "caption assistant-turn-note is-hidden");
      view.errorEl = h("p", "assistant-error is-hidden");
      view.actionNote = h("span", "caption");
      view.actions = h(
        "div",
        "assistant-msg-actions is-hidden",
        button("Speak", "btn btn-ghost btn-sm", () => void speak(turn)),
        copyButton(turn),
        view.actionNote,
      );
      view.root.append(view.thinking, view.answer, view.noteEl, view.errorEl, view.actions);
    }
    return turn;
  }

  function copyButton(turn: Turn): HTMLButtonElement {
    const copy = button("Copy", "btn btn-ghost btn-sm", () => {
      void copyToClipboard(markdownToPlainText(turn.text)).then(
        () => {
          copy.textContent = "Copied";
          window.setTimeout(() => (copy.textContent = "Copy"), 1500);
        },
        (error) => setActionNote(turn, errorText(error)),
      );
    });
    return copy;
  }

  function setActionNote(turn: Turn, message: string): void {
    if (turn.view.actionNote) turn.view.actionNote.textContent = message;
  }

  async function speak(turn: Turn): Promise<void> {
    const text = markdownToPlainText(turn.text);
    if (!text) return;
    setActionNote(turn, "");
    try {
      await invoke("speak_text", { text });
    } catch (error) {
      setActionNote(turn, `Could not speak: ${errorText(error)}`);
      log(`Assistant speak failed: ${errorText(error)}`, "error");
    }
  }

  /** Brings an assistant message's DOM up to date with its data. Called at most once per frame while streaming. */
  function renderTurn(turn: Turn): void {
    const { view } = turn;
    if (turn.role !== "assistant" || !view.answer || !view.thinking || !view.thinkingText) return;
    const stick = nearBottom();
    if (turn.reasoning !== turn.renderedReasoning) {
      view.thinkingText.textContent = turn.reasoning;
      view.thinking.classList.toggle("is-hidden", !turn.reasoning);
      turn.renderedReasoning = turn.reasoning;
    }
    if (turn.text !== turn.renderedText) {
      renderMarkdown(parseMarkdown(turn.text), view.answer);
      turn.renderedText = turn.text;
    }
    view.answer.classList.toggle("is-streaming", turn.status === "streaming");
    if (view.noteEl) {
      view.noteEl.textContent = turn.note;
      view.noteEl.classList.toggle("is-hidden", !turn.note);
    }
    if (view.errorEl) {
      view.errorEl.textContent = turn.error;
      view.errorEl.classList.toggle("is-hidden", !turn.error);
    }
    view.actions?.classList.toggle("is-hidden", turn.status === "streaming" || !turn.text.trim());
    if (stick) scrollToEnd();
  }

  function scheduleRender(turn: Turn): void {
    if (turn.frame) return;
    turn.frame = requestAnimationFrame(() => {
      turn.frame = 0;
      renderTurn(turn);
    });
  }

  function renderNow(turn: Turn): void {
    if (turn.frame) {
      cancelAnimationFrame(turn.frame);
      turn.frame = 0;
    }
    renderTurn(turn);
  }

  function addTurn(turn: Turn): void {
    emptyEl.remove();
    turns.push(turn);
    messagesEl.appendChild(turn.view.root);
    renderNow(turn);
    scrollToEnd();
  }

  function removeTurn(turn: Turn): void {
    const index = turns.indexOf(turn);
    if (index >= 0) turns.splice(index, 1);
    if (turn.frame) cancelAnimationFrame(turn.frame);
    turn.view.root.remove();
    if (turns.length === 0) messagesEl.appendChild(emptyEl);
  }

  // -- Backend actions --------------------------------------------------------

  function applyStatus(next: AssistantStatus): void {
    const wasReady = isReady();
    status = next;
    statusError = "";
    if (next.server.state === "ready") loadError = "";
    noteBackendInstalled(next.backend_installed);
    // An ejected (or failed) model also ends voice input. Voice mode reopens the microphone
    // itself when the model is ready again (voice.refresh in renderAll).
    if (wasReady && !isReady()) {
      ptt.cancel();
      void mic.close();
    }
    renderAll();
  }

  async function refreshStatus(): Promise<void> {
    try {
      applyStatus(await invoke<AssistantStatus>("assistant_status"));
    } catch (error) {
      statusError = `Could not check the Assistant: ${errorText(error)}`;
      log(statusError, "error");
      renderAll();
    }
  }

  async function loadSelected(): Promise<void> {
    const modelId = selectedModelId();
    if (!modelId || loading) return;
    // Captured with the model, before any await: the selection may change meanwhile.
    const ctxForLoad = effectiveCtx();
    loading = true;
    loadError = "";
    renderAll();
    try {
      // A running server keeps its old model and context length, so stop it first.
      if (serverState() === "ready" || serverState() === "error") {
        await cancelStreamingRequest();
        applyStatus(await invoke<AssistantStatus>("assistant_unload"));
      }
      applyStatus(await invoke<AssistantStatus>("assistant_load", { modelId, ctxSize: ctxForLoad }));
    } catch (error) {
      loadError = errorText(error);
      log(`Assistant load failed: ${loadError}`, "error");
      await refreshStatus();
    } finally {
      loading = false;
      renderAll();
    }
  }

  async function unloadModel(): Promise<void> {
    loading = true;
    unloading = true;
    renderAll();
    try {
      await cancelStreamingRequest();
      applyStatus(await invoke<AssistantStatus>("assistant_unload"));
    } catch (error) {
      loadError = errorText(error);
      log(`Assistant unload failed: ${loadError}`, "error");
    } finally {
      loading = false;
      unloading = false;
      renderAll();
    }
  }

  function toggleLoad(): void {
    if (busy()) return;
    if (serverState() === "ready") void unloadModel();
    else void loadSelected();
  }

  /** Loads the chosen model when the page opens and nothing is running. */
  async function autoLoadOnOpen(): Promise<void> {
    await refreshStatus();
    if (status?.platform_supported) void prompts.load();
    if (
      status?.platform_supported &&
      status.backend_installed &&
      status.server.state === "stopped" &&
      selectedModelId() &&
      !loading
    ) {
      await loadSelected();
    }
  }

  // -- Chat actions -----------------------------------------------------------

  function history(): AssistantChatMessage[] {
    // A question is sent back only with its answer, so the history keeps alternating user and
    // assistant messages (some chat templates reject two user messages in a row). An exchange
    // whose answer failed, or never produced text, is left out entirely.
    const messages: AssistantChatMessage[] = [];
    for (let index = 0; index + 1 < turns.length; index += 1) {
      const question = turns[index];
      const answer = turns[index + 1];
      if (question.role !== "user" || answer.role !== "assistant") continue;
      if (answer.status === "done" && answer.text.trim()) {
        messages.push({ role: "user", text: question.text, images: question.images });
        messages.push({ role: "assistant", text: answer.text, images: [] });
      } else if (answer.status === "stopped") {
        messages.push({ role: "user", text: question.text, images: question.images });
        messages.push({ role: "assistant", text: answer.text + STOPPED_SUFFIX, images: [] });
      }
    }
    return messages;
  }

  /** The sampling settings of the model the request goes to (the loaded one), or null for its defaults. */
  function requestSampling() {
    const id = status?.server.model_id ?? selectedModelId();
    const model = status?.models.find((entry) => entry.id === id);
    return model ? samplingFor(model) : null;
  }

  async function send(): Promise<void> {
    const text = input.value.trim();
    if (!canSend() || (!text && attached.length === 0)) return;
    composerError = "";
    const images = attached;
    const messages = [...history(), { role: "user" as const, text, images }];
    const id = crypto.randomUUID();
    const userTurn = createTurn("user", text, images);
    const answer = createTurn("assistant", "", []);
    // Events can arrive before the start command resolves, so the request is registered first.
    requestId = id;
    activeTurn = answer;
    streaming = true;
    addTurn(userTurn);
    addTurn(answer);
    renderComposer();
    const startedIn = conversation;
    try {
      const started = await invoke<AssistantChatStartResult>("assistant_chat_start", {
        requestId: id,
        messages,
        think,
        sampling: requestSampling(),
      });
      // New chat was pressed while the request was starting: the composer now belongs to the
      // new conversation (its draft may even repeat this text), so leave it alone.
      if (startedIn !== conversation) return;
      if (started.dropped_messages > 0) {
        const count = started.dropped_messages;
        answer.note = `${count} older message${count === 1 ? "" : "s"} no longer fit and ${count === 1 ? "was" : "were"} left out`;
        scheduleRender(answer);
      }
      if (input.value.trim() === text) {
        input.value = "";
        resizeInput();
      }
      if (attached === images) attached = [];
    } catch (error) {
      if (startedIn !== conversation) return;
      // Not started (for example "message too long"): undo the optimistic turns and keep the draft.
      if (requestId === id) {
        requestId = null;
        activeTurn = null;
        streaming = false;
      }
      removeTurn(answer);
      removeTurn(userTurn);
      composerError = errorText(error);
    }
    renderComposer();
  }

  async function cancelStreamingRequest(): Promise<void> {
    const id = requestId;
    if (!id) return;
    try {
      await invoke("assistant_chat_cancel", { requestId: id });
    } catch (error) {
      log(`Assistant cancel failed: ${errorText(error)}`, "error");
    }
  }

  async function stopStreaming(): Promise<void> {
    stopBtn.disabled = true;
    await cancelStreamingRequest();
    stopBtn.disabled = false;
  }

  function finishRequest(turn: Turn, next: TurnStatus): void {
    turn.status = next;
    requestId = null;
    activeTurn = null;
    streaming = false;
    renderNow(turn);
    renderComposer();
  }

  async function newChat(): Promise<void> {
    // New chat clears the conversation of the mode on screen; the other mode keeps its own.
    if (mode === "voice") {
      voice.newConversation();
      return;
    }
    conversation += 1;
    // Reset everything before awaiting the cancel, so a draft typed while it is pending survives.
    const cancelledId = streaming ? requestId : null;
    if (streaming) {
      const turn = activeTurn;
      // Late events for the cancelled request are ignored because the id is cleared.
      requestId = null;
      activeTurn = null;
      streaming = false;
      if (turn) turn.status = "stopped";
    }
    for (const turn of turns) if (turn.frame) cancelAnimationFrame(turn.frame);
    turns.length = 0;
    messagesEl.replaceChildren(emptyEl);
    attached = [];
    composerError = "";
    input.value = "";
    resizeInput();
    renderState();
    renderComposer();
    if (cancelledId) {
      try {
        await invoke("assistant_chat_cancel", { requestId: cancelledId });
      } catch (error) {
        log(`Assistant cancel failed: ${errorText(error)}`, "error");
      }
    }
  }

  // -- Wiring -----------------------------------------------------------------

  function resizeInput(): void {
    input.style.height = "auto";
    input.style.height = `${Math.min(input.scrollHeight, 200)}px`;
  }

  async function attachFiles(files: File[], startedIn = conversation): Promise<void> {
    if (files.length === 0) return;
    const result = await prepareImages(files, attached.length);
    if (startedIn !== conversation) return;
    // Another attach may have finished while these images were decoding, so check the room again.
    const room = Math.max(0, MAX_ATTACHED_IMAGES - attached.length);
    const errors = [...result.errors];
    if (result.images.length > room) errors.push(`You can attach up to ${MAX_ATTACHED_IMAGES} images per message.`);
    attached = [...attached, ...result.images.slice(0, room)];
    composerError = [...new Set(errors)].join(" ");
    renderComposer();
  }

  input.addEventListener("input", resizeInput);
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
      event.preventDefault();
      void send();
    }
  });
  input.addEventListener("paste", (event) => {
    const data = event.clipboardData;
    if (!data) return;
    const images = imageFilesOf(data.files);
    // A copied spreadsheet range carries text plus a picture; only treat picture-only pastes as attachments.
    if (images.length > 0 && !data.getData("text/plain")) {
      event.preventDefault();
      void attachFiles(images);
    }
  });
  attachBtn.addEventListener("click", () => fileInput.click());
  fileInput.addEventListener("change", () => {
    const files = Array.from(fileInput.files ?? []);
    fileInput.value = "";
    void attachFiles(files);
  });
  composer.addEventListener("dragover", (event) => {
    if (event.dataTransfer?.types.includes("Files")) {
      event.preventDefault();
      composer.classList.add("is-dragging");
    }
  });
  composer.addEventListener("dragleave", () => composer.classList.remove("is-dragging"));
  composer.addEventListener("drop", (event) => {
    composer.classList.remove("is-dragging");
    const files = Array.from(event.dataTransfer?.files ?? []);
    if (files.length === 0) return;
    event.preventDefault();
    const images = imageFilesOf(files);
    if (images.length === 0) {
      composerError = "Only PNG, JPEG and WebP images can be attached.";
      renderComposer();
      return;
    }
    void attachFiles(images);
  });
  thinkBox.addEventListener("change", () => {
    think = thinkBox.checked;
    writeStored(THINK_KEY, String(think));
  });
  ctxSelect.addEventListener("change", () => {
    ctxSize = Number(ctxSelect.value);
    writeStored(CTX_KEY, String(ctxSize));
    renderAll();
  });
  modelSelect.addEventListener("change", () => {
    const next = modelSelect.value;
    if (!next || next === selectedModelId()) return;
    chosenModel = next;
    writeStored(MODEL_KEY, next);
    // Choosing a model loads it (the running one is unloaded first), unless another window has the server.
    if (serverState() === "in_use_elsewhere") renderAll();
    else void loadSelected();
  });
  panelBtn.addEventListener("click", () => setPanelOpen(!panelOpen, isNarrow()));
  panelClose.addEventListener("click", () => setPanelOpen(false, true));
  panel.addEventListener("keydown", (event) => {
    if (event.key === "Escape" && isNarrow()) {
      event.stopPropagation();
      setPanelOpen(false, true);
    }
  });

  // -- Events from the backend ------------------------------------------------

  onHardware((next) => {
    hardware = next;
    renderModelSection();
  });
  await listen<AssistantStatus>("assistant:status", ({ payload }) => applyStatus(payload));
  await listen<AssistantChatDeltaPayload>("assistant:chat-delta", ({ payload }) => {
    if (payload.request_id !== requestId || !activeTurn) return;
    if (payload.content) activeTurn.text += payload.content;
    if (payload.reasoning) activeTurn.reasoning += payload.reasoning;
    scheduleRender(activeTurn);
  });
  await listen<AssistantChatDonePayload>("assistant:chat-done", ({ payload }) => {
    if (payload.request_id !== requestId || !activeTurn) return;
    const turn = activeTurn;
    if (payload.finish_reason === "length" && !payload.stopped) {
      turn.note = "The answer was cut off because the context length or the maximum response length was reached.";
    }
    finishRequest(turn, payload.stopped ? "stopped" : "done");
  });
  await listen<AssistantChatErrorPayload>("assistant:chat-error", ({ payload }) => {
    if (payload.request_id !== requestId || !activeTurn) return;
    const turn = activeTurn;
    turn.error = payload.message || "The model returned an error.";
    log(`Assistant chat error: ${turn.error}`, "error");
    finishRequest(turn, "error");
  });
  // Desktop drops arrive as file paths through Tauri's own file-drop event (Transcribe uses it
  // too, so window file drop stays on). Rust reads the image and the usual downscaling applies.
  await listen<string[]>("tauri://file-drop", ({ payload }) => {
    if (!options.isPageActive() || !Array.isArray(payload) || payload.length === 0) return;
    // Reading the files is asynchronous; images dropped before New chat must not join the new one.
    const droppedIn = conversation;
    void (async () => {
      const files: File[] = [];
      const errors: string[] = [];
      for (const path of payload) {
        try {
          const dataUrl = await invoke<string>("assistant_read_dropped_image", { path });
          const blob = await (await fetch(dataUrl)).blob();
          files.push(new File([blob], path.split(/[\\/]/).pop() || "image", { type: blob.type }));
        } catch (error) {
          errors.push(String(error));
        }
      }
      if (droppedIn !== conversation) return;
      await attachFiles(files, droppedIn);
      if (errors.length > 0 && droppedIn === conversation) {
        composerError = [composerError, ...errors].filter(Boolean).join(" ");
        renderComposer();
      }
    })();
  });

  // Auto-load when the page becomes the visible one.
  let wasActive = options.isPageActive();
  new MutationObserver(() => {
    const active = options.isPageActive();
    if (active && !wasActive) {
      void autoLoadOnOpen();
      if (turns.length > 0) scrollToEnd();
    } else if (!active && wasActive) {
      // Leaving the page cancels a voice answer and closes the microphone.
      ptt.cancel();
      void mic.close();
    }
    wasActive = active;
    syncVoiceEntered();
  }).observe(section, { attributes: true, attributeFilter: ["class"] });

  renderAll();
  ensureHardware(log);
  await refreshStatus();
  // Read through a function: the compiler cannot see that refreshStatus() assigned `status`.
  if (platformSupported()) void prompts.load();
  if (wasActive) await autoLoadOnOpen();
}
