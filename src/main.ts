import { invoke } from "@tauri-apps/api/tauri";
import { emit, listen } from "@tauri-apps/api/event";
import {
  decodePcm16Base64ToFloat32,
  minPrebufferSeconds,
  prependSilence,
  rebufferSeconds,
} from "./playback";
import {
  RATE_UPDATED_EVENT,
  TOOLBAR_ACTION_EVENT,
  TOOLBAR_HIDE_EVENT,
  TOOLBAR_PAUSED_EVENT,
  TOOLBAR_SHOW_EVENT,
  TOOLBAR_SKIP_BACK_NOOP_EVENT,
  clampRate,
  formatRateNumber,
  rateFromPayload,
} from "./shared";
import "./styles.css";
import type {
  Audio8DownloadResult,
  Audio8ModelStatus,
  BootstrapPayload,
  CloneVoiceResult,
  ComputeDevicePayload,
  EngineStoragePathsPayload,
  HotkeyResult,
  JobCancelRequestedPayload,
  JobStartedPayload,
  JsonValue,
  ModelDownloadProgressPayload,
  ModelOption,
  ModelUpdatePayload,
  PrefetchModelsResult,
  QueuedPlayback,
  RuntimeStatusPayload,
  SpeakerPreset,
  StoredVoice,
  ThemeMode,
  ToolbarActionPayload,
  ToolbarPausePayload,
  ToolbarShowPayload,
  UnifiedVoiceOption,
} from "./types";

const AUDIO8_MODEL_ID = "audio8_tts_0_1b";
const CLONE_REF_TEXT_PLACEHOLDER_DEFAULT = "Optional transcript of the uploaded sample";
const CLONE_REF_TEXT_PLACEHOLDER_AUDIO8 = "Exact transcript of the uploaded sample (required for Audio8)";
const CLONE_HINT_DEFAULT = "Upload a short, clean reference clip to create and save a cloned voice profile.";
const CLONE_HINT_AUDIO8 =
  "Audio8 needs a 0.5 to 30 second clean reference clip plus its exact transcript to create and save a cloned voice profile.";

const VOICE_ORDINAL_STORAGE_KEY = "voicereader.saved_voice_ordinals.v1";
const THEME_STORAGE_KEY = "voicereader.theme.v1";

function readThemePreference(): ThemeMode {
  try {
    const raw = window.localStorage.getItem(THEME_STORAGE_KEY);
    return raw === "light" ? "light" : "dark";
  } catch {
    return "dark";
  }
}

let currentTheme: ThemeMode = readThemePreference();
document.documentElement.setAttribute("data-theme", currentTheme);

const app = document.querySelector<HTMLDivElement>("#app");
if (!app) {
  throw new Error("Missing app root");
}

app.innerHTML = `
  <main class="shell">
    <header class="hero">
      <div class="hero-left">
        <h1>VOICEREADER DESKTOP</h1>
        <button class="runtime runtime-btn" id="runtime-pill" type="button" title="Open engine diagnostics">Engine: checking...</button>
      </div>
      <button id="theme-toggle-btn" class="theme-toggle" type="button" aria-label="Switch theme" title="Switch theme">
        <span class="theme-toggle-track">
          <span class="theme-toggle-icon sun" aria-hidden="true">☀</span>
          <span class="theme-toggle-icon moon" aria-hidden="true">☾</span>
          <span class="theme-toggle-thumb" aria-hidden="true"></span>
        </span>
      </button>
    </header>

    <section class="tabs" role="tablist" aria-label="VoiceReader pages">
      <button class="tab active" data-tab="reader" role="tab" aria-selected="true">Reader</button>
      <button class="tab" data-tab="voices" role="tab" aria-selected="false">Voices & Clone</button>
      <button class="tab" data-tab="engine" role="tab" aria-selected="false">Engine</button>
    </section>

    <section class="panel active" id="reader-panel">
      <div class="grid">
        <article class="card">
          <h2>Quick Start</h2>
          <p class="hint">Use the global hotkey shown below after highlighting text in any app.</p>
          <div class="inline-row hotkey-row">
            <div class="hotkey" id="hotkey-pill">Loading hotkey...</div>
            <button id="hotkey-edit-btn">Edit</button>
          </div>
          <div class="row">
            <div class="inline-row hotkey-capture is-hidden" id="hotkey-capture-row">
              <input id="hotkey-input" placeholder="Click and press a shortcut" readonly />
              <button id="set-hotkey-btn">Set Hotkey</button>
              <button id="cancel-hotkey-btn">Cancel</button>
            </div>
            <p class="hint">Avoid OS-reserved combos such as Alt+Space (Windows) and Cmd+Space (macOS).</p>
          </div>

          <div class="row">
            <label for="model-select">Model Mode</label>
            <select id="model-select"></select>
          </div>

          <div class="row">
            <label for="voice-select">Available Voices</label>
            <select id="voice-select"></select>
          </div>

          <details class="advanced-settings">
            <summary>Advanced Settings</summary>
            <div class="controls">
              <label>Rate <input id="rate" type="number" min="0.25" max="4" step="0.05" value="1.5" /></label>
              <label>Volume <input id="volume" type="number" min="0" max="2" step="0.05" value="1" /></label>
              <label>Chunk Max Chars <input id="chunk-max" type="number" min="100" max="200" step="10" value="200" /></label>
            </div>
          </details>

          <div class="button-row">
            <button id="read-btn">Read Selection Now</button>
            <button id="cancel-btn">Cancel Active Job</button>
          </div>
        </article>

        <article class="card">
          <h2>Speak Test</h2>
          <p class="hint">This uses the same /speak -> WS pipeline as the hotkey flow.</p>
          <textarea id="speak-text" rows="6">This is VoiceReader app integration test text. If you hear this, the sidecar handshake and stream playback path are working end to end.</textarea>
          <div class="button-row">
            <button id="speak-btn" class="accent">Speak Text</button>
          </div>
        </article>
      </div>
    </section>

    <section class="panel" id="voices-panel">
      <div class="grid single">
        <article class="card">
          <h2>Clone Voice</h2>
          <p class="hint" id="clone-hint">Upload a short, clean reference clip to create and save a cloned voice profile.</p>
          <div class="clone-grid">
            <label>
              Voice Name
              <input id="clone-display-name" placeholder="My Voice" />
            </label>
            <label>
              Language Hint
              <input id="clone-language" placeholder="en" value="en" />
            </label>
            <label class="span-2">
              Reference Text (optional for Kyutai, required for Audio8)
              <textarea id="clone-ref-text" rows="2" placeholder="Optional transcript of the uploaded sample"></textarea>
            </label>
            <label class="span-2">
              Reference Audio File (WAV)
              <input id="clone-audio-file" type="file" accept=".wav,audio/wav" />
            </label>
            <div class="button-row span-2">
              <button id="clone-voice-btn" class="accent">Clone & Save Voice</button>
              <button id="refresh-voices-btn">Refresh Voices</button>
            </div>
            <p class="clone-feedback is-hidden span-2" id="clone-status" role="status" aria-live="polite"></p>
            <p class="hint span-2" id="clone-file-label">No file selected</p>
          </div>

          <h2>Voice Library</h2>
          <p class="hint">Preset + saved voices in one editable table. Save edits per row, and delete saved cloned voices.</p>
          <table>
            <thead>
              <tr>
                <th>Source</th>
                <th>Voice #</th>
                <th>Name</th>
                <th>Language</th>
                <th>Description</th>
                <th>Actions</th>
              </tr>
            </thead>
            <tbody id="voices-table"></tbody>
          </table>
        </article>
      </div>
    </section>

    <section class="panel" id="engine-panel">
      <div class="grid single">
        <article class="card" id="model-downloads-card">
          <h2>Model Downloads</h2>
          <p class="hint">Kyutai Pocket TTS is bundled. Use these actions to download Qwen models on demand.</p>
          <p class="hint mono" id="model-storage-paths">Storage: loading...</p>
          <div class="button-row engine-actions">
            <button id="download-qwen-custom-btn">Download Qwen CustomVoice</button>
            <button id="download-qwen-base-btn">Download Qwen Base</button>
            <button id="download-qwen-all-btn" class="accent">Download Both Qwen Models</button>
          </div>
          <p class="hint" id="model-download-status">No download in progress.</p>
        </article>
        <article class="card is-hidden" id="audio8-card">
          <h2>Audio8 TTS (optional download)</h2>
          <p class="hint">Multilingual model with voice cloning, about 860 MB. Runs on the CPU, and uses a GPU for audio decoding when one helps. English and Chinese are the primary languages.</p>
          <div class="button-row engine-actions">
            <button id="download-audio8-btn" class="accent">Download Audio8 model</button>
          </div>
          <p class="hint" id="audio8-status">Checking Audio8 model status...</p>
          <progress id="audio8-progress" class="download-progress is-hidden" max="100" value="0"></progress>
          <p class="hint mono is-hidden" id="audio8-progress-text"></p>
        </article>
        <article class="card is-hidden" id="compute-card">
          <h2>Compute Device</h2>
          <p class="hint">Where the heavy part of a model runs. Auto uses the GPU when one is available and faster than the CPU. Choose CPU to keep the GPU free for other work.</p>
          <div class="button-row engine-actions">
            <button class="compute-btn" data-compute="auto">Auto</button>
            <button class="compute-btn" data-compute="gpu">GPU</button>
            <button class="compute-btn" data-compute="cpu">CPU</button>
          </div>
          <p class="hint" id="compute-status"></p>
        </article>
        <article class="card">
          <h2>Engine Health</h2>
          <div class="button-row engine-actions">
            <button id="refresh-btn">Refresh Health</button>
            <button id="restart-btn">Restart Engine</button>
          </div>
          <pre id="health-json" class="json-box"></pre>
        </article>
        <article class="card">
          <h2>Activity</h2>
          <div class="log-wrap">
            <div id="log" class="log"></div>
          </div>
        </article>
      </div>
    </section>
  </main>
`;

const hotkeyPill = document.querySelector<HTMLDivElement>("#hotkey-pill")!;
const hotkeyCaptureRow = document.querySelector<HTMLDivElement>("#hotkey-capture-row")!;
const themeToggleBtn = document.querySelector<HTMLButtonElement>("#theme-toggle-btn")!;
const runtimePill = document.querySelector<HTMLButtonElement>("#runtime-pill")!;
const hotkeyInput = document.querySelector<HTMLInputElement>("#hotkey-input")!;
const hotkeyEditBtn = document.querySelector<HTMLButtonElement>("#hotkey-edit-btn")!;
const hotkeyCancelBtn = document.querySelector<HTMLButtonElement>("#cancel-hotkey-btn")!;
const setHotkeyBtn = document.querySelector<HTMLButtonElement>("#set-hotkey-btn")!;
const modelSelect = document.querySelector<HTMLSelectElement>("#model-select")!;
const voiceSelect = document.querySelector<HTMLSelectElement>("#voice-select")!;
const healthJson = document.querySelector<HTMLPreElement>("#health-json")!;
const voicesTable = document.querySelector<HTMLTableSectionElement>("#voices-table")!;
const speakText = document.querySelector<HTMLTextAreaElement>("#speak-text")!;
const logEl = document.querySelector<HTMLDivElement>("#log")!;
const cloneDisplayNameInput = document.querySelector<HTMLInputElement>("#clone-display-name")!;
const cloneLanguageInput = document.querySelector<HTMLInputElement>("#clone-language")!;
const cloneRefTextInput = document.querySelector<HTMLTextAreaElement>("#clone-ref-text")!;
const cloneAudioFileInput = document.querySelector<HTMLInputElement>("#clone-audio-file")!;
const cloneVoiceBtn = document.querySelector<HTMLButtonElement>("#clone-voice-btn")!;
const refreshVoicesBtn = document.querySelector<HTMLButtonElement>("#refresh-voices-btn")!;
const cloneStatus = document.querySelector<HTMLParagraphElement>("#clone-status")!;
const cloneFileLabel = document.querySelector<HTMLParagraphElement>("#clone-file-label")!;
const modelDownloadsCard = document.querySelector<HTMLElement>("#model-downloads-card")!;
const modelStoragePaths = document.querySelector<HTMLParagraphElement>("#model-storage-paths")!;
const modelDownloadStatus = document.querySelector<HTMLParagraphElement>("#model-download-status")!;
const downloadQwenCustomBtn = document.querySelector<HTMLButtonElement>("#download-qwen-custom-btn")!;
const downloadQwenBaseBtn = document.querySelector<HTMLButtonElement>("#download-qwen-base-btn")!;
const downloadQwenAllBtn = document.querySelector<HTMLButtonElement>("#download-qwen-all-btn")!;
const computeCard = document.querySelector<HTMLElement>("#compute-card")!;
const computeStatus = document.querySelector<HTMLParagraphElement>("#compute-status")!;
const computeButtons = Array.from(document.querySelectorAll<HTMLButtonElement>(".compute-btn"));
const audio8Card = document.querySelector<HTMLElement>("#audio8-card")!;
const audio8Status = document.querySelector<HTMLParagraphElement>("#audio8-status")!;
const audio8Progress = document.querySelector<HTMLProgressElement>("#audio8-progress")!;
const audio8ProgressText = document.querySelector<HTMLParagraphElement>("#audio8-progress-text")!;
const downloadAudio8Btn = document.querySelector<HTMLButtonElement>("#download-audio8-btn")!;
const cloneHint = document.querySelector<HTMLParagraphElement>("#clone-hint")!;

const rateInput = document.querySelector<HTMLInputElement>("#rate")!;
const volumeInput = document.querySelector<HTMLInputElement>("#volume")!;
const chunkMaxInput = document.querySelector<HTMLInputElement>("#chunk-max")!;

const refreshBtn = document.querySelector<HTMLButtonElement>("#refresh-btn")!;
const restartBtn = document.querySelector<HTMLButtonElement>("#restart-btn")!;
const readBtn = document.querySelector<HTMLButtonElement>("#read-btn")!;
const cancelBtn = document.querySelector<HTMLButtonElement>("#cancel-btn")!;
const speakBtn = document.querySelector<HTMLButtonElement>("#speak-btn")!;

let audioContext: AudioContext | null = null;
let playbackCursor = 0;
let runtimeWasDown = false;
const activeAudioSources = new Set<AudioBufferSourceNode>();
const suppressedJobIds = new Set<string>();
const playbackChunkCounts = new Map<string, number>();
const queuedPlaybackByJob = new Map<string, QueuedPlayback>();
let hasOutputPrimed = false;
let hasStartupSilenceInjected = false;
let currentPresetSpeakers: SpeakerPreset[] = [];
let currentSelectedModel = "";
let latestVoicesPayload: JsonValue = {};
let voiceOptionMap = new Map<string, UnifiedVoiceOption>();
const presetDescriptionOverrides = new Map<string, string>();
let savedVoiceOrdinals = loadSavedVoiceOrdinals();
let qwenEnabled = true;
let audio8Supported = false;
let audio8Downloaded = false;
let audio8Downloading = false;
let modelSwitchInFlight = false;
let pendingHotkeyCapture = "";
let cloneStatusTimeoutId: number | null = null;
let toolbarPaused = false;
let activeToolbarJobId = "";

applyTheme(currentTheme, false);

function log(message: string, level: "info" | "error" = "info"): void {
  const line = document.createElement("div");
  line.className = `line ${level}`;
  line.textContent = `${new Date().toLocaleTimeString()} | ${message}`;
  logEl.prepend(line);
}

function showCloneStatus(message: string, level: "info" | "success" | "error", autoHideMs = 6000): void {
  if (cloneStatusTimeoutId !== null) {
    window.clearTimeout(cloneStatusTimeoutId);
    cloneStatusTimeoutId = null;
  }
  cloneStatus.textContent = message;
  cloneStatus.classList.remove("is-hidden", "info", "success", "error");
  cloneStatus.classList.add(level);
  if (autoHideMs <= 0) {
    return;
  }
  cloneStatusTimeoutId = window.setTimeout(() => {
    cloneStatus.classList.add("is-hidden");
    cloneStatusTimeoutId = null;
  }, autoHideMs);
}

function applyCloneFormForModel(modelId: string): void {
  const isAudio8 = modelId === AUDIO8_MODEL_ID;
  cloneRefTextInput.placeholder = isAudio8 ? CLONE_REF_TEXT_PLACEHOLDER_AUDIO8 : CLONE_REF_TEXT_PLACEHOLDER_DEFAULT;
  cloneHint.textContent = isAudio8 ? CLONE_HINT_AUDIO8 : CLONE_HINT_DEFAULT;
}

function setActiveModel(modelId: string): void {
  currentSelectedModel = modelId;
  applyCloneFormForModel(modelId);
}

function themeToggleAriaLabel(theme: ThemeMode): string {
  return theme === "dark" ? "Switch to light mode" : "Switch to dark mode";
}

function applyTheme(theme: ThemeMode, persist = true): void {
  currentTheme = theme;
  document.documentElement.setAttribute("data-theme", theme);
  const label = themeToggleAriaLabel(theme);
  themeToggleBtn.setAttribute("aria-label", label);
  themeToggleBtn.setAttribute("title", label);
  if (!persist) {
    return;
  }
  try {
    window.localStorage.setItem(THEME_STORAGE_KEY, theme);
  } catch {
    // Ignore storage write errors.
  }
}

function setHotkeyDisplay(value: string): void {
  hotkeyPill.textContent = value;
  hotkeyInput.value = value;
}

function setHotkeyEditMode(enabled: boolean): void {
  hotkeyEditBtn.classList.toggle("is-hidden", enabled);
  hotkeyCaptureRow.classList.toggle("is-hidden", !enabled);
  if (enabled) {
    pendingHotkeyCapture = "";
    hotkeyInput.value = "";
    hotkeyInput.focus();
    hotkeyInput.select();
    return;
  }
  pendingHotkeyCapture = "";
}

function normalizeCapturedKey(event: KeyboardEvent): string | null {
  const raw = event.key;
  if (!raw) {
    return null;
  }

  if (["Control", "Shift", "Alt", "Meta", "OS", "AltGraph"].includes(raw)) {
    return null;
  }

  if (raw.length === 1) {
    const normalized = raw.toUpperCase();
    if (/^[A-Z0-9]$/.test(normalized)) {
      return normalized;
    }
  }

  if (/^F\d{1,2}$/.test(raw)) {
    return raw.toUpperCase();
  }

  if (raw === " ") {
    return "Space";
  }
  if (raw === "Escape") {
    return "Esc";
  }
  if (raw.startsWith("Arrow")) {
    return raw.slice(5);
  }

  return raw.length === 1 ? raw.toUpperCase() : raw;
}

function captureHotkeyFromEvent(event: KeyboardEvent): string | null {
  const key = normalizeCapturedKey(event);
  const parts: string[] = [];
  const hasAltGraph = event.getModifierState?.("AltGraph") ?? false;
  const metaDown = event.metaKey;
  let ctrlDown = event.ctrlKey;
  const altDown = event.altKey || hasAltGraph;
  const shiftDown = event.shiftKey;
  // Some keyboard layouts report AltGr as Ctrl+Alt. Treat it as Alt only.
  if (hasAltGraph) {
    ctrlDown = false;
  }

  if (metaDown) {
    parts.push("Cmd");
  }
  if (ctrlDown) {
    parts.push("Ctrl");
  }
  if (altDown) {
    parts.push("Alt");
  }
  if (shiftDown) {
    parts.push("Shift");
  }

  if (!key) {
    return parts.length > 0 ? parts.join("+") : null;
  }
  if (parts.length === 0) {
    return null;
  }
  parts.push(key);
  return parts.join("+");
}

function loadSavedVoiceOrdinals(): Map<string, number> {
  try {
    const raw = window.localStorage.getItem(VOICE_ORDINAL_STORAGE_KEY);
    if (!raw) {
      return new Map<string, number>();
    }
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    const entries = Object.entries(parsed)
      .map(([voiceId, value]) => [voiceId, Number(value)] as const)
      .filter(([voiceId, value]) => voiceId.length > 0 && Number.isInteger(value) && value >= 1);
    return new Map<string, number>(entries);
  } catch {
    return new Map<string, number>();
  }
}

function persistSavedVoiceOrdinals(): void {
  try {
    const payload = Object.fromEntries(savedVoiceOrdinals.entries());
    window.localStorage.setItem(VOICE_ORDINAL_STORAGE_KEY, JSON.stringify(payload));
  } catch {
    // Ignore storage write errors.
  }
}

function syncSavedVoiceOrdinals(storedVoices: StoredVoice[]): void {
  const activeIds = new Set(storedVoices.map((voice) => voice.voice_id));
  let changed = false;

  for (const existingId of Array.from(savedVoiceOrdinals.keys())) {
    if (!activeIds.has(existingId)) {
      savedVoiceOrdinals.delete(existingId);
      changed = true;
    }
  }

  const used = new Set<number>(savedVoiceOrdinals.values());
  for (const voice of storedVoices) {
    if (savedVoiceOrdinals.has(voice.voice_id)) {
      continue;
    }
    let next = 1;
    while (used.has(next)) {
      next += 1;
    }
    savedVoiceOrdinals.set(voice.voice_id, next);
    used.add(next);
    changed = true;
  }

  if (changed) {
    persistSavedVoiceOrdinals();
  }
}

function savedVoiceOrdinal(voiceId: string): number {
  return savedVoiceOrdinals.get(voiceId) ?? 0;
}

function activateTab(target: string): void {
  const tabs = Array.from(document.querySelectorAll<HTMLButtonElement>(".tab"));
  const panels = Array.from(document.querySelectorAll<HTMLElement>(".panel"));

  tabs.forEach((tab) => {
    const active = tab.dataset.tab === target;
    tab.classList.toggle("active", active);
    tab.setAttribute("aria-selected", String(active));
  });

  panels.forEach((panel) => {
    const panelId = panel.id.replace("-panel", "");
    panel.classList.toggle("active", panelId === target);
  });
}

function setTabs(): void {
  const tabs = Array.from(document.querySelectorAll<HTMLButtonElement>(".tab"));

  tabs.forEach((tab) => {
    tab.addEventListener("click", () => {
      const target = tab.dataset.tab;
      if (!target) {
        return;
      }
      activateTab(target);
    });
  });
}

function encodeJson(value: unknown): string {
  return JSON.stringify(value, null, 2);
}

function renderRuntimeStatus(status: RuntimeStatusPayload): void {
  if (status.running) {
    runtimePill.className = "runtime ok";
    // A local (in-process) runtime has no process id or address worth showing; say which
    // model is active and what it runs on instead.
    const parts = [status.pid != null ? `Engine: running (pid=${status.pid}) @ ${status.base_url}` : "Engine: running"];
    if (status.model_label) {
      parts.push(status.model_label);
    }
    if (status.device_label) {
      parts.push(status.device_label);
    }
    runtimePill.textContent = parts.join(" · ");
    runtimeWasDown = false;
    return;
  }

  runtimePill.className = "runtime down";
  runtimePill.textContent = "Engine: down";
  if (!runtimeWasDown) {
    log("Engine sidecar is not running. Use Restart Engine or trigger a read action.", "error");
    runtimeWasDown = true;
  }
}

async function pollRuntimeStatus(): Promise<void> {
  const status = await invoke<RuntimeStatusPayload>("engine_runtime_status");
  renderRuntimeStatus(status);
}

function parseStoredVoices(voicesPayload: JsonValue): StoredVoice[] {
  const voices = Array.isArray(voicesPayload.voices) ? voicesPayload.voices : [];
  return voices
    .map((raw) => ({
      voice_id: String((raw as Record<string, unknown>).voice_id ?? ""),
      display_name: String((raw as Record<string, unknown>).display_name ?? "Unknown"),
      language_hint: String((raw as Record<string, unknown>).language_hint ?? ""),
      description: String((raw as Record<string, unknown>).description ?? ""),
    }))
    .filter((item) => item.voice_id.length > 0);
}

function orderedSavedVoices(): StoredVoice[] {
  const storedVoices = parseStoredVoices(latestVoicesPayload).filter((voice) => voice.voice_id !== "0");
  syncSavedVoiceOrdinals(storedVoices);
  return [...storedVoices].sort((a, b) => savedVoiceOrdinal(a.voice_id) - savedVoiceOrdinal(b.voice_id));
}

function buildUnifiedVoiceOptions(): UnifiedVoiceOption[] {
  const options: UnifiedVoiceOption[] = [];

  for (const speaker of currentPresetSpeakers) {
    options.push({
      value: `preset:${speaker.id}`,
      label: `${speaker.id} (Built-in)`,
      kind: "preset",
      id: speaker.id,
    });
  }

  const savedVoices = orderedSavedVoices();
  for (let idx = 0; idx < savedVoices.length; idx += 1) {
    const voice = savedVoices[idx];
    const ordinal = savedVoiceOrdinal(voice.voice_id) || idx + 1;
    const language = voice.language_hint ? ` [${voice.language_hint}]` : "";
    options.push({
      value: `voice:${voice.voice_id}`,
      label: `Voice ${ordinal}: ${voice.display_name}${language}`,
      kind: "stored",
      id: voice.voice_id,
    });
  }

  return options;
}

function preferredVoiceOption(selectedVoiceId: string, selectedSpeaker: string): string | null {
  if (selectedVoiceId && selectedVoiceId !== "0") {
    const candidate = `voice:${selectedVoiceId}`;
    if (voiceOptionMap.has(candidate)) {
      return candidate;
    }
  }

  if (selectedSpeaker) {
    const candidate = `preset:${selectedSpeaker}`;
    if (voiceOptionMap.has(candidate)) {
      return candidate;
    }
  }

  return null;
}

function renderUnifiedVoiceOptions(selectedVoiceId: string, selectedSpeaker: string): void {
  const options = buildUnifiedVoiceOptions();
  voiceOptionMap = new Map(options.map((item) => [item.value, item]));

  voiceSelect.innerHTML = "";
  for (const optionItem of options) {
    const option = document.createElement("option");
    option.value = optionItem.value;
    option.textContent = optionItem.label;
    voiceSelect.append(option);
  }

  const preferred = preferredVoiceOption(selectedVoiceId, selectedSpeaker);
  if (preferred && voiceOptionMap.has(preferred)) {
    voiceSelect.value = preferred;
    return;
  }
  if (options.length > 0) {
    voiceSelect.value = options[0].value;
  }
}

function ensureAudioContext(): AudioContext {
  if (!audioContext) {
    audioContext = new AudioContext();
    playbackCursor = audioContext.currentTime;
  }
  return audioContext;
}

function selectedRateSetting(): number {
  const parsed = Number(rateInput.value);
  if (!Number.isFinite(parsed)) {
    return 1;
  }
  return clampRate(parsed);
}

function sourceLabelFromWindowTitle(sourceWindow: string): string {
  const trimmed = sourceWindow.trim();
  if (!trimmed) {
    return "Reading aloud...";
  }
  const segments = trimmed
    .split(" - ")
    .map((segment) => segment.trim())
    .filter((segment) => segment.length > 0);
  return segments[segments.length - 1] ?? trimmed;
}

function showToolbar(sourceWindow: string, rate: number): void {
  toolbarPaused = false;
  const payload: ToolbarShowPayload = {
    job_id: activeToolbarJobId,
    source_window: sourceLabelFromWindowTitle(sourceWindow),
    rate,
  };
  void emit(TOOLBAR_SHOW_EVENT, payload);
  void emit(TOOLBAR_PAUSED_EVENT, { paused: false } satisfies ToolbarPausePayload);
}

function hideToolbar(): void {
  toolbarPaused = false;
  activeToolbarJobId = "";
  void emit(TOOLBAR_HIDE_EVENT, {});
}

async function togglePlaybackPause(): Promise<void> {
  const context = ensureAudioContext();
  if (toolbarPaused) {
    if (context.state === "suspended") {
      await context.resume();
    }
    toolbarPaused = false;
  } else {
    if (context.state !== "suspended") {
      await context.suspend();
    }
    toolbarPaused = true;
  }
  void emit(TOOLBAR_PAUSED_EVENT, { paused: toolbarPaused } satisfies ToolbarPausePayload);
}

function playbackIsIdle(): boolean {
  return activeAudioSources.size === 0 && queuedPlaybackByJob.size === 0;
}

function playbackHasRunDry(): boolean {
  // Everything scheduled so far has already finished playing. While paused the audio
  // clock is frozen, so a pause is never mistaken for running dry.
  return audioContext !== null && playbackCursor <= audioContext.currentTime;
}

function ensureQueuedPlayback(jobId: string): QueuedPlayback {
  const existing = queuedPlaybackByJob.get(jobId);
  if (existing) {
    return existing;
  }
  const created: QueuedPlayback = {
    buffers: [],
    bufferedSeconds: 0,
    started: false,
    terminal: false,
    rebufferCount: 0,
  };
  queuedPlaybackByJob.set(jobId, created);
  return created;
}

function clearQueuedPlayback(): void {
  queuedPlaybackByJob.clear();
}

function nextChunkLeadSeconds(jobId: string): number {
  const DEFAULT_LEAD_SECONDS = 0.015;
  const FIRST_CHUNK_LEAD_SECONDS = 0.1;
  const FIRST_OUTPUT_PREROLL_SECONDS = 0.14;

  let lead = DEFAULT_LEAD_SECONDS;
  if (jobId) {
    const currentCount = playbackChunkCounts.get(jobId) ?? 0;
    if (currentCount === 0) {
      lead = FIRST_CHUNK_LEAD_SECONDS;
    }
    playbackChunkCounts.set(jobId, currentCount + 1);
  }

  if (!hasOutputPrimed) {
    lead += FIRST_OUTPUT_PREROLL_SECONDS;
    hasOutputPrimed = true;
  }
  return lead;
}

function scheduleAudioBuffer(jobId: string, buffer: AudioBuffer): void {
  const context = ensureAudioContext();
  const source = context.createBufferSource();
  source.buffer = buffer;
  source.connect(context.destination);
  activeAudioSources.add(source);
  source.onended = () => {
    activeAudioSources.delete(source);
    if (jobId !== activeToolbarJobId) {
      return;
    }
    if (playbackIsIdle()) {
      resetPlaybackCursor();
      hideToolbar();
    }
  };

  const now = context.currentTime;
  const leadSeconds = nextChunkLeadSeconds(jobId);
  const startAt = Math.max(playbackCursor, now + leadSeconds);
  source.start(startAt);
  playbackCursor = startAt + buffer.duration;
}

function flushQueuedPlayback(jobId: string, forceStart: boolean): void {
  const queued = queuedPlaybackByJob.get(jobId);
  if (!queued) {
    return;
  }

  if (queued.started && !forceStart && !queued.terminal && queued.buffers.length > 0 && playbackHasRunDry()) {
    // Audio ran out before this chunk arrived. Playing each late chunk as it lands
    // sounds choppy, so hold playback and refill the buffer first.
    queued.started = false;
    queued.rebufferCount += 1;
    log(
      `playback_rebuffer job_id=${jobId} count=${queued.rebufferCount} wait_for=${rebufferSeconds(selectedRateSetting(), queued.rebufferCount).toFixed(2)}s`,
    );
  }

  if (!queued.started) {
    const rate = selectedRateSetting();
    const requiredSeconds =
      queued.rebufferCount > 0 ? rebufferSeconds(rate, queued.rebufferCount) : minPrebufferSeconds(rate);
    if (!forceStart && queued.bufferedSeconds < requiredSeconds) {
      return;
    }
    queued.started = true;
  }

  while (queued.buffers.length > 0) {
    const buffer = queued.buffers.shift();
    if (!buffer) {
      continue;
    }
    scheduleAudioBuffer(jobId, buffer);
    queued.bufferedSeconds = Math.max(0, queued.bufferedSeconds - buffer.duration);
  }

  if (queued.terminal && queued.buffers.length === 0) {
    queuedPlaybackByJob.delete(jobId);
  }
}

async function fileToBase64(file: File): Promise<string> {
  const arrayBuffer = await file.arrayBuffer();
  const bytes = new Uint8Array(arrayBuffer);
  let binary = "";
  for (let idx = 0; idx < bytes.length; idx += 1) {
    binary += String.fromCharCode(bytes[idx]);
  }
  return btoa(binary);
}

async function enqueueAudioChunk(eventPayload: Record<string, unknown>): Promise<void> {
  const audio = eventPayload.audio as Record<string, unknown> | undefined;
  if (!audio) {
    return;
  }

  const dataBase64 = String(audio.data_base64 ?? "");
  if (!dataBase64) {
    return;
  }

  const sampleRate = Number(audio.sample_rate ?? 24000);
  const channels = Number(audio.channels ?? 1);
  if (channels !== 1) {
    log(`Received channels=${channels}; only mono playback is currently handled`, "error");
    return;
  }

  const context = ensureAudioContext();
  if (context.state === "suspended" && !toolbarPaused) {
    await context.resume();
  }

  let samples = decodePcm16Base64ToFloat32(dataBase64);
  if (!hasStartupSilenceInjected) {
    // The first device wake-up can clip a short prefix; prepend silence once.
    samples = prependSilence(samples, sampleRate, 160);
    hasStartupSilenceInjected = true;
  }
  // Normalize to a plain ArrayBuffer-backed Float32Array for strict DOM typings.
  const channelSamples = new Float32Array(samples);
  const buffer = context.createBuffer(1, samples.length, sampleRate);
  buffer.copyToChannel(channelSamples, 0, 0);
  const jobId = String(eventPayload.job_id ?? "");
  if (!jobId) {
    scheduleAudioBuffer(jobId, buffer);
    return;
  }

  const queued = ensureQueuedPlayback(jobId);
  queued.buffers.push(buffer);
  queued.bufferedSeconds += buffer.duration;
  flushQueuedPlayback(jobId, false);
}

function resetPlaybackCursor(): void {
  if (!audioContext) {
    return;
  }
  playbackCursor = audioContext.currentTime;
}

function stopActiveSources(): void {
  for (const source of Array.from(activeAudioSources)) {
    try {
      source.stop(0);
    } catch {
      // no-op: source may have already ended
    }
    try {
      source.disconnect();
    } catch {
      // no-op
    }
  }
  activeAudioSources.clear();
}

function stopAllPlayback(): void {
  stopActiveSources();
  clearQueuedPlayback();
  playbackChunkCounts.clear();
  resetPlaybackCursor();
}

function skipPlaybackForward(): void {
  if (!activeToolbarJobId) {
    return;
  }
  stopActiveSources();
  resetPlaybackCursor();
  flushQueuedPlayback(activeToolbarJobId, true);
  if (playbackIsIdle()) {
    hideToolbar();
  }
}

async function toolbarStopPlayback(): Promise<void> {
  stopAllPlayback();
  await invoke("cancel_active_job");
  hideToolbar();
}

async function refreshHealthAndVoices(): Promise<void> {
  const [health, voices, runtime] = await Promise.all([
    invoke<JsonValue>("engine_health"),
    invoke<JsonValue>("engine_list_voices"),
    invoke<RuntimeStatusPayload>("engine_runtime_status"),
  ]);

  healthJson.textContent = encodeJson(health);
  latestVoicesPayload = voices;
  setActiveModel(runtime.selected_model);
  if (Array.from(modelSelect.options).some((option) => option.value === runtime.selected_model)) {
    modelSelect.value = runtime.selected_model;
  }
  renderUnifiedVoiceOptions(runtime.selected_voice_id, runtime.selected_speaker);
  renderVoicesTable();
}

function renderStoragePaths(paths: EngineStoragePathsPayload): void {
  modelStoragePaths.textContent = `Data: ${paths.data_dir} | Models: ${paths.models_dir} | HF cache: ${paths.hf_cache_dir}`;
}

async function refreshEngineStoragePaths(): Promise<void> {
  try {
    renderStoragePaths(await invoke<EngineStoragePathsPayload>("engine_storage_paths"));
  } catch (error) {
    modelStoragePaths.textContent = `Storage: unavailable (${String(error)})`;
  }
}

function applyBuildCapabilities(payload: BootstrapPayload): void {
  qwenEnabled = payload.qwen_enabled;
  modelDownloadsCard.classList.toggle("is-hidden", !qwenEnabled);
  downloadQwenCustomBtn.disabled = !qwenEnabled;
  downloadQwenBaseBtn.disabled = !qwenEnabled;
  downloadQwenAllBtn.disabled = !qwenEnabled;
  if (!qwenEnabled) {
    modelDownloadStatus.textContent = "Qwen downloads are disabled in Base build.";
  }

  audio8Supported = payload.audio8_supported === true;
  audio8Downloaded = audio8Supported && payload.audio8_downloaded === true;
  audio8Card.classList.toggle("is-hidden", !audio8Supported);
  renderAudio8Card();
}

function renderAudio8Card(): void {
  if (!audio8Supported) {
    return;
  }
  if (audio8Downloading) {
    downloadAudio8Btn.disabled = true;
    return;
  }
  if (audio8Downloaded) {
    downloadAudio8Btn.disabled = true;
    downloadAudio8Btn.textContent = "Downloaded";
    audio8Status.textContent = "Audio8 TTS is downloaded and ready. Select it from Model Mode on the Reader tab.";
    audio8Progress.classList.add("is-hidden");
    audio8ProgressText.classList.add("is-hidden");
    return;
  }
  downloadAudio8Btn.disabled = false;
  downloadAudio8Btn.textContent = "Download Audio8 model";
  if (!audio8Status.textContent || audio8Status.textContent.startsWith("Checking")) {
    audio8Status.textContent = "Not downloaded yet.";
  }
}

function formatMegabytes(bytes: number): number {
  return Math.round(bytes / (1024 * 1024));
}

function renderAudio8DownloadProgress(payload: ModelDownloadProgressPayload): void {
  const total = Number(payload.total_bytes) || 0;
  const done = Number(payload.downloaded_bytes) || 0;
  const percent = total > 0 ? Math.min(100, Math.floor((done / total) * 100)) : 0;
  audio8Progress.classList.remove("is-hidden");
  audio8ProgressText.classList.remove("is-hidden");
  audio8Progress.value = percent;
  const fileInfo = payload.file ? ` \u2014 ${payload.file} (${payload.file_index}/${payload.file_count})` : "";
  audio8ProgressText.textContent = `${percent}% (${formatMegabytes(done)} / ${formatMegabytes(total)} MB)${fileInfo}`;
}

async function refreshAudio8Status(): Promise<void> {
  if (!audio8Supported) {
    return;
  }
  try {
    const status = await invoke<Audio8ModelStatus>("audio8_model_status");
    audio8Supported = status.supported;
    audio8Downloaded = status.supported && status.downloaded;
    audio8Card.classList.toggle("is-hidden", !audio8Supported);
    if (!audio8Downloaded && !audio8Downloading) {
      const sizeMb = formatMegabytes(status.download_size_bytes);
      audio8Status.textContent = sizeMb > 0 ? `Not downloaded yet (about ${sizeMb} MB).` : "Not downloaded yet.";
    }
    renderAudio8Card();
  } catch (error) {
    log(`Audio8 status check failed: ${String(error)}`, "error");
  }
}

function renderComputeDevice(payload: ComputeDevicePayload): void {
  computeCard.classList.toggle("is-hidden", !payload.gpu_supported);
  for (const button of computeButtons) {
    button.classList.toggle("accent", button.dataset.compute === payload.preference);
  }
  if (!payload.model_loaded || !payload.active_device) {
    computeStatus.textContent =
      "Takes effect when Audio8 TTS is loaded. Kyutai Pocket TTS always runs on the CPU.";
    return;
  }
  const where = payload.active_device === "cpu" ? "the CPU" : `the GPU (${payload.active_device})`;
  computeStatus.textContent = `Audio8 is decoding audio on ${where}${payload.note ? ` — ${payload.note}` : ""}.`;
}

async function refreshComputeDevice(): Promise<void> {
  try {
    renderComputeDevice(await invoke<ComputeDevicePayload>("get_compute_device"));
  } catch (error) {
    log(`Compute device status check failed: ${String(error)}`, "error");
  }
}

async function chooseComputeDevice(preference: string): Promise<void> {
  for (const button of computeButtons) {
    button.disabled = true;
  }
  computeStatus.textContent = "Applying... a loaded model is reloaded, which takes a few seconds.";
  try {
    const payload = await invoke<ComputeDevicePayload>("set_compute_device", { preference });
    renderComputeDevice(payload);
    log(`Compute device set to ${payload.preference}${payload.active_device ? ` (now using ${payload.active_device})` : ""}`);
    await refreshHealthAndVoices();
  } catch (error) {
    computeStatus.textContent = `Could not change compute device: ${String(error)}`;
    log(`Failed to set compute device: ${String(error)}`, "error");
    await refreshComputeDevice();
  } finally {
    for (const button of computeButtons) {
      button.disabled = false;
    }
  }
}

async function refreshModelOptionsFromBootstrap(): Promise<void> {
  const payload = await invoke<BootstrapPayload>("app_bootstrap");
  applyBuildCapabilities(payload);
  renderModelOptions(payload.models, currentSelectedModel || payload.selected_model);
}

function setModelDownloadBusy(isBusy: boolean): void {
  downloadQwenCustomBtn.disabled = isBusy;
  downloadQwenBaseBtn.disabled = isBusy;
  downloadQwenAllBtn.disabled = isBusy;
}

function renderModelOptions(models: ModelOption[], selectedModel: string): void {
  modelSelect.innerHTML = "";
  for (const model of models) {
    const option = document.createElement("option");
    option.value = model.id;
    option.textContent = `${model.label} | ${model.status}`;
    modelSelect.append(option);
  }
  modelSelect.value = selectedModel;
}

function renderVoicesTable(): void {
  voicesTable.innerHTML = "";
  const savedVoices = orderedSavedVoices();

  const hasRows = currentPresetSpeakers.length > 0 || savedVoices.length > 0;
  if (!hasRows) {
    const row = document.createElement("tr");
    row.innerHTML = `<td colspan="6" class="hint">No voices available.</td>`;
    voicesTable.append(row);
    return;
  }

  for (const speaker of currentPresetSpeakers) {
    const row = document.createElement("tr");

    const sourceCell = document.createElement("td");
    sourceCell.textContent = "Preset";

    const idCell = document.createElement("td");
    idCell.textContent = speaker.id;

    const nameCell = document.createElement("td");
    nameCell.textContent = speaker.id;

    const languageCell = document.createElement("td");
    languageCell.textContent = speaker.native_language;

    const descCell = document.createElement("td");
    const descInput = document.createElement("input");
    descInput.value = presetDescriptionOverrides.get(speaker.id) ?? speaker.description;
    descInput.className = "table-input";
    descCell.append(descInput);

    const actionsCell = document.createElement("td");
    const saveBtn = document.createElement("button");
    saveBtn.textContent = "Save";
    saveBtn.className = "table-action";
    saveBtn.addEventListener("click", () => {
      presetDescriptionOverrides.set(speaker.id, descInput.value.trim());
      log(`Updated preset description for ${speaker.id}`);
    });
    const deleteBtn = document.createElement("button");
    deleteBtn.textContent = "Delete";
    deleteBtn.className = "table-action danger";
    deleteBtn.disabled = true;
    deleteBtn.title = "Built-in preset rows cannot be deleted";
    actionsCell.append(saveBtn, deleteBtn);

    row.append(sourceCell, idCell, nameCell, languageCell, descCell, actionsCell);
    voicesTable.append(row);
  }

  for (let idx = 0; idx < savedVoices.length; idx += 1) {
    const voice = savedVoices[idx];
    const ordinal = savedVoiceOrdinal(voice.voice_id) || idx + 1;
    const row = document.createElement("tr");

    const sourceCell = document.createElement("td");
    sourceCell.textContent = "Saved";

    const idCell = document.createElement("td");
    idCell.textContent = String(ordinal);
    idCell.title = `Internal ID: ${voice.voice_id}`;

    const nameCell = document.createElement("td");
    const nameInput = document.createElement("input");
    nameInput.className = "table-input";
    nameInput.value = voice.display_name;
    nameCell.append(nameInput);

    const languageCell = document.createElement("td");
    const languageInput = document.createElement("input");
    languageInput.className = "table-input";
    languageInput.value = voice.language_hint ?? "";
    languageInput.placeholder = "auto / en";
    languageCell.append(languageInput);

    const descCell = document.createElement("td");
    const descInput = document.createElement("input");
    descInput.className = "table-input";
    descInput.value = voice.description ?? "";
    descInput.placeholder = "Add voice description";
    descCell.append(descInput);

    const actionsCell = document.createElement("td");
    const saveBtn = document.createElement("button");
    saveBtn.textContent = "Save";
    saveBtn.className = "table-action";

    saveBtn.addEventListener("click", async () => {
      const displayName = nameInput.value.trim();
      if (!displayName) {
        log("Voice name cannot be empty", "error");
        return;
      }

      saveBtn.disabled = true;
      try {
        await invoke("update_saved_voice", {
          voiceId: voice.voice_id,
          displayName,
          language: languageInput.value.trim() || null,
          description: descInput.value.trim() || null,
        });
        await refreshHealthAndVoices();
        log(`Saved voice updated: ${displayName}`);
      } catch (error) {
        log(`Failed to update voice ${voice.voice_id}: ${String(error)}`, "error");
      } finally {
        saveBtn.disabled = false;
      }
    });

    const deleteBtn = document.createElement("button");
    deleteBtn.textContent = "Delete";
    deleteBtn.className = "table-action danger";
    deleteBtn.title = "Delete saved voice";
    deleteBtn.addEventListener("click", async () => {
      const voiceLabel = nameInput.value.trim() || voice.voice_id;
      if (!window.confirm(`Delete saved voice "${voiceLabel}"?`)) {
        return;
      }
      deleteBtn.disabled = true;
      try {
        await invoke("delete_saved_voice", { voiceId: voice.voice_id });
        await refreshHealthAndVoices();
        log(`Deleted saved voice: ${voiceLabel}`);
      } catch (error) {
        log(`Failed to delete voice ${voice.voice_id}: ${String(error)}`, "error");
      } finally {
        deleteBtn.disabled = false;
      }
    });
    actionsCell.append(saveBtn, deleteBtn);

    row.append(sourceCell, idCell, nameCell, languageCell, descCell, actionsCell);
    voicesTable.append(row);
  }
}

async function applySpeakSettings(): Promise<void> {
  const rate = Number(rateInput.value);
  const volume = Number(volumeInput.value);
  const chunkMaxChars = Number(chunkMaxInput.value);

  await invoke("set_speak_settings", {
    rate,
    volume,
    chunkMaxChars,
  });
}

async function bootstrap(): Promise<void> {
  const payload = await invoke<BootstrapPayload>("app_bootstrap");

  applyBuildCapabilities(payload);
  setHotkeyDisplay(payload.hotkey);
  setHotkeyEditMode(false);
  renderModelOptions(payload.models, payload.selected_model);
  currentPresetSpeakers = payload.preset_speakers;
  setActiveModel(payload.selected_model);

  healthJson.textContent = encodeJson(payload.health);
  latestVoicesPayload = payload.voices;

  renderUnifiedVoiceOptions(payload.selected_voice_id, payload.selected_speaker);
  renderVoicesTable();

  if (payload.startup_error) {
    log(`Startup warning: ${payload.startup_error}`, "error");
    log("Bootstrap completed with warnings");
  } else {
    if (payload.build_variant === "base") {
      log("Rust Kyutai runtime started and ready");
    } else {
      log("Engine sidecar started and handshake completed");
    }
  }
  const buildSuffix = payload.qwen_enabled
    ? " (Qwen enabled)"
    : payload.audio8_supported
      ? " (Kyutai + optional Audio8)"
      : " (Kyutai only)";
  log(`Build variant: ${payload.build_variant}${buildSuffix}`);

  await pollRuntimeStatus();
  await refreshEngineStoragePaths();
  await refreshAudio8Status();
  await refreshComputeDevice();
}

async function applyModelUpdate(result: ModelUpdatePayload): Promise<void> {
  currentPresetSpeakers = result.preset_speakers;
  setActiveModel(result.selected_model);
  await invoke("set_selected_voice", { voiceId: "0" });
  await refreshHealthAndVoices();
}

async function invokeAndLog(command: string, fallbackMessage: string, args?: Record<string, unknown>): Promise<void> {
  const response = await invoke<Record<string, unknown>>(command, args);
  log(String(response.message ?? fallbackMessage));
}

async function bindActions(): Promise<void> {
  runtimePill.addEventListener("click", () => {
    activateTab("engine");
  });

  themeToggleBtn.addEventListener("click", () => {
    const nextTheme: ThemeMode = currentTheme === "dark" ? "light" : "dark";
    applyTheme(nextTheme);
  });

  modelSelect.addEventListener("change", async () => {
    if (modelSwitchInFlight) {
      return;
    }
    const requestedModel = modelSelect.value;
    const previousModel = currentSelectedModel;
    modelSwitchInFlight = true;
    modelSelect.disabled = true;
    if (requestedModel === AUDIO8_MODEL_ID) {
      log("Loading Audio8 TTS... the first load can take several seconds.");
    }
    try {
      const result = await invoke<ModelUpdatePayload>("select_model", { model: requestedModel });
      await applyModelUpdate(result);
      await refreshComputeDevice();
      log(result.message ?? "Model updated");
    } catch (error) {
      log(`Failed to select model: ${String(error)}`, "error");
      if (previousModel && Array.from(modelSelect.options).some((option) => option.value === previousModel)) {
        modelSelect.value = previousModel;
      }
      applyCloneFormForModel(currentSelectedModel);
    } finally {
      modelSwitchInFlight = false;
      modelSelect.disabled = false;
    }
  });

  hotkeyEditBtn.addEventListener("click", () => {
    setHotkeyEditMode(true);
  });

  hotkeyCancelBtn.addEventListener("click", () => {
    setHotkeyEditMode(false);
  });

  hotkeyInput.addEventListener("keydown", (event) => {
    event.preventDefault();

    if (event.key === "Escape") {
      setHotkeyEditMode(false);
      return;
    }

    const captured = captureHotkeyFromEvent(event);
    if (!captured) {
      return;
    }
    pendingHotkeyCapture = captured;
    hotkeyInput.value = captured;
  });

  setHotkeyBtn.addEventListener("click", async () => {
    const candidate = pendingHotkeyCapture.trim();
    if (!candidate) {
      log("Press a key combination first, then click Set Hotkey", "error");
      return;
    }

    try {
      const result = await invoke<HotkeyResult>("set_hotkey", { hotkey: candidate });
      setHotkeyDisplay(result.hotkey);
      setHotkeyEditMode(false);
      log(result.message ?? `Hotkey set to ${result.hotkey}`);
    } catch (error) {
      log(`Failed to update hotkey: ${String(error)}`, "error");
    }
  });

  voiceSelect.addEventListener("change", async () => {
    const selected = voiceOptionMap.get(voiceSelect.value);
    if (!selected) {
      return;
    }

    if (selected.kind === "preset") {
      const result = await invoke<ModelUpdatePayload>("set_preset_speaker", {
        speakerId: selected.id,
      });
      await applyModelUpdate(result);
      log(`Selected built-in voice ${selected.id}`);
      return;
    }

    await invoke("set_selected_voice", { voiceId: selected.id });
    log(`Selected saved voice ${selected.label}`);
  });

  [rateInput, volumeInput, chunkMaxInput].forEach((input) => {
    input.addEventListener("change", async () => {
      await applySpeakSettings();
      log("Speak settings updated");
    });
  });

  refreshBtn.addEventListener("click", async () => {
    await refreshHealthAndVoices();
    await refreshEngineStoragePaths();
    log("Health and voice list refreshed");
  });

  refreshVoicesBtn.addEventListener("click", async () => {
    await refreshHealthAndVoices();
    log("Voice list refreshed");
  });

  cloneAudioFileInput.addEventListener("change", () => {
    const file = cloneAudioFileInput.files?.[0];
    cloneFileLabel.textContent = file ? `Selected file: ${file.name}` : "No file selected";
  });

  cloneVoiceBtn.addEventListener("click", async () => {
    const selectedFile = cloneAudioFileInput.files?.[0];
    if (!selectedFile) {
      showCloneStatus("Select an audio file before cloning.", "error");
      log("Select an audio file before cloning", "error");
      return;
    }
    if (!selectedFile.name.toLowerCase().endsWith(".wav")) {
      showCloneStatus("Only WAV files are supported for cloning in this UI.", "error");
      log("Only WAV files are supported for cloning in this UI right now", "error");
      return;
    }

    const displayName = cloneDisplayNameInput.value.trim() || selectedFile.name.replace(/\.[^/.]+$/, "");
    const language = cloneLanguageInput.value.trim();
    const refText = cloneRefTextInput.value.trim();
    if (currentSelectedModel === AUDIO8_MODEL_ID && !refText) {
      showCloneStatus("Audio8 needs the exact transcript of the reference clip. Fill in Reference Text first.", "error");
      log("Audio8 voice cloning requires the exact reference transcript", "error");
      return;
    }

    cloneVoiceBtn.disabled = true;
    showCloneStatus("Cloning voice... this can take a few seconds.", "info", 0);
    try {
      const wavBase64 = await fileToBase64(selectedFile);
      const result = await invoke<CloneVoiceResult>("clone_voice_from_audio", {
        displayName,
        wavBase64,
        language: language || null,
        refText: refText || null,
      });
      await refreshHealthAndVoices();
      const clonedOptionValue = `voice:${result.voice_id}`;
      if (voiceSelect.querySelector(`option[value="${clonedOptionValue}"]`)) {
        voiceSelect.value = clonedOptionValue;
        await invoke("set_selected_voice", { voiceId: result.voice_id });
      }
      const successMessage = result.message || `Voice cloned successfully: ${displayName}`;
      showCloneStatus(successMessage, "success");
      log(result.message || `Cloned voice saved: ${result.voice_id}`);
    } catch (error) {
      showCloneStatus(`Clone failed: ${String(error)}`, "error");
      log(`Clone failed: ${String(error)}`, "error");
    } finally {
      cloneVoiceBtn.disabled = false;
    }
  });

  restartBtn.addEventListener("click", async () => {
    const response = await invoke<Record<string, unknown>>("restart_engine");
    await refreshHealthAndVoices();
    await refreshComputeDevice();
    await refreshEngineStoragePaths();
    await pollRuntimeStatus();
    log(String(response.message ?? "Engine restarted"));
  });

  const runModelPrefetch = async (mode: "qwen_custom" | "qwen_base" | "qwen_all"): Promise<void> => {
    if (!qwenEnabled) {
      modelDownloadStatus.textContent = "Qwen downloads are disabled in Base build.";
      log("Qwen model downloads are disabled in Base build.", "error");
      return;
    }
    setModelDownloadBusy(true);
    modelDownloadStatus.textContent = `Downloading models (${mode})... this can take several minutes.`;
    try {
      const result = await invoke<PrefetchModelsResult>("prefetch_models", { mode });
      modelDownloadStatus.textContent = `Download complete: ${result.downloaded.join(", ")}`;
      renderStoragePaths(result);
      log(result.message || `Model prefetch complete (${mode})`);
    } catch (error) {
      modelDownloadStatus.textContent = `Download failed: ${String(error)}`;
      log(`Model prefetch failed (${mode}): ${String(error)}`, "error");
    } finally {
      setModelDownloadBusy(false);
    }
  };

  downloadQwenCustomBtn.addEventListener("click", async () => {
    await runModelPrefetch("qwen_custom");
  });
  downloadQwenBaseBtn.addEventListener("click", async () => {
    await runModelPrefetch("qwen_base");
  });
  downloadQwenAllBtn.addEventListener("click", async () => {
    await runModelPrefetch("qwen_all");
  });

  for (const button of computeButtons) {
    button.addEventListener("click", async () => {
      await chooseComputeDevice(button.dataset.compute ?? "auto");
    });
  }

  downloadAudio8Btn.addEventListener("click", async () => {
    if (!audio8Supported || audio8Downloading || audio8Downloaded) {
      return;
    }
    audio8Downloading = true;
    downloadAudio8Btn.disabled = true;
    audio8Status.textContent = "Downloading Audio8 model... this can take several minutes.";
    audio8Progress.value = 0;
    audio8Progress.classList.remove("is-hidden");
    audio8ProgressText.classList.remove("is-hidden");
    audio8ProgressText.textContent = "Starting download...";
    log("Audio8 model download started");
    try {
      const result = await invoke<Audio8DownloadResult>("download_audio8_model");
      audio8Downloading = false;
      audio8Downloaded = true;
      log(result.message || "Audio8 model download finished");
      try {
        await refreshModelOptionsFromBootstrap();
      } catch (refreshError) {
        log(`Failed to refresh model list: ${String(refreshError)}`, "error");
      }
      renderAudio8Card();
      if (result.message) {
        audio8Status.textContent = result.message;
      }
    } catch (error) {
      audio8Downloading = false;
      audio8Status.textContent = `Download failed: ${String(error)}. Click the button to retry; partial downloads resume.`;
      downloadAudio8Btn.disabled = false;
      log(`Audio8 model download failed: ${String(error)}`, "error");
    }
  });

  readBtn.addEventListener("click", async () => {
    await applySpeakSettings();
    await invokeAndLog("trigger_read_selection", "Triggered read-selection");
  });

  cancelBtn.addEventListener("click", async () => {
    stopAllPlayback();
    await invokeAndLog("cancel_active_job", "Cancel requested");
  });

  speakBtn.addEventListener("click", async () => {
    await applySpeakSettings();
    await invokeAndLog("speak_text", "Speak requested", { text: speakText.value });
  });
}

async function bindEvents(): Promise<void> {
  await listen<ToolbarActionPayload>(TOOLBAR_ACTION_EVENT, async ({ payload }) => {
    const action = payload.action;
    if (action === "pause-toggle") {
      try {
        await togglePlaybackPause();
      } catch (error) {
        log(`Pause/resume failed: ${String(error)}`, "error");
      }
      return;
    }
    if (action === "skip-forward") {
      skipPlaybackForward();
      return;
    }
    if (action === "skip-back") {
      void emit(TOOLBAR_SKIP_BACK_NOOP_EVENT, {});
      return;
    }
    if (action === "stop") {
      try {
        await toolbarStopPlayback();
        log("Playback stopped from toolbar");
      } catch (error) {
        log(`Toolbar stop failed: ${String(error)}`, "error");
      }
    }
  });

  await listen<Record<string, unknown>>(RATE_UPDATED_EVENT, ({ payload }) => {
    const parsed = rateFromPayload(payload);
    if (parsed === null) {
      return;
    }
    rateInput.value = formatRateNumber(clampRate(parsed));
  });

  await listen<JsonValue>("voicereader:ws-event", async ({ payload }) => {
    const eventType = String(payload.type ?? "UNKNOWN");
    const jobId = String(payload.job_id ?? "");

    if (eventType === "AUDIO_CHUNK" && jobId && suppressedJobIds.has(jobId)) {
      return;
    }

    log(`ws_event=${eventType}`);

    if (eventType === "AUDIO_CHUNK") {
      await enqueueAudioChunk(payload);
      return;
    }

    if (eventType === "JOB_CANCELED") {
      if (jobId) {
        suppressedJobIds.delete(jobId);
        playbackChunkCounts.delete(jobId);
        queuedPlaybackByJob.delete(jobId);
      }
      stopAllPlayback();
      if (!jobId || jobId === activeToolbarJobId) {
        hideToolbar();
      }
      return;
    }

    if (eventType === "JOB_DONE" || eventType === "JOB_ERROR") {
      if (jobId) {
        suppressedJobIds.delete(jobId);
        playbackChunkCounts.delete(jobId);
        const queued = queuedPlaybackByJob.get(jobId);
        if (queued) {
          queued.terminal = true;
          flushQueuedPlayback(jobId, true);
        }
      }
      if (playbackIsIdle()) {
        resetPlaybackCursor();
        if (!jobId || jobId === activeToolbarJobId) {
          hideToolbar();
        }
      }
    }
  });

  await listen<JobStartedPayload>("voicereader:job-started", ({ payload }) => {
    const jobId = String(payload.job_id ?? "unknown");
    const sourceWindow = String(payload.source_window ?? "");
    const playbackRate = Number(payload.rate ?? selectedRateSetting());
    const toolbarRate = Number.isFinite(playbackRate) ? clampRate(playbackRate) : selectedRateSetting();
    if (jobId !== "unknown") {
      suppressedJobIds.delete(jobId);
      playbackChunkCounts.set(jobId, 0);
      activeToolbarJobId = jobId;
      for (const existingJobId of Array.from(queuedPlaybackByJob.keys())) {
        if (existingJobId !== jobId) {
          queuedPlaybackByJob.delete(existingJobId);
        }
      }
    }
    showToolbar(sourceWindow, toolbarRate);
    log(`job_started id=${jobId}`);
  });

  await listen<Record<string, unknown>>("voicereader:hotkey-updated", ({ payload }) => {
    const hotkey = String(payload.hotkey ?? "");
    if (hotkey) {
      setHotkeyDisplay(hotkey);
      log(`hotkey_updated=${hotkey}`);
    }
  });

  await listen<JobCancelRequestedPayload>("voicereader:job-cancel-requested", ({ payload }) => {
    const jobId = String(payload.job_id ?? "");
    if (jobId) {
      suppressedJobIds.add(jobId);
      playbackChunkCounts.delete(jobId);
      queuedPlaybackByJob.delete(jobId);
    }
    stopAllPlayback();
    if (!jobId || jobId === activeToolbarJobId) {
      hideToolbar();
    }
    log(`playback_stop job_id=${jobId || "unknown"}`);
  });

  await listen<Record<string, unknown>>("voicereader:selection-empty", () => {
    log("No selection was detected. Highlight text and try the hotkey again.", "error");
  });

  await listen<Record<string, unknown>>("voicereader:error", ({ payload }) => {
    log(String(payload.message ?? "Unknown engine/app error"), "error");
  });

  await listen<ModelDownloadProgressPayload>("voicereader:model-download", ({ payload }) => {
    if (payload.model !== AUDIO8_MODEL_ID) {
      return;
    }
    if (payload.state === "progress") {
      renderAudio8DownloadProgress(payload);
    } else if (payload.state === "done") {
      renderAudio8DownloadProgress({
        ...payload,
        downloaded_bytes: payload.total_bytes,
      });
    } else if (payload.state === "error") {
      log(`Audio8 download error: ${payload.message}`, "error");
    }
  });

  await listen<JsonValue>("voicereader:engine-ready", () => {
    log("Engine is ready");
  });
}

setTabs();
bootstrap()
  .then(bindActions)
  .then(bindEvents)
  .then(async () => {
    setInterval(() => {
      pollRuntimeStatus().catch((error) => {
        log(`Runtime monitor error: ${String(error)}`, "error");
      });
    }, 5000);
  })
  .catch((error) => {
    log(`Bootstrap failed: ${String(error)}`, "error");
  });
