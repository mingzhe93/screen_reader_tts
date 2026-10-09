import { invoke } from "@tauri-apps/api/tauri";
import { emit, listen } from "@tauri-apps/api/event";
import { decodePcm16Base64ToFloat32 } from "./playback";
import { SpeechPlayer } from "./player";
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
import { brandMark, icon } from "./icons";
import { asrModelStatus, downloadAsrModel, initTranscribe } from "./transcribe";
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
  <div class="app-shell">
    <aside class="sidebar">
      <div class="brand">
        ${brandMark(28)}
        <span class="brand-name">VoiceReader</span>
      </div>

      <nav class="nav" aria-label="VoiceReader pages">
        <div class="nav-group">
          <p class="nav-label">Speak</p>
          <button class="nav-item active" type="button" data-nav="reader" title="Read aloud" aria-current="page">
            ${icon("speaker")}<span class="nav-text">Read aloud</span>
          </button>
          <button class="nav-item" type="button" data-nav="voices" title="Voices">
            ${icon("voices")}<span class="nav-text">Voices</span>
          </button>
        </div>
        <div class="nav-group">
          <p class="nav-label">Listen</p>
          <button class="nav-item" type="button" data-nav="transcribe" title="Transcribe">
            ${icon("mic")}<span class="nav-text">Transcribe</span>
          </button>
        </div>
        <div class="nav-group">
          <p class="nav-label">App</p>
          <button class="nav-item" type="button" data-nav="models" title="Models">
            ${icon("models")}<span class="nav-text">Models</span>
          </button>
          <button class="nav-item" type="button" data-nav="settings" title="Settings">
            ${icon("settings")}<span class="nav-text">Settings</span>
          </button>
        </div>
      </nav>

      <button class="runtime-status" id="runtime-pill" type="button" title="Open diagnostics">
        <span class="runtime-text" id="runtime-text">Checking engine...</span>
      </button>
    </aside>

    <main class="content" id="content">
      <div class="content-inner">

        <section class="page active" data-page="reader" aria-labelledby="reader-title">
          <header class="page-header">
            <h1 class="page-title" id="reader-title">Read aloud</h1>
            <div class="page-actions">
              <span class="hotkey-label">Hotkey</span>
              <div class="keycaps" id="hotkey-pill">Loading...</div>
              <button class="btn btn-ghost btn-sm" id="hotkey-edit-btn" type="button">Change</button>
            </div>
          </header>
          <div class="hotkey-capture is-hidden" id="hotkey-capture-row">
            <input id="hotkey-input" placeholder="Click here and press a shortcut" readonly />
            <button class="btn" id="set-hotkey-btn" type="button">Set hotkey</button>
            <button class="btn btn-ghost" id="cancel-hotkey-btn" type="button">Cancel</button>
          </div>
          <p class="hint hotkey-hint">Avoid OS-reserved combos such as Alt+Space (Windows) and Cmd+Space (macOS).</p>

          <article class="card">
            <div class="field-row">
              <div class="field">
                <label for="model-select">Model</label>
                <select id="model-select"></select>
              </div>
              <div class="field">
                <label for="voice-select">Voice</label>
                <select id="voice-select"></select>
              </div>
              <div class="field">
                <div class="field-label">
                  <label for="rate">Speed</label>
                  <span class="range-readout" id="rate-readout">1.5x</span>
                </div>
                <div class="range-wrap">
                  <input id="rate" type="range" min="0.25" max="4" step="0.05" value="1.5" />
                </div>
              </div>
            </div>

            <label class="sr-only" for="speak-text">Text to read</label>
            <textarea id="speak-text" class="speak-text" rows="8" placeholder="Paste or type text to hear it in the selected voice">Welcome to VoiceReader. Highlight any text, press your hotkey, and hear it read aloud in the voice you choose.</textarea>

            <div class="action-row">
              <button class="btn btn-primary" id="speak-btn" type="button">Speak</button>
              <button class="btn" id="read-btn" type="button">Read selection</button>
              <button class="btn" id="cancel-btn" type="button">Stop</button>
            </div>
          </article>
        </section>

        <section class="page" data-page="voices" aria-labelledby="voices-title">
          <header class="page-header">
            <h1 class="page-title" id="voices-title">Voices</h1>
          </header>
          <div class="stack">
            <article class="card">
              <h2 class="card-title">Clone a voice</h2>
              <p class="hint" id="clone-hint">Upload a short, clean reference clip to create and save a cloned voice profile.</p>
              <div class="clone-grid">
                <div class="field">
                  <label for="clone-display-name">Voice name</label>
                  <input id="clone-display-name" placeholder="My voice" />
                </div>
                <div class="field">
                  <label for="clone-language">Language hint</label>
                  <input id="clone-language" placeholder="en" value="en" />
                </div>
                <div class="field span-2">
                  <label for="clone-audio-file">Reference audio (WAV)</label>
                  <input id="clone-audio-file" type="file" accept=".wav,audio/wav" />
                  <p class="caption" id="clone-file-label">No file selected</p>
                </div>
                <div class="field span-2">
                  <div class="field-label">
                    <label for="clone-ref-text">Reference text (optional for Kyutai, required for Audio8)</label>
                    <button class="btn btn-sm btn-two-line" id="clone-transcribe-btn" type="button" title="Fill in the reference text from the audio file. English only.">
                      <span id="clone-transcribe-label">Transcribe audio</span>
                      <span class="btn-note">(English only)</span>
                    </button>
                  </div>
                  <textarea id="clone-ref-text" rows="2" placeholder="Optional transcript of the uploaded sample"></textarea>
                  <div class="clone-asr-prompt is-hidden" id="clone-asr-prompt">
                    <p class="hint" id="clone-asr-prompt-text"></p>
                    <div class="action-row">
                      <button class="btn btn-sm" id="clone-asr-download-btn" type="button">Download model</button>
                      <button class="btn btn-ghost btn-sm" id="clone-asr-dismiss-btn" type="button">Not now</button>
                    </div>
                    <progress class="download-progress is-hidden" data-asr-progress max="100" value="0"></progress>
                    <p class="caption mono is-hidden" data-asr-progress-text></p>
                  </div>
                </div>
                <div class="action-row span-2">
                  <button class="btn btn-primary" id="clone-voice-btn" type="button">Clone and save voice</button>
                </div>
                <p class="clone-feedback is-hidden span-2" id="clone-status" role="status" aria-live="polite"></p>
              </div>
            </article>

            <article class="card">
              <div class="card-header">
                <div>
                  <h2 class="card-title">Voice library</h2>
                  <p class="hint">Built-in and saved voices. Edit a saved voice and save it, or delete it.</p>
                </div>
                <button class="btn btn-sm" id="refresh-voices-btn" type="button">Refresh voices</button>
              </div>
              <div class="table-wrap">
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
              </div>
            </article>
          </div>
        </section>

        <section class="page" data-page="transcribe" aria-labelledby="transcribe-title">
          <header class="page-header">
            <div class="page-heading">
              <h1 class="page-title" id="transcribe-title">Transcribe</h1>
              <span class="badge">English only</span>
            </div>
          </header>
          <div id="transcribe-root">
            <article class="card empty-state">
              <span class="empty-icon">${icon("mic", 24)}</span>
              <p class="empty-text">Transcription is being set up in this build.</p>
            </article>
          </div>
        </section>

        <section class="page" data-page="models" aria-labelledby="models-title">
          <header class="page-header">
            <h1 class="page-title" id="models-title">Models</h1>
          </header>
          <div class="stack">
            <article class="card model-row">
              <div class="model-main">
                <div class="model-title">
                  <h2 class="model-name">Kyutai Pocket TTS</h2>
                  <span class="badge">Bundled</span>
                  <span class="badge">English</span>
                </div>
                <p class="model-desc">Fast, lightweight voices with voice cloning. Always runs on the CPU.</p>
                <p class="model-status ok">Ready</p>
              </div>
            </article>

            <article class="card model-row is-hidden" id="audio8-card">
              <div class="model-main">
                <div class="model-title">
                  <h2 class="model-name">Audio8 TTS</h2>
                  <span class="badge">About 860 MB</span>
                </div>
                <p class="model-desc">Multilingual model with voice cloning. English and Chinese are the primary languages.</p>
                <p class="model-status" id="audio8-status">Checking Audio8 model status...</p>
              </div>
              <div class="model-action">
                <button class="btn btn-primary" id="download-audio8-btn" type="button">Download model</button>
              </div>
              <div class="model-progress">
                <progress id="audio8-progress" class="download-progress is-hidden" max="100" value="0"></progress>
                <p class="caption mono is-hidden" id="audio8-progress-text"></p>
              </div>
            </article>

            <div id="asr-models-slot"></div>

            <article class="card model-row" id="model-downloads-card">
              <div class="model-main">
                <div class="model-title">
                  <h2 class="model-name">Qwen3 TTS</h2>
                  <span class="badge">Optional download</span>
                </div>
                <p class="model-desc">CustomVoice and Base models, downloaded on demand.</p>
                <p class="model-status" id="model-download-status">No download in progress.</p>
              </div>
              <div class="model-footer">
                <button class="btn" id="download-qwen-custom-btn" type="button">Download CustomVoice</button>
                <button class="btn" id="download-qwen-base-btn" type="button">Download Base</button>
                <button class="btn" id="download-qwen-all-btn" type="button">Download both</button>
              </div>
            </article>

            <p class="caption mono storage-line" id="model-storage-paths">Storage: loading...</p>
          </div>
        </section>

        <section class="page" data-page="settings" aria-labelledby="settings-title">
          <header class="page-header">
            <h1 class="page-title" id="settings-title">Settings</h1>
          </header>
          <div class="stack">
            <article class="card settings-section">
              <h2 class="card-title">Playback</h2>
              <div class="setting-row">
                <div class="setting-text">
                  <label for="volume">Volume</label>
                  <p class="setting-desc">Playback loudness. 1 is normal, and values up to 2 boost the audio.</p>
                </div>
                <input id="volume" type="number" min="0" max="2" step="0.05" value="1" />
              </div>
              <div class="setting-row">
                <div class="setting-text">
                  <label for="chunk-max">Chunk max chars</label>
                  <p class="setting-desc">Most characters sent to the model at once, from 100 to 200. Smaller chunks can start playing sooner.</p>
                </div>
                <input id="chunk-max" type="number" min="100" max="200" step="10" value="200" />
              </div>
            </article>

            <article class="card settings-section is-hidden" id="selection-access-card">
              <h2 class="card-title">Read highlighted text</h2>
              <p class="setting-desc">Allow VoiceReader in System Settings → Privacy &amp; Security → Accessibility (Device Control and Data Access on newer macOS). This lets the hotkey copy the text you highlight in another app.</p>
              <p class="hint" id="selection-access-status" role="status"></p>
              <button class="btn" id="selection-access-btn" type="button">Check access</button>
            </article>

            <article class="card settings-section is-hidden" id="compute-card">
              <h2 class="card-title">Compute device</h2>
              <p class="setting-desc">Where the heavy part of a model runs. Auto uses the GPU when one is available and faster than the CPU. Choose CPU to keep the GPU free for other work.</p>
              <div class="segmented" role="group" aria-label="Compute device">
                <button class="compute-btn" type="button" data-compute="auto" aria-pressed="false">Auto</button>
                <button class="compute-btn" type="button" data-compute="gpu" aria-pressed="false">GPU</button>
                <button class="compute-btn" type="button" data-compute="cpu" aria-pressed="false">CPU</button>
              </div>
              <p class="setting-desc" id="compute-status"></p>
            </article>

            <article class="card settings-section">
              <h2 class="card-title">Appearance</h2>
              <div class="setting-row">
                <div class="setting-text">
                  <span class="setting-label">Theme</span>
                  <p class="setting-desc">Switch between the dark and light interface.</p>
                </div>
                <button id="theme-toggle-btn" class="theme-toggle" type="button" aria-label="Switch theme" title="Switch theme">
                  <span class="theme-toggle-track">
                    <span class="theme-toggle-thumb" aria-hidden="true"></span>
                    <span class="theme-toggle-icon sun" aria-hidden="true">${icon("sun", 14)}</span>
                    <span class="theme-toggle-icon moon" aria-hidden="true">${icon("moon", 14)}</span>
                  </span>
                </button>
              </div>
            </article>

            <article class="card diagnostics" id="settings-diagnostics">
              <div class="card-header">
                <div>
                  <h2 class="card-title">Diagnostics</h2>
                  <p class="hint">For troubleshooting. Check engine health or restart the engine.</p>
                </div>
                <div class="action-row">
                  <button class="btn btn-sm" id="refresh-btn" type="button">Refresh health</button>
                  <button class="btn btn-sm" id="restart-btn" type="button">Restart engine</button>
                </div>
              </div>
              <pre id="health-json" class="json-box"></pre>
              <h3 class="subhead">Activity log</h3>
              <div class="log-wrap">
                <div id="log" class="log"></div>
              </div>
            </article>
          </div>
        </section>

      </div>
    </main>
  </div>
`;

const contentEl = document.querySelector<HTMLElement>("#content")!;
const hotkeyPill = document.querySelector<HTMLDivElement>("#hotkey-pill")!;
const hotkeyCaptureRow = document.querySelector<HTMLDivElement>("#hotkey-capture-row")!;
const themeToggleBtn = document.querySelector<HTMLButtonElement>("#theme-toggle-btn")!;
const runtimePill = document.querySelector<HTMLButtonElement>("#runtime-pill")!;
const runtimeText = document.querySelector<HTMLSpanElement>("#runtime-text")!;
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
const cloneTranscribeBtn = document.querySelector<HTMLButtonElement>("#clone-transcribe-btn")!;
const cloneTranscribeLabel = document.querySelector<HTMLSpanElement>("#clone-transcribe-label")!;
const cloneAsrPrompt = document.querySelector<HTMLDivElement>("#clone-asr-prompt")!;
const cloneAsrPromptText = document.querySelector<HTMLParagraphElement>("#clone-asr-prompt-text")!;
const cloneAsrDownloadBtn = document.querySelector<HTMLButtonElement>("#clone-asr-download-btn")!;
const cloneAsrDismissBtn = document.querySelector<HTMLButtonElement>("#clone-asr-dismiss-btn")!;
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
const rateReadout = document.querySelector<HTMLSpanElement>("#rate-readout")!;
const volumeInput = document.querySelector<HTMLInputElement>("#volume")!;
const chunkMaxInput = document.querySelector<HTMLInputElement>("#chunk-max")!;

const refreshBtn = document.querySelector<HTMLButtonElement>("#refresh-btn")!;
const restartBtn = document.querySelector<HTMLButtonElement>("#restart-btn")!;
const readBtn = document.querySelector<HTMLButtonElement>("#read-btn")!;
const cancelBtn = document.querySelector<HTMLButtonElement>("#cancel-btn")!;
const speakBtn = document.querySelector<HTMLButtonElement>("#speak-btn")!;

let runtimeWasDown = false;
const suppressedJobIds = new Set<string>();
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
// True when the backend sends audio at normal speed and the player applies the chosen
// speed (the Base build). False when the audio arrives already at that speed (the
// sidecar build), in which case the player must not stretch it again.
let playerAppliesRate = false;
const player = new SpeechPlayer({
  onFinished: () => hideToolbar(),
  onRebuffer: (count, waitSeconds) => log(`playback_rebuffer count=${count} wait_for=${waitSeconds.toFixed(2)}s`),
  onError: (message) => log(message, "error"),
});
// The newest job the backend announced. Audio and end-of-job events from any other
// job are stale: that job was replaced, and its output must not reach the speakers.
let currentPlaybackJobId = "";

applyTheme(currentTheme, false);
syncRateReadout();

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
  // Render each key as its own keycap. A trailing "+" is the plus key itself, not a separator.
  hotkeyPill.replaceChildren();
  const keys = value.split(/\+(?!$)/).filter((key) => key.length > 0);
  for (const key of keys) {
    const cap = document.createElement("kbd");
    cap.textContent = key;
    hotkeyPill.append(cap);
  }
  hotkeyInput.value = value;
}

function syncRateReadout(): void {
  const min = Number(rateInput.min) || 0;
  const max = Number(rateInput.max) || 1;
  const parsed = Number(rateInput.value);
  const value = Number.isFinite(parsed) ? clampRate(parsed) : 1;
  rateReadout.textContent = `${formatRateNumber(value)}x`;
  const percent = max > min ? Math.min(100, Math.max(0, ((value - min) / (max - min)) * 100)) : 0;
  rateInput.style.setProperty("--fill", `${percent.toFixed(1)}%`);
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
  const navItems = Array.from(document.querySelectorAll<HTMLButtonElement>(".nav-item"));
  const pages = Array.from(document.querySelectorAll<HTMLElement>(".page"));
  if (!pages.some((page) => page.dataset.page === target)) {
    return;
  }

  navItems.forEach((item) => {
    const active = item.dataset.nav === target;
    item.classList.toggle("active", active);
    if (active) {
      item.setAttribute("aria-current", "page");
    } else {
      item.removeAttribute("aria-current");
    }
  });

  pages.forEach((page) => {
    page.classList.toggle("active", page.dataset.page === target);
  });
  contentEl.scrollTop = 0;
}

function setTabs(): void {
  const navItems = Array.from(document.querySelectorAll<HTMLButtonElement>(".nav-item"));

  navItems.forEach((item) => {
    item.addEventListener("click", () => {
      const target = item.dataset.nav;
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
    runtimePill.className = "runtime-status ok";
    // A local (in-process) runtime has no process id or address worth showing; say which
    // model is active and what it runs on instead. The full detail stays in the tooltip.
    const detail = [status.model_label, status.device_label].filter((part): part is string => Boolean(part));
    const full = [status.pid != null ? `Engine: running (pid=${status.pid}) @ ${status.base_url}` : "Engine: running", ...detail];
    runtimeText.textContent = detail.length > 0 ? detail.join(" · ") : "Engine running";
    runtimePill.title = `${full.join(" · ")}. Open diagnostics.`;
    runtimeWasDown = false;
    return;
  }

  runtimePill.className = "runtime-status down";
  runtimeText.textContent = "Engine is down";
  runtimePill.title = "Engine: down. Open diagnostics.";
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

/** Sends the chosen speed to the player. Heard at once, including mid-sentence. */
function applyPlayerTempo(): void {
  player.setTempo(playerAppliesRate ? selectedRateSetting() : 1);
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

async function showToolbar(sourceWindow: string, rate: number): Promise<void> {
  toolbarPaused = false;
  void player.setPaused(false);
  const payload: ToolbarShowPayload = {
    job_id: activeToolbarJobId,
    source_window: sourceLabelFromWindowTitle(sourceWindow),
    rate,
  };
  try {
    await invoke("show_playback_toolbar");
    // Playback may have finished while the native window was being shown.
    if (activeToolbarJobId !== payload.job_id || !activeToolbarJobId) {
      if (!activeToolbarJobId) await invoke("hide_playback_toolbar");
      return;
    }
    await emit(TOOLBAR_SHOW_EVENT, payload);
    await emit(TOOLBAR_PAUSED_EVENT, { paused: false } satisfies ToolbarPausePayload);
  } catch (error) { log(`Could not show playback toolbar: ${String(error)}`, "error"); }
}

function hideToolbar(): void {
  toolbarPaused = false;
  void player.setPaused(false);
  activeToolbarJobId = "";
  void invoke("hide_playback_toolbar").catch((error) => {
    log(`Could not hide playback toolbar: ${String(error)}`, "error");
  });
  void emit(TOOLBAR_HIDE_EVENT, {});
}

async function togglePlaybackPause(): Promise<void> {
  toolbarPaused = !toolbarPaused;
  await player.setPaused(toolbarPaused);
  void emit(TOOLBAR_PAUSED_EVENT, { paused: toolbarPaused } satisfies ToolbarPausePayload);
}

/** The WAV file chosen in the clone form, or null after telling the user what is wrong. */
function selectedCloneFile(action: string): File | null {
  const file = cloneAudioFileInput.files?.[0];
  const problem = !file
    ? `Select an audio file before ${action}.`
    : !file.name.toLowerCase().endsWith(".wav")
      ? "Only WAV files are supported for cloning in this UI."
      : null;
  if (problem || !file) {
    showCloneStatus(problem ?? "", "error");
    log(problem ?? "", "error");
    return null;
  }
  return file;
}

/** Fills the reference text from the chosen clip, using the transcription model. */
async function transcribeCloneClip(file: File): Promise<void> {
  cloneTranscribeBtn.disabled = true;
  cloneTranscribeLabel.textContent = "Transcribing...";
  showCloneStatus("Transcribing the clip... this takes a few seconds.", "info", 0);
  try {
    const result = await invoke<{ text: string }>("transcribe_reference_clip", {
      wavBase64: await fileToBase64(file),
    });
    if (result.text) {
      cloneRefTextInput.value = result.text;
      showCloneStatus("Reference text filled in. Check it against the clip before cloning.", "success");
      log(`Transcribed reference clip ${file.name}`);
    } else {
      showCloneStatus("No speech was recognised in this clip. The transcriber understands English only.", "error");
    }
  } catch (error) {
    showCloneStatus(`Transcription failed: ${String(error)}`, "error");
    log(`Reference clip transcription failed: ${String(error)}`, "error");
  } finally {
    cloneTranscribeBtn.disabled = false;
    cloneTranscribeLabel.textContent = "Transcribe audio";
  }
}

/** Transcribes the chosen clip, offering the model download first when it is missing. */
async function transcribeCloneClipOrOfferDownload(): Promise<void> {
  const file = selectedCloneFile("transcribing");
  if (!file) {
    return;
  }
  let status;
  try {
    status = await asrModelStatus();
  } catch (error) {
    showCloneStatus(`Could not check the transcription model: ${String(error)}`, "error");
    return;
  }
  if (!status.supported) {
    showCloneStatus("Transcription is not available in this build. Type the reference text instead.", "error");
    return;
  }
  if (status.downloaded) {
    cloneAsrPrompt.classList.add("is-hidden");
    await transcribeCloneClip(file);
    return;
  }
  const size = (status.download_size_bytes / (1024 * 1024 * 1024)).toFixed(2);
  cloneAsrPromptText.textContent = `Transcribing needs the transcription model, a one-time download of about ${size} GB. It is the same model the Transcribe page uses.`;
  cloneAsrDownloadBtn.disabled = false;
  cloneAsrPrompt.classList.remove("is-hidden");
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

function enqueueAudioChunk(eventPayload: Record<string, unknown>): void {
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

  player.enqueue(decodePcm16Base64ToFloat32(dataBase64), sampleRate);
}

function stopAllPlayback(): void {
  player.reset();
}

function skipPlaybackForward(): void {
  if (!activeToolbarJobId) {
    return;
  }
  // Jumps past the audio received so far. If the job has ended, the player
  // reports that it finished and the toolbar closes.
  player.skipQueued();
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
  // One path per line; `.storage-line` keeps the line breaks.
  modelStoragePaths.textContent = [
    `Data: ${paths.data_dir}`,
    `Models: ${paths.models_dir}`,
    `HF cache: ${paths.hf_cache_dir}`,
  ].join("\n");
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
    audio8Status.classList.add("ok");
    audio8Status.textContent = "Ready. Select it from Model on the Read aloud page.";
    audio8Progress.classList.add("is-hidden");
    audio8ProgressText.classList.add("is-hidden");
    return;
  }
  downloadAudio8Btn.disabled = false;
  audio8Status.classList.remove("ok");
  downloadAudio8Btn.textContent = "Download model";
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
    const active = button.dataset.compute === payload.preference;
    button.classList.toggle("is-active", active);
    button.setAttribute("aria-pressed", String(active));
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
    activateTab("settings");
    document.querySelector<HTMLElement>("#settings-diagnostics")?.scrollIntoView({ block: "start", behavior: "smooth" });
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

  rateInput.addEventListener("input", () => {
    syncRateReadout();
    // While the slider is being dragged, not only when it is released.
    applyPlayerTempo();
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

  cloneTranscribeBtn.addEventListener("click", () => void transcribeCloneClipOrOfferDownload());
  cloneAsrDismissBtn.addEventListener("click", () => cloneAsrPrompt.classList.add("is-hidden"));
  cloneAsrDownloadBtn.addEventListener("click", async () => {
    cloneAsrDownloadBtn.disabled = true;
    cloneAsrPromptText.textContent = "Downloading the transcription model. You can keep using the app.";
    const downloaded = await downloadAsrModel();
    if (!downloaded) {
      cloneAsrDownloadBtn.disabled = false;
      cloneAsrPromptText.textContent = "The download did not finish. Try again; it continues where it stopped.";
      return;
    }
    cloneAsrPrompt.classList.add("is-hidden");
    // The download was started to transcribe this clip, so carry on with it.
    await transcribeCloneClipOrOfferDownload();
  });

  cloneVoiceBtn.addEventListener("click", async () => {
    const selectedFile = selectedCloneFile("cloning");
    if (!selectedFile) {
      return;
    }

    const displayName = cloneDisplayNameInput.value.trim() || selectedFile.name.replace(/\.[^/.]+$/, "");
    const language = cloneLanguageInput.value.trim();
    const refText = cloneRefTextInput.value.trim();
    if (currentSelectedModel === AUDIO8_MODEL_ID && !refText) {
      showCloneStatus("Audio8 needs the exact transcript of the reference clip. Fill in the reference text first.", "error");
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
      const successMessage = result.message || `Voice cloned: ${displayName}`;
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
    syncRateReadout();
    applyPlayerTempo();
  });

  await listen<JsonValue>("voicereader:ws-event", async ({ payload }) => {
    const eventType = String(payload.type ?? "UNKNOWN");
    const jobId = String(payload.job_id ?? "");
    // A replaced job keeps sending for a moment, until its synthesis notices the cancel.
    const replaced = jobId !== "" && currentPlaybackJobId !== "" && jobId !== currentPlaybackJobId;

    if (eventType === "AUDIO_CHUNK" && jobId && (suppressedJobIds.has(jobId) || replaced)) {
      return;
    }

    log(`ws_event=${eventType}`);

    if (replaced) {
      // Its end-of-job event must not stop or flush the job that replaced it.
      suppressedJobIds.delete(jobId);
      return;
    }

    if (eventType === "AUDIO_CHUNK") {
      enqueueAudioChunk(payload);
      return;
    }

    if (eventType === "JOB_CANCELED") {
      suppressedJobIds.delete(jobId);
      stopAllPlayback();
      hideToolbar();
      return;
    }

    if (eventType === "JOB_DONE" || eventType === "JOB_ERROR") {
      suppressedJobIds.delete(jobId);
      // What is queued plays out; the player then reports that it finished.
      player.end();
    }
  });

  await listen<JobStartedPayload>("voicereader:job-started", ({ payload }) => {
    const jobId = String(payload.job_id ?? "unknown");
    const sourceWindow = String(payload.source_window ?? "");
    const playbackRate = Number(payload.rate ?? selectedRateSetting());
    const toolbarRate = Number.isFinite(playbackRate) ? clampRate(playbackRate) : selectedRateSetting();
    if (jobId !== "unknown") {
      if (jobId !== currentPlaybackJobId) {
        // A new job replaces whatever is playing or queued. Without this the old audio
        // already scheduled keeps playing and the two jobs share one timeline.
        if (!player.idle) {
          log(`playback_replaced old_job_id=${currentPlaybackJobId || "unknown"} new_job_id=${jobId}`);
        }
        stopAllPlayback();
      }
      currentPlaybackJobId = jobId;
      suppressedJobIds.delete(jobId);
      activeToolbarJobId = jobId;
      playerAppliesRate = payload.rate_applied_by_player === true;
      player.setTempo(playerAppliesRate ? toolbarRate : 1);
    }
    void showToolbar(sourceWindow, toolbarRate);
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
// Kyutai, the model selected at every start, produces 24 kHz audio.
player.prepare(24000);
initTranscribe({
  log,
  isPageActive: () => document.querySelector('.page[data-page="transcribe"]')?.classList.contains("active") ?? false,
}).catch((error) => {
  log(`Transcribe setup failed: ${String(error)}`, "error");
});
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

async function refreshSelectionAccess(prompt = false): Promise<void> {
  try {
    const status = await invoke<{ required: boolean; allowed: boolean }>("selection_access", { prompt });
    document.querySelector("#selection-access-card")?.classList.toggle("is-hidden", !status.required);
    const label = document.querySelector("#selection-access-status");
    if (label) label.textContent = status.allowed
      ? "Access granted. Highlight text in another app and press your hotkey."
      : "Access needed. Enable this copy of VoiceReader, then quit and reopen it. If it is already enabled, remove the old entry and add the current app again.";
  } catch (error) { log(`Could not check selection access: ${String(error)}`, "error"); }
}
document.querySelector("#selection-access-btn")?.addEventListener("click", () => { void refreshSelectionAccess(true); });
window.addEventListener("focus", () => { void refreshSelectionAccess(); });
void refreshSelectionAccess();
