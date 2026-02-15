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

type HotkeyResult = {
  ok: boolean;
  message: string;
  hotkey: string;
};

const app = document.querySelector<HTMLDivElement>("#app");
if (!app) {
  throw new Error("Missing app root");
}

app.innerHTML = `
  <main class="shell">
    <header class="hero">
      <p class="eyebrow">VoiceReader Desktop</p>
      <h1>Highlight, hotkey, speak</h1>
      <p class="sub">Phase 1 vertical slice: sidecar launch, auth handshake, global hotkey read flow, and streamed playback.</p>
    </header>

    <section class="tabs" role="tablist" aria-label="VoiceReader pages">
      <button class="tab active" data-tab="reader" role="tab" aria-selected="true">Reader</button>
      <button class="tab" data-tab="voices" role="tab" aria-selected="false">Voices & Clone</button>
    </section>

    <section class="panel active" id="reader-panel">
      <div class="grid">
        <article class="card">
          <h2>Quick Start</h2>
          <p class="hint">Use the global hotkey shown below after highlighting text in any app.</p>
          <div class="hotkey" id="hotkey-pill">Loading hotkey...</div>
          <div class="runtime" id="runtime-pill">Engine status: checking...</div>
          <div class="row">
            <label for="hotkey-input">Global Hotkey</label>
            <div class="inline-row">
              <input id="hotkey-input" placeholder="Alt+Shift+Space or CmdOrCtrl+Shift+S" />
              <button id="set-hotkey-btn">Set Hotkey</button>
            </div>
            <p class="hint">Avoid OS-reserved combos such as Alt+Space (Windows) and Cmd+Space (macOS).</p>
          </div>

          <div class="row">
            <label for="model-select">Model Mode</label>
            <select id="model-select"></select>
          </div>

          <div class="row">
            <label for="speaker-select">Preset Speaker</label>
            <select id="speaker-select"></select>
          </div>

          <div class="row">
            <label for="voice-select">Available Voice ID</label>
            <select id="voice-select"></select>
          </div>

          <div class="controls">
            <label>Rate <input id="rate" type="number" min="0.25" max="4" step="0.05" value="1" /></label>
            <label>Pitch <input id="pitch" type="number" min="0.5" max="2" step="0.05" value="1" /></label>
            <label>Volume <input id="volume" type="number" min="0" max="2" step="0.05" value="1" /></label>
            <label>Chunk Max Chars <input id="chunk-max" type="number" min="100" max="2000" step="10" value="160" /></label>
          </div>

          <div class="button-row">
            <button id="refresh-btn">Refresh Health</button>
            <button id="restart-btn">Restart Engine</button>
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

          <h3>Engine Health</h3>
          <pre id="health-json" class="json-box"></pre>
        </article>
      </div>
    </section>

    <section class="panel" id="voices-panel">
      <div class="grid single">
        <article class="card">
          <h2>Preset Speakers</h2>
          <p class="hint">Model-specific voice presets. Clone management UI lands next.</p>
          <table>
            <thead>
              <tr><th>Speaker</th><th>Description</th><th>Native Language</th></tr>
            </thead>
            <tbody id="preset-table"></tbody>
          </table>

          <h2>Saved Voices</h2>
          <p class="hint">Current engine voices endpoint (default + cloned profiles when enabled).</p>
          <pre id="voices-json" class="json-box"></pre>
        </article>
      </div>
    </section>

    <section class="log-wrap">
      <h2>Activity</h2>
      <div id="log" class="log"></div>
    </section>
  </main>
`;

const hotkeyPill = document.querySelector<HTMLDivElement>("#hotkey-pill")!;
const runtimePill = document.querySelector<HTMLDivElement>("#runtime-pill")!;
const hotkeyInput = document.querySelector<HTMLInputElement>("#hotkey-input")!;
const setHotkeyBtn = document.querySelector<HTMLButtonElement>("#set-hotkey-btn")!;
const modelSelect = document.querySelector<HTMLSelectElement>("#model-select")!;
const speakerSelect = document.querySelector<HTMLSelectElement>("#speaker-select")!;
const voiceSelect = document.querySelector<HTMLSelectElement>("#voice-select")!;
const healthJson = document.querySelector<HTMLPreElement>("#health-json")!;
const voicesJson = document.querySelector<HTMLPreElement>("#voices-json")!;
const presetTable = document.querySelector<HTMLTableSectionElement>("#preset-table")!;
const speakText = document.querySelector<HTMLTextAreaElement>("#speak-text")!;
const logEl = document.querySelector<HTMLDivElement>("#log")!;

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

function log(message: string, level: "info" | "error" = "info"): void {
  const line = document.createElement("div");
  line.className = `line ${level}`;
  line.textContent = `${new Date().toLocaleTimeString()} | ${message}`;
  logEl.prepend(line);
}

function setTabs(): void {
  const tabs = Array.from(document.querySelectorAll<HTMLButtonElement>(".tab"));
  const panels = {
    reader: document.querySelector<HTMLElement>("#reader-panel"),
    voices: document.querySelector<HTMLElement>("#voices-panel"),
  };

  tabs.forEach((tab) => {
    tab.addEventListener("click", () => {
      const target = tab.dataset.tab;
      tabs.forEach((t) => {
        const active = t === tab;
        t.classList.toggle("active", active);
        t.setAttribute("aria-selected", String(active));
      });
      panels.reader?.classList.toggle("active", target === "reader");
      panels.voices?.classList.toggle("active", target === "voices");
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

function parseVoiceList(voicesPayload: JsonValue): Array<{ voice_id: string; display_name: string }> {
  const voices = Array.isArray(voicesPayload.voices) ? voicesPayload.voices : [];
  return voices
    .map((raw) => ({
      voice_id: String((raw as Record<string, unknown>).voice_id ?? ""),
      display_name: String((raw as Record<string, unknown>).display_name ?? "Unknown"),
    }))
    .filter((item) => item.voice_id.length > 0);
}

function ensureAudioContext(): AudioContext {
  if (!audioContext) {
    audioContext = new AudioContext();
    playbackCursor = audioContext.currentTime;
  }
  return audioContext;
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

  const samples = decodePcm16Base64ToFloat32(dataBase64);
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
  const startAt = Math.max(playbackCursor, now + 0.02);
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
  const [health, voices] = await Promise.all([
    invoke<JsonValue>("engine_health"),
    invoke<JsonValue>("engine_list_voices"),
  ]);

  healthJson.textContent = encodeJson(health);
  voicesJson.textContent = encodeJson(voices);

  const voiceList = parseVoiceList(voices);
  const current = voiceSelect.value;
  voiceSelect.innerHTML = "";
  for (const voice of voiceList) {
    const option = document.createElement("option");
    option.value = voice.voice_id;
    option.textContent = `${voice.display_name} (${voice.voice_id})`;
    voiceSelect.append(option);
  }

  if (voiceList.some((item) => item.voice_id === current)) {
    voiceSelect.value = current;
  }
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

function renderPresetSpeakers(speakers: SpeakerPreset[], selected: string): void {
  speakerSelect.innerHTML = "";
  presetTable.innerHTML = "";

  for (const speaker of speakers) {
    const option = document.createElement("option");
    option.value = speaker.id;
    option.textContent = `${speaker.id} (${speaker.native_language})`;
    speakerSelect.append(option);

    const row = document.createElement("tr");
    row.innerHTML = `<td>${speaker.id}</td><td>${speaker.description}</td><td>${speaker.native_language}</td>`;
    presetTable.append(row);
  }

  speakerSelect.value = selected;
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

  hotkeyPill.textContent = payload.hotkey;
  hotkeyInput.value = payload.hotkey;
  renderModelOptions(payload.models, payload.selected_model);
  renderPresetSpeakers(payload.preset_speakers, payload.selected_speaker);

  healthJson.textContent = encodeJson(payload.health);
  voicesJson.textContent = encodeJson(payload.voices);

  const voices = parseVoiceList(payload.voices);
  voiceSelect.innerHTML = "";
  for (const voice of voices) {
    const option = document.createElement("option");
    option.value = voice.voice_id;
    option.textContent = `${voice.display_name} (${voice.voice_id})`;
    voiceSelect.append(option);
  }
  if (voices.some((item) => item.voice_id === payload.selected_voice_id)) {
    voiceSelect.value = payload.selected_voice_id;
  }

  if (payload.startup_error) {
    log(`Startup warning: ${payload.startup_error}`, "error");
    log("Bootstrap completed with warnings");
  } else {
    log("Engine sidecar started and handshake completed");
  }

  await pollRuntimeStatus();
}

async function bindActions(): Promise<void> {
  modelSelect.addEventListener("change", async () => {
    const result = await invoke<ModelUpdatePayload>("select_model", { model: modelSelect.value });
    renderPresetSpeakers(result.preset_speakers, result.selected_speaker);
    healthJson.textContent = encodeJson(result.health ?? {});
    log(result.message ?? "Model updated");
  });

  setHotkeyBtn.addEventListener("click", async () => {
    try {
      const result = await invoke<HotkeyResult>("set_hotkey", { hotkey: hotkeyInput.value });
      hotkeyPill.textContent = result.hotkey;
      hotkeyInput.value = result.hotkey;
      log(result.message ?? `Hotkey set to ${result.hotkey}`);
    } catch (error) {
      log(`Failed to update hotkey: ${String(error)}`, "error");
    }
  });

  speakerSelect.addEventListener("change", async () => {
    const result = await invoke<ModelUpdatePayload>("set_preset_speaker", {
      speakerId: speakerSelect.value,
    });
    renderPresetSpeakers(result.preset_speakers, result.selected_speaker);
    healthJson.textContent = encodeJson(result.health ?? {});
    log(result.message ?? "Speaker updated");
  });

  voiceSelect.addEventListener("change", async () => {
    await invoke("set_selected_voice", { voiceId: voiceSelect.value });
    log(`Selected voice_id=${voiceSelect.value}`);
  });

  [rateInput, pitchInput, volumeInput, chunkMaxInput].forEach((input) => {
    input.addEventListener("change", async () => {
      await applySpeakSettings();
      log("Speak settings updated");
    });
  });

  refreshBtn.addEventListener("click", async () => {
    await refreshHealthAndVoices();
    log("Health and voice list refreshed");
  });

  restartBtn.addEventListener("click", async () => {
    const response = await invoke<Record<string, unknown>>("restart_engine");
    await refreshHealthAndVoices();
    await pollRuntimeStatus();
    log(String(response.message ?? "Engine restarted"));
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
      }
      stopAllPlayback();
      return;
    }

    if (eventType === "JOB_DONE" || eventType === "JOB_ERROR") {
      if (jobId) {
        suppressedJobIds.delete(jobId);
      }
      resetPlaybackCursor();
    }
  });

  await listen<Record<string, unknown>>("voicereader:job-started", ({ payload }) => {
    const jobId = String(payload.job_id ?? "unknown");
    if (jobId !== "unknown") {
      suppressedJobIds.delete(jobId);
    }
    log(`job_started id=${jobId}`);
  });

  await listen<Record<string, unknown>>("voicereader:hotkey-updated", ({ payload }) => {
    const hotkey = String(payload.hotkey ?? "");
    if (hotkey) {
      hotkeyPill.textContent = hotkey;
      hotkeyInput.value = hotkey;
      log(`hotkey_updated=${hotkey}`);
    }
  });

  await listen<JobCancelRequestedPayload>("voicereader:job-cancel-requested", ({ payload }) => {
    const jobId = String(payload.job_id ?? "");
    if (jobId) {
      suppressedJobIds.add(jobId);
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
