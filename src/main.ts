import { invoke } from "@tauri-apps/api/tauri";
import { listen } from "@tauri-apps/api/event";
import "./styles.css";

type JsonValue = Record<string, unknown>;

type SpeakerPreset = {
  id: string;
  description: string;
  native_language: string;
};

type ModelOption = {
  id: string;
  label: string;
  status: string;
  notes: string;
};

type BootstrapPayload = {
  hotkey: string;
  selected_voice_id: string;
  selected_model: string;
  selected_speaker: string;
  startup_error?: string | null;
  build_variant: string;
  qwen_enabled: boolean;
  models: ModelOption[];
  preset_speakers: SpeakerPreset[];
  health: JsonValue;
  voices: JsonValue;
};

type RuntimeStatusPayload = {
  running: boolean;
  pid: number | null;
  base_url: string;
  selected_voice_id: string;
  selected_model: string;
  selected_speaker: string;
};

type ModelUpdatePayload = {
  selected_model: string;
  selected_speaker: string;
  preset_speakers: SpeakerPreset[];
  applied: boolean;
  message: string;
  health: JsonValue;
};

type JobCancelRequestedPayload = {
  job_id: string;
};

type CloneVoiceResult = {
  ok: boolean;
  message: string;
  voice_id: string;
};

type HotkeyResult = {
  ok: boolean;
  message: string;
  hotkey: string;
};

type StoredVoice = {
  voice_id: string;
  display_name: string;
  created_at?: string;
  tts_model_id?: string;
  language_hint?: string | null;
  description?: string | null;
};

type UnifiedVoiceOption = {
  value: string;
  label: string;
  kind: "preset" | "stored";
  id: string;
};

type EngineStoragePathsPayload = {
  data_dir: string;
  models_dir: string;
  hf_cache_dir: string;
};

type PrefetchModelsResult = {
  ok: boolean;
  message: string;
  mode: string;
  downloaded: string[];
  data_dir: string;
  models_dir: string;
  hf_cache_dir: string;
};

const VOICE_ORDINAL_STORAGE_KEY = "voicereader.saved_voice_ordinals.v1";
const THEME_STORAGE_KEY = "voicereader.theme.v1";

type ThemeMode = "dark" | "light";

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
    <header class="hero compact">
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
              <label>Pitch <input id="pitch" type="number" min="0.5" max="2" step="0.05" value="1" /></label>
              <label>Volume <input id="volume" type="number" min="0" max="2" step="0.05" value="1" /></label>
              <label>Chunk Max Chars <input id="chunk-max" type="number" min="100" max="2000" step="10" value="160" /></label>
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
          <h2>Clone Voice (Kyutai)</h2>
          <p class="hint">Upload a short, clean reference clip to create and save a cloned voice profile.</p>
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
              Reference Text (optional)
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

const rateInput = document.querySelector<HTMLInputElement>("#rate")!;
const pitchInput = document.querySelector<HTMLInputElement>("#pitch")!;
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
let hasOutputPrimed = false;
let hasStartupSilenceInjected = false;
let currentPresetSpeakers: SpeakerPreset[] = [];
let currentSelectedSpeaker = "";
let currentSelectedModel = "";
let latestVoicesPayload: JsonValue = {};
let voiceOptionMap = new Map<string, UnifiedVoiceOption>();
const presetDescriptionOverrides = new Map<string, string>();
let savedVoiceOrdinals = loadSavedVoiceOrdinals();
let qwenEnabled = true;
let pendingHotkeyCapture = "";
let cloneStatusTimeoutId: number | null = null;
const pressedModifiers = {
  ctrl: false,
  alt: false,
  shift: false,
  meta: false,
};

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
  pressedModifiers.ctrl = false;
  pressedModifiers.alt = false;
  pressedModifiers.shift = false;
  pressedModifiers.meta = false;
  if (enabled) {
    pendingHotkeyCapture = "";
    hotkeyInput.value = "";
    hotkeyInput.focus();
    hotkeyInput.select();
    return;
  }
  pendingHotkeyCapture = "";
}

function setPressedModifier(key: string, pressed: boolean): boolean {
  if (key === "Control") {
    pressedModifiers.ctrl = pressed;
    return true;
  }
  if (key === "Alt" || key === "AltGraph") {
    pressedModifiers.alt = pressed;
    return true;
  }
  if (key === "Shift") {
    pressedModifiers.shift = pressed;
    return true;
  }
  if (key === "Meta" || key === "OS") {
    pressedModifiers.meta = pressed;
    return true;
  }
  return false;
}

function normalizeCapturedKey(event: KeyboardEvent): string | null {
  const raw = event.key;
  if (!raw) {
    return null;
  }

  if (["Control", "Shift", "Alt", "Meta", "OS", "AltGraph"].includes(raw)) {
    return null;
  }

  const code = event.code;
  if (code.startsWith("Key")) {
    return code.slice(3).toUpperCase();
  }
  if (code.startsWith("Digit")) {
    return code.slice(5);
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
  if (pressedModifiers.meta) {
    parts.push("Cmd");
  }
  if (pressedModifiers.ctrl) {
    parts.push("Ctrl");
  }
  if (pressedModifiers.alt) {
    parts.push("Alt");
  }
  if (pressedModifiers.shift) {
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
    runtimePill.textContent = `Engine: running (pid=${String(status.pid ?? "n/a")}) @ ${status.base_url}`;
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
      created_at: String((raw as Record<string, unknown>).created_at ?? ""),
      tts_model_id: String((raw as Record<string, unknown>).tts_model_id ?? ""),
      language_hint: String((raw as Record<string, unknown>).language_hint ?? ""),
      description: String((raw as Record<string, unknown>).description ?? ""),
    }))
    .filter((item) => item.voice_id.length > 0);
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

  const storedVoices = parseStoredVoices(latestVoicesPayload).filter((voice) => voice.voice_id !== "0");
  syncSavedVoiceOrdinals(storedVoices);
  const orderedSavedVoices = [...storedVoices].sort(
    (a, b) => savedVoiceOrdinal(a.voice_id) - savedVoiceOrdinal(b.voice_id),
  );

  for (let idx = 0; idx < orderedSavedVoices.length; idx += 1) {
    const voice = orderedSavedVoices[idx];
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

function nextChunkLeadSeconds(jobId: string): number {
  const DEFAULT_LEAD_SECONDS = 0.02;
  const FIRST_CHUNK_LEAD_SECONDS = 0.16;
  const FIRST_OUTPUT_PREROLL_SECONDS = 0.26;

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

function prependSilence(samples: Float32Array, sampleRate: number, ms: number): Float32Array {
  const silenceFrames = Math.max(0, Math.round((sampleRate * ms) / 1000));
  if (silenceFrames === 0) {
    return samples;
  }
  const withSilence = new Float32Array(silenceFrames + samples.length);
  withSilence.set(samples, silenceFrames);
  return withSilence;
}

function decodePcm16Base64ToFloat32(base64Data: string): Float32Array {
  const binary = atob(base64Data);
  const bytes = new Uint8Array(binary.length);
  for (let idx = 0; idx < binary.length; idx += 1) {
    bytes[idx] = binary.charCodeAt(idx);
  }

  const view = new DataView(bytes.buffer);
  const sampleCount = Math.floor(bytes.byteLength / 2);
  const output = new Float32Array(sampleCount);
  for (let idx = 0; idx < sampleCount; idx += 1) {
    const value = view.getInt16(idx * 2, true);
    output[idx] = value / 32768;
  }
  return output;
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
  if (context.state === "suspended") {
    await context.resume();
  }

  let samples = decodePcm16Base64ToFloat32(dataBase64);
  if (!hasStartupSilenceInjected) {
    // The first device wake-up can clip a short prefix; prepend silence once.
    samples = prependSilence(samples, sampleRate, 160);
    hasStartupSilenceInjected = true;
  }
  const buffer = context.createBuffer(1, samples.length, sampleRate);
  buffer.copyToChannel(samples, 0, 0);

  const source = context.createBufferSource();
  source.buffer = buffer;
  source.connect(context.destination);
  activeAudioSources.add(source);
  source.onended = () => {
    activeAudioSources.delete(source);
  };

  const now = context.currentTime;
  const jobId = String(eventPayload.job_id ?? "");
  const leadSeconds = nextChunkLeadSeconds(jobId);
  const startAt = Math.max(playbackCursor, now + leadSeconds);
  source.start(startAt);
  playbackCursor = startAt + buffer.duration;
}

function resetPlaybackCursor(): void {
  if (!audioContext) {
    return;
  }
  playbackCursor = audioContext.currentTime;
}

function stopAllPlayback(): void {
  for (const source of activeAudioSources) {
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
  resetPlaybackCursor();
}

async function refreshHealthAndVoices(): Promise<void> {
  const [health, voices, runtime] = await Promise.all([
    invoke<JsonValue>("engine_health"),
    invoke<JsonValue>("engine_list_voices"),
    invoke<RuntimeStatusPayload>("engine_runtime_status"),
  ]);

  healthJson.textContent = encodeJson(health);
  latestVoicesPayload = voices;
  currentSelectedSpeaker = runtime.selected_speaker;
  currentSelectedModel = runtime.selected_model;
  if (Array.from(modelSelect.options).some((option) => option.value === runtime.selected_model)) {
    modelSelect.value = runtime.selected_model;
  }
  renderUnifiedVoiceOptions(runtime.selected_voice_id, runtime.selected_speaker);
  renderVoicesTable();
}

async function refreshEngineStoragePaths(): Promise<void> {
  try {
    const paths = await invoke<EngineStoragePathsPayload>("engine_storage_paths");
    modelStoragePaths.textContent = `Data: ${paths.data_dir} | Models: ${paths.models_dir} | HF cache: ${paths.hf_cache_dir}`;
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
  const storedVoices = parseStoredVoices(latestVoicesPayload).filter((voice) => voice.voice_id !== "0");
  syncSavedVoiceOrdinals(storedVoices);
  const orderedSavedVoices = [...storedVoices].sort(
    (a, b) => savedVoiceOrdinal(a.voice_id) - savedVoiceOrdinal(b.voice_id),
  );

  const hasRows = currentPresetSpeakers.length > 0 || storedVoices.length > 0;
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

  for (let idx = 0; idx < orderedSavedVoices.length; idx += 1) {
    const voice = orderedSavedVoices[idx];
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
  const pitch = Number(pitchInput.value);
  const volume = Number(volumeInput.value);
  const chunkMaxChars = Number(chunkMaxInput.value);

  await invoke("set_speak_settings", {
    rate,
    pitch,
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
  currentSelectedSpeaker = payload.selected_speaker;
  currentSelectedModel = payload.selected_model;

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
  log(`Build variant: ${payload.build_variant}${payload.qwen_enabled ? " (Qwen enabled)" : " (Kyutai only)"}`);

  await pollRuntimeStatus();
  await refreshEngineStoragePaths();
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
    const result = await invoke<ModelUpdatePayload>("select_model", { model: modelSelect.value });
    currentPresetSpeakers = result.preset_speakers;
    currentSelectedSpeaker = result.selected_speaker;
    currentSelectedModel = result.selected_model;
    await invoke("set_selected_voice", { voiceId: "0" });
    await refreshHealthAndVoices();
    log(result.message ?? "Model updated");
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

    if (setPressedModifier(event.key, true)) {
      return;
    }

    const captured = captureHotkeyFromEvent(event);
    if (!captured) {
      return;
    }
    pendingHotkeyCapture = captured;
    hotkeyInput.value = captured;
  });

  hotkeyInput.addEventListener("keyup", (event) => {
    if (setPressedModifier(event.key, false)) {
      return;
    }
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
      currentPresetSpeakers = result.preset_speakers;
      currentSelectedSpeaker = result.selected_speaker;
      currentSelectedModel = result.selected_model;
      await invoke("set_selected_voice", { voiceId: "0" });
      await refreshHealthAndVoices();
      log(`Selected built-in voice ${selected.id}`);
      return;
    }

    await invoke("set_selected_voice", { voiceId: selected.id });
    log(`Selected saved voice ${selected.label}`);
  });

  [rateInput, pitchInput, volumeInput, chunkMaxInput].forEach((input) => {
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
      modelStoragePaths.textContent = `Data: ${result.data_dir} | Models: ${result.models_dir} | HF cache: ${result.hf_cache_dir}`;
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

  readBtn.addEventListener("click", async () => {
    const response = await invoke<Record<string, unknown>>("trigger_read_selection");
    log(String(response.message ?? "Triggered read-selection"));
  });

  cancelBtn.addEventListener("click", async () => {
    stopAllPlayback();
    const response = await invoke<Record<string, unknown>>("cancel_active_job");
    log(String(response.message ?? "Cancel requested"));
  });

  speakBtn.addEventListener("click", async () => {
    const text = speakText.value;
    const response = await invoke<Record<string, unknown>>("speak_text", { text });
    log(String(response.message ?? "Speak requested"));
  });
}

async function bindEvents(): Promise<void> {
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
      }
      stopAllPlayback();
      return;
    }

    if (eventType === "JOB_DONE" || eventType === "JOB_ERROR") {
      if (jobId) {
        suppressedJobIds.delete(jobId);
        playbackChunkCounts.delete(jobId);
      }
      resetPlaybackCursor();
    }
  });

  await listen<Record<string, unknown>>("voicereader:job-started", ({ payload }) => {
    const jobId = String(payload.job_id ?? "unknown");
    if (jobId !== "unknown") {
      suppressedJobIds.delete(jobId);
      playbackChunkCounts.set(jobId, 0);
    }
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
    }
    stopAllPlayback();
    log(`playback_stop job_id=${jobId || "unknown"}`);
  });

  await listen<Record<string, unknown>>("voicereader:selection-empty", () => {
    log("No selection was detected. Highlight text and try the hotkey again.", "error");
  });

  await listen<Record<string, unknown>>("voicereader:error", ({ payload }) => {
    log(String(payload.message ?? "Unknown engine/app error"), "error");
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
