// The Transcribe page: turns speech into a transcript with speaker labels, live from
// the microphone or from a recording.
// The models run in the Rust backend; this file owns the page, the model download row
// on the Models page, and the copy/export formats.

import { invoke } from "@tauri-apps/api/tauri";
import { listen } from "@tauri-apps/api/event";
import { open, save } from "@tauri-apps/api/dialog";
import { icon } from "./icons";
import type {
  AsrModelStatus,
  AudioInput,
  ModelDownloadProgressPayload,
  TranscribeStartPayload,
  TranscriptEventPayload,
  TranscriptTurn,
} from "./types";

const ASR_MODEL_ID = "parakeet_multitalker";
const AUDIO_EXTENSIONS = ["wav", "mp3", "m4a", "mp4", "aac", "flac", "ogg", "oga", "mkv", "mov"];
// One colour per speaker the diarizer can tell apart (`.speaker-chip.s1` to `.s8`).
const SPEAKER_COLOURS = 8;
const IDLE_HINT =
  "Record from the microphone, or choose a recording (you can also drop a file on this window). WAV, MP3, M4A, MP4, FLAC and OGG files work. Everything stays on this computer.";

type TranscribeOptions = {
  log: (message: string, level?: "info" | "error") => void;
  isPageActive: () => boolean;
};

type ExportFormat = "txt" | "md" | "srt";

function clock(seconds: number): string {
  const total = Math.max(0, Math.floor(seconds));
  const hours = Math.floor(total / 3600);
  const minutes = Math.floor((total % 3600) / 60);
  const secs = total % 60;
  const padded = String(secs).padStart(2, "0");
  return hours > 0 ? `${hours}:${String(minutes).padStart(2, "0")}:${padded}` : `${minutes}:${padded}`;
}

function srtClock(seconds: number): string {
  const millis = Math.max(0, Math.round(seconds * 1000));
  const part = (value: number, width: number) => String(value).padStart(width, "0");
  return `${part(Math.floor(millis / 3_600_000), 2)}:${part(Math.floor(millis / 60_000) % 60, 2)}:${part(
    Math.floor(millis / 1000) % 60,
    2,
  )},${part(millis % 1000, 3)}`;
}

function megabytes(bytes: number): number {
  return Math.round(bytes / (1024 * 1024));
}

function gigabytes(bytes: number): string {
  return (bytes / (1024 * 1024 * 1024)).toFixed(2);
}

function fileNameOf(path: string): string {
  return path.split(/[\\/]/).pop() ?? path;
}

/** Builds the text for "Copy" and "Export". `turns` must already be in reading order. */
export function formatTranscript(
  turns: TranscriptTurn[],
  speakerName: (speaker: number) => string,
  format: ExportFormat,
  title: string,
): string {
  if (format === "srt") {
    return turns
      .map(
        (turn, index) =>
          `${index + 1}\n${srtClock(turn.start_secs)} --> ${srtClock(
            Math.max(turn.end_secs, turn.start_secs + 0.5),
          )}\n${speakerName(turn.speaker)}: ${turn.text}\n`,
      )
      .join("\n");
  }
  if (format === "md") {
    const body = turns
      .map((turn) => `**${speakerName(turn.speaker)}** (${clock(turn.start_secs)})\n\n${turn.text}\n`)
      .join("\n");
    return `# Transcript: ${title}\n\n${body}`;
  }
  return turns.map((turn) => `[${clock(turn.start_secs)}] ${speakerName(turn.speaker)}: ${turn.text}`).join("\n") + "\n";
}

// Set by `initTranscribe`, so other pages can start the model download and share its progress display.
let startModelDownload: (() => Promise<boolean>) | null = null;

/** Whether the transcription model is available and downloaded. */
export function asrModelStatus(): Promise<AsrModelStatus> {
  return invoke<AsrModelStatus>("asr_model_status");
}

/**
 * Downloads the transcription model, joining a download that is already running.
 * Every `[data-asr-progress]` element on any page shows the progress.
 * Resolves to true when the model is downloaded afterwards.
 */
export function downloadAsrModel(): Promise<boolean> {
  return startModelDownload ? startModelDownload() : Promise.resolve(false);
}

export async function initTranscribe(options: TranscribeOptions): Promise<void> {
  const { log } = options;
  const root = document.querySelector<HTMLDivElement>("#transcribe-root")!;
  const modelSlot = document.querySelector<HTMLDivElement>("#asr-models-slot")!;
  const contentEl = document.querySelector<HTMLElement>("#content");

  root.innerHTML = `
    <div class="stack">
      <article class="card empty-state is-hidden" id="asr-setup">
        <span class="empty-icon">${icon("mic", 24)}</span>
        <p class="empty-text" id="asr-setup-text">Checking the transcription model...</p>
        <button class="btn btn-primary is-hidden" id="asr-setup-download-btn" type="button">Download model</button>
        <div class="asr-setup-progress">
          <progress class="download-progress is-hidden" data-asr-progress max="100" value="0"></progress>
          <p class="caption mono is-hidden" data-asr-progress-text></p>
        </div>
      </article>

      <article class="card is-hidden" id="asr-controls">
        <div class="transcribe-toolbar">
          <button class="btn btn-primary" id="asr-record-btn" type="button">${icon("mic", 16)}Record</button>
          <button class="btn" id="asr-choose-btn" type="button">${icon("file", 16)}Choose recording</button>
          <button class="btn btn-quiet-danger is-hidden" id="asr-stop-btn" type="button">${icon("stop", 16)}<span id="asr-stop-label">Stop</span></button>
          <span class="asr-live is-hidden" id="asr-live">
            <span class="rec-dot" aria-hidden="true"></span>
            <span class="level-meter" role="img" aria-label="Microphone level"><span id="asr-level"></span></span>
          </span>
          <span class="caption asr-file-name" id="asr-file-name"></span>
        </div>
        <div class="transcribe-options">
          <label class="caption" for="asr-microphone">Microphone</label>
          <select class="compact" id="asr-microphone"><option value="">System default</option></select>
          <label class="caption" for="asr-max-speakers">Speakers</label>
          <select class="compact" id="asr-max-speakers" title="How many different voices get their own label. Anyone beyond this number is still transcribed, under Unknown speaker."></select>
        </div>
        <progress class="download-progress is-hidden" id="asr-progress" max="100" value="0"></progress>
        <p class="hint asr-status" id="asr-status">${IDLE_HINT}</p>
      </article>

      <section class="is-hidden" id="asr-transcript-section" aria-label="Transcript">
        <div class="card-header">
          <h2 class="card-title">Transcript</h2>
          <div class="action-row">
            <button class="btn btn-sm" id="asr-copy-btn" type="button">${icon("copy", 14)}Copy</button>
            <button class="btn btn-sm" id="asr-export-btn" type="button">${icon("download", 14)}Transcription</button>
            <button class="btn btn-sm" id="asr-export-recording-btn" type="button" disabled>${icon("download", 14)}Recording &amp; transcription</button>
          </div>
        </div>
        <p class="caption is-hidden" id="asr-recording-export-hint">Download your recording before starting another transcription or closing VoiceReader.</p>
        <div class="speaker-names" id="asr-speakers"></div>
        <div class="transcript-list" id="asr-transcript"></div>
      </section>
    </div>
  `;

  modelSlot.innerHTML = `
    <article class="card model-row" id="asr-model-card">
      <div class="model-main">
        <div class="model-title">
          <h2 class="model-name">Transcription</h2>
          <span class="badge" id="asr-model-size">About 1 GB</span>
          <span class="badge">English</span>
        </div>
        <p class="model-desc">Speech to text with speaker labels, for the Transcribe page. Multitalker Parakeet with the Nemotron-3 diarizer; runs on the CPU.</p>
        <p class="model-status" id="asr-model-status">Checking transcription model status...</p>
      </div>
      <div class="model-action">
        <button class="btn btn-primary" id="asr-model-download-btn" type="button">Download model</button>
      </div>
      <div class="model-progress">
        <progress class="download-progress is-hidden" data-asr-progress max="100" value="0"></progress>
        <p class="caption mono is-hidden" data-asr-progress-text></p>
      </div>
    </article>
  `;
  modelSlot.classList.add("is-hidden");

  const setupCard = root.querySelector<HTMLElement>("#asr-setup")!;
  const setupText = root.querySelector<HTMLParagraphElement>("#asr-setup-text")!;
  const setupDownloadBtn = root.querySelector<HTMLButtonElement>("#asr-setup-download-btn")!;
  const controlsCard = root.querySelector<HTMLElement>("#asr-controls")!;
  const recordBtn = root.querySelector<HTMLButtonElement>("#asr-record-btn")!;
  const chooseBtn = root.querySelector<HTMLButtonElement>("#asr-choose-btn")!;
  const stopLabel = root.querySelector<HTMLSpanElement>("#asr-stop-label")!;
  const liveEl = root.querySelector<HTMLSpanElement>("#asr-live")!;
  const levelEl = root.querySelector<HTMLSpanElement>("#asr-level")!;
  const microphoneSelect = root.querySelector<HTMLSelectElement>("#asr-microphone")!;
  const stopBtn = root.querySelector<HTMLButtonElement>("#asr-stop-btn")!;
  const fileNameEl = root.querySelector<HTMLSpanElement>("#asr-file-name")!;
  const maxSpeakersSelect = root.querySelector<HTMLSelectElement>("#asr-max-speakers")!;
  const progressEl = root.querySelector<HTMLProgressElement>("#asr-progress")!;
  const statusEl = root.querySelector<HTMLParagraphElement>("#asr-status")!;
  const transcriptSection = root.querySelector<HTMLElement>("#asr-transcript-section")!;
  const speakersEl = root.querySelector<HTMLDivElement>("#asr-speakers")!;
  const transcriptEl = root.querySelector<HTMLDivElement>("#asr-transcript")!;
  const copyBtn = root.querySelector<HTMLButtonElement>("#asr-copy-btn")!;
  const exportBtn = root.querySelector<HTMLButtonElement>("#asr-export-btn")!;
  const exportRecordingBtn = root.querySelector<HTMLButtonElement>("#asr-export-recording-btn")!;
  const modelStatusEl = modelSlot.querySelector<HTMLParagraphElement>("#asr-model-status")!;
  const modelSizeEl = modelSlot.querySelector<HTMLSpanElement>("#asr-model-size")!;
  const modelDownloadBtn = modelSlot.querySelector<HTMLButtonElement>("#asr-model-download-btn")!;
  // The download can be started from either page, so both show its progress.
  const downloadBars = Array.from(document.querySelectorAll<HTMLProgressElement>("[data-asr-progress]"));
  const downloadTexts = Array.from(document.querySelectorAll<HTMLParagraphElement>("[data-asr-progress-text]"));

  let modelStatus: AsrModelStatus | null = null;
  let downloading = false;
  let running = false;
  // Where the running transcription gets its audio.
  let source: "file" | "microphone" = "file";
  let hearingAudio = false;
  let jobId: string | null = null;
  let recordingName = "";
  let completedJobId: string | null = null;
  let exporting = false;
  const turns = new Map<number, TranscriptTurn>();
  const rows = new Map<number, HTMLDivElement>();
  const speakerNames = new Map<number, string>();

  // People beyond the speaker limit are transcribed together under one label.
  const unknownSpeakers = new Set<number>();
  const defaultSpeakerName = (speaker: number): string =>
    unknownSpeakers.has(speaker) ? "Unknown speaker" : `Speaker ${speaker + 1}`;
  const speakerName = (speaker: number): string => speakerNames.get(speaker)?.trim() || defaultSpeakerName(speaker);
  const speakerClass = (speaker: number): string =>
    unknownSpeakers.has(speaker) ? "unknown" : `s${(speaker % SPEAKER_COLOURS) + 1}`;
  const orderedTurns = (): TranscriptTurn[] =>
    Array.from(turns.values()).sort((a, b) => a.start_secs - b.start_secs || a.id - b.id);

  function setStatus(message: string, level: "info" | "error" = "info"): void {
    statusEl.textContent = message;
    statusEl.classList.toggle("is-error", level === "error");
  }

  function renderModelState(): void {
    const supported = modelStatus?.supported ?? false;
    const downloaded = modelStatus?.downloaded ?? false;
    modelSlot.classList.toggle("is-hidden", !supported);
    setupCard.classList.toggle("is-hidden", downloaded);
    controlsCard.classList.toggle("is-hidden", !downloaded);
    setupDownloadBtn.classList.toggle("is-hidden", !supported || downloaded);
    setupDownloadBtn.disabled = downloading;
    modelDownloadBtn.disabled = downloading || downloaded;
    modelDownloadBtn.textContent = downloaded ? "Downloaded" : "Download model";
    modelStatusEl.classList.toggle("ok", downloaded);

    if (!modelStatus) {
      return;
    }
    const size = `${gigabytes(modelStatus.download_size_bytes)} GB`;
    modelSizeEl.textContent = `About ${size}`;
    if (!supported) {
      setupText.textContent = "Transcription is not available in this build.";
    } else if (downloading) {
      setupText.textContent = "Downloading the transcription model. You can keep using the app.";
      modelStatusEl.textContent = "Downloading...";
    } else if (downloaded) {
      modelStatusEl.textContent = "Ready. Open the Transcribe page to use it.";
    } else {
      setupText.textContent = `Transcription needs a one-time model download of about ${size}. It runs on this computer, so recordings are never uploaded.`;
      modelStatusEl.textContent = "Not downloaded.";
    }
  }

  function renderDownloadProgress(payload: ModelDownloadProgressPayload | null): void {
    for (const bar of downloadBars) {
      bar.classList.toggle("is-hidden", payload === null);
      if (payload && payload.total_bytes > 0) {
        bar.value = Math.min(100, (payload.downloaded_bytes / payload.total_bytes) * 100);
      }
    }
    for (const text of downloadTexts) {
      text.classList.toggle("is-hidden", payload === null);
      if (payload) {
        text.textContent = `${megabytes(payload.downloaded_bytes)} of ${megabytes(payload.total_bytes)} MB`;
      }
    }
  }

  async function refreshModelStatus(): Promise<void> {
    modelStatus = await invoke<AsrModelStatus>("asr_model_status");
    if (maxSpeakersSelect.options.length === 0 && modelStatus.supported) {
      for (let count = 1; count <= modelStatus.max_speakers_limit; count += 1) {
        const option = document.createElement("option");
        option.value = String(count);
        option.textContent = count === 1 ? "1 speaker" : `Up to ${count}`;
        maxSpeakersSelect.append(option);
      }
      maxSpeakersSelect.value = String(modelStatus.default_max_speakers);
    }
    renderModelState();
  }

  let runningDownload: Promise<boolean> | null = null;

  function downloadModel(): Promise<boolean> {
    runningDownload ??= runModelDownload().finally(() => {
      runningDownload = null;
    });
    return runningDownload;
  }
  startModelDownload = downloadModel;

  async function runModelDownload(): Promise<boolean> {
    downloading = true;
    renderModelState();
    log("Downloading the transcription model");
    try {
      const result = await invoke<{ ok: boolean; message: string }>("download_asr_model");
      log(result.message);
    } catch (error) {
      log(`Transcription model download failed: ${String(error)}`, "error");
      setupText.textContent = `The download did not finish: ${String(error)} Try again; it continues where it stopped.`;
    } finally {
      downloading = false;
      renderDownloadProgress(null);
      await refreshModelStatus().catch(() => renderModelState());
    }
    return modelStatus?.downloaded ?? false;
  }

  function renderSpeakers(): void {
    const speakers = Array.from(new Set(Array.from(turns.values(), (turn) => turn.speaker))).sort((a, b) => a - b);
    const shown = Array.from(speakersEl.querySelectorAll<HTMLInputElement>("input"), (input) => Number(input.dataset.speaker));
    if (speakers.length === shown.length && speakers.every((speaker, index) => speaker === shown[index])) {
      return;
    }
    speakersEl.replaceChildren(
      ...speakers.map((speaker) => {
        const label = document.createElement("label");
        label.className = "speaker-name";
        const chip = document.createElement("span");
        chip.className = `speaker-chip ${speakerClass(speaker)}`;
        chip.textContent = unknownSpeakers.has(speaker) ? "?" : String(speaker + 1);
        const input = document.createElement("input");
        input.type = "text";
        input.dataset.speaker = String(speaker);
        input.value = speakerNames.get(speaker) ?? "";
        input.placeholder = defaultSpeakerName(speaker);
        input.setAttribute("aria-label", `Name for ${defaultSpeakerName(speaker).toLowerCase()}`);
        input.addEventListener("input", () => {
          speakerNames.set(speaker, input.value);
          for (const chipEl of transcriptEl.querySelectorAll<HTMLSpanElement>(`.speaker-chip[data-speaker="${speaker}"]`)) {
            chipEl.textContent = speakerName(speaker);
          }
        });
        label.append(chip, input);
        return label;
      }),
    );
  }

  function upsertTurn(turn: TranscriptTurn): void {
    const nearBottom = contentEl
      ? contentEl.scrollHeight - contentEl.scrollTop - contentEl.clientHeight < 160
      : false;
    turns.set(turn.id, turn);
    if (turn.unknown) {
      unknownSpeakers.add(turn.speaker);
    }
    let row = rows.get(turn.id);
    if (!row) {
      row = document.createElement("div");
      row.className = "transcript-row";
      row.dataset.start = String(turn.start_secs);
      const time = document.createElement("span");
      time.className = "transcript-time";
      time.textContent = clock(turn.start_secs);
      const chip = document.createElement("span");
      chip.className = `speaker-chip ${speakerClass(turn.speaker)}`;
      chip.dataset.speaker = String(turn.speaker);
      chip.textContent = speakerName(turn.speaker);
      const text = document.createElement("p");
      text.className = "transcript-text";
      row.append(time, chip, text);
      rows.set(turn.id, row);
      // Turns nearly always arrive in order; when two people overlap, a turn can
      // start before one that is already shown.
      const later = Array.from(transcriptEl.children).find(
        (child) => Number((child as HTMLElement).dataset.start) > turn.start_secs,
      );
      transcriptEl.insertBefore(row, later ?? null);
      renderSpeakers();
    }
    row.querySelector<HTMLParagraphElement>(".transcript-text")!.textContent = turn.text;
    transcriptSection.classList.remove("is-hidden");
    renderExportActions();
    if (nearBottom && contentEl) {
      contentEl.scrollTop = contentEl.scrollHeight;
    }
  }

  function clearTranscript(): void {
    completedJobId = null;
    turns.clear();
    rows.clear();
    speakerNames.clear();
    unknownSpeakers.clear();
    transcriptEl.replaceChildren();
    speakersEl.replaceChildren();
    transcriptSection.classList.add("is-hidden");
    renderExportActions();
  }

  function renderExportActions(): void {
    root.querySelector("#asr-recording-export-hint")?.classList.toggle("is-hidden", source !== "microphone" || !completedJobId);
    copyBtn.disabled = turns.size === 0;
    exportBtn.disabled = turns.size === 0 || exporting;
    exportRecordingBtn.disabled = running || !completedJobId || exporting;
    exportRecordingBtn.title = running ? "Stop recording and wait for transcription to finish before downloading" : "Save a ZIP with the recording and transcription";
    recordBtn.disabled = exporting;
    chooseBtn.disabled = exporting;
  }

  function setRunning(next: boolean): void {
    running = next;
    renderExportActions();
    const live = next && source === "microphone";
    recordBtn.classList.toggle("is-hidden", next);
    chooseBtn.classList.toggle("is-hidden", next);
    microphoneSelect.disabled = next;
    maxSpeakersSelect.disabled = next;
    stopBtn.classList.toggle("is-hidden", !next);
    stopBtn.disabled = false;
    stopLabel.textContent = live ? "Stop recording" : "Stop";
    liveEl.classList.toggle("is-hidden", !live);
    levelEl.style.width = "0%";
    hearingAudio = false;
    if (!next) {
      jobId = null;
      progressEl.classList.add("is-hidden");
      void refreshMicrophones();
    }
  }

  async function refreshMicrophones(): Promise<void> {
    if (running) {
      return;
    }
    let inputs: AudioInput[] = [];
    try {
      inputs = await invoke<AudioInput[]>("list_audio_inputs");
    } catch (error) {
      log(`Could not list microphones: ${String(error)}`, "error");
    }
    const chosen = microphoneSelect.value;
    const defaultInput = inputs.find((input) => input.is_default);
    const fallback = document.createElement("option");
    fallback.value = "";
    fallback.textContent = defaultInput ? `System default (${defaultInput.name})` : "System default";
    microphoneSelect.replaceChildren(
      fallback,
      ...inputs.map((input) => {
        const option = document.createElement("option");
        option.value = input.name;
        option.textContent = input.name;
        return option;
      }),
    );
    // Keep the user's choice while that device is still plugged in.
    microphoneSelect.value = inputs.some((input) => input.name === chosen) ? chosen : "";
    recordBtn.title = inputs.length === 0 ? "No microphone was found" : "";
  }

  async function record(): Promise<void> {
    if (running || exporting || !modelStatus?.downloaded) {
      return;
    }
    clearTranscript();
    const now = new Date();
    const two = (value: number) => String(value).padStart(2, "0");
    recordingName = `recording-${now.getFullYear()}${two(now.getMonth() + 1)}${two(now.getDate())}-${two(now.getHours())}${two(now.getMinutes())}`;
    fileNameEl.textContent = "";
    source = "microphone";
    setRunning(true);
    setStatus("Loading the transcription model...");
    log("Recording from the microphone");
    try {
      const started = await invoke<TranscribeStartPayload>("transcribe_microphone_input", {
        device: microphoneSelect.value || null,
        maxSpeakers: Number(maxSpeakersSelect.value) || null,
      });
      if (running) {
        jobId = started.job_id;
      }
    } catch (error) {
      setRunning(false);
      setStatus(String(error), "error");
      log(`Recording failed: ${String(error)}`, "error");
    }
  }

  async function transcribe(path: string): Promise<void> {
    if (exporting) return;
    if (running) {
      setStatus("A transcription is already running. Stop it first to start another.");
      return;
    }
    if (!modelStatus?.downloaded) {
      return;
    }
    clearTranscript();
    recordingName = fileNameOf(path);
    fileNameEl.textContent = recordingName;
    source = "file";
    setRunning(true);
    progressEl.removeAttribute("value");
    progressEl.classList.remove("is-hidden");
    setStatus("Loading the transcription model...");
    log(`Transcribing ${recordingName}`);
    try {
      const started = await invoke<TranscribeStartPayload>("transcribe_audio_file", {
        path,
        maxSpeakers: Number(maxSpeakersSelect.value) || null,
      });
      // The job may already have finished (an unreadable file fails at once).
      if (running) {
        jobId = started.job_id;
      }
    } catch (error) {
      setRunning(false);
      setStatus(String(error), "error");
      log(`Transcription failed: ${String(error)}`, "error");
    }
  }

  function handleTranscriptEvent(payload: TranscriptEventPayload): void {
    if (!running || (jobId !== null && payload.job_id !== jobId)) {
      return;
    }
    switch (payload.kind) {
      case "loading":
        break;
      case "started":
        if (source === "microphone") {
          setStatus("Starting the microphone...");
        } else {
          setStatus(payload.total_secs ? `Transcribing 0:00 of ${clock(payload.total_secs)}` : "Transcribing...");
        }
        break;
      case "level":
        // The square root makes quiet speech visible on the meter.
        levelEl.style.width = `${Math.min(100, Math.sqrt(payload.level) * 100)}%`;
        if (!hearingAudio) {
          hearingAudio = true;
          setStatus("Listening. Speak now.");
        }
        break;
      case "turn":
        if (payload.turn) {
          upsertTurn(payload.turn);
        }
        break;
      case "progress":
        if (source === "microphone") {
          if (!stopBtn.disabled) {
            setStatus(`Recording ${clock(payload.processed_secs)}`);
          }
        } else if (payload.total_secs && payload.total_secs > 0) {
          progressEl.value = Math.min(100, (payload.processed_secs / payload.total_secs) * 100);
          setStatus(`Transcribing ${clock(payload.processed_secs)} of ${clock(payload.total_secs)}`);
        } else {
          setStatus(`Transcribing ${clock(payload.processed_secs)}`);
        }
        break;
      case "done":
        completedJobId = payload.job_id;
        transcriptSection.classList.remove("is-hidden");
        setRunning(false);
        setStatus(turns.size > 0 ? payload.message : `${payload.message} No speech was found.`);
        log(payload.message);
        break;
      case "cancelled":
        completedJobId = payload.job_id;
        transcriptSection.classList.remove("is-hidden");
        setRunning(false);
        setStatus(turns.size > 0 ? "Stopped. The transcript so far is kept below." : "Stopped.");
        log("Transcription stopped");
        break;
      case "error":
        setRunning(false);
        setStatus(payload.message, "error");
        log(`Transcription failed: ${payload.message}`, "error");
        break;
    }
  }

  function transcriptText(format: ExportFormat): string {
    return formatTranscript(orderedTurns(), speakerName, format, recordingName);
  }

  async function copyTranscript(): Promise<void> {
    const text = transcriptText("txt");
    try {
      await navigator.clipboard.writeText(text);
    } catch {
      // The clipboard API can be refused when the window is not focused.
      const area = document.createElement("textarea");
      area.value = text;
      area.style.position = "fixed";
      area.style.opacity = "0";
      document.body.append(area);
      area.select();
      document.execCommand("copy");
      area.remove();
    }
    const label = copyBtn.lastChild!;
    label.textContent = "Copied";
    window.setTimeout(() => {
      label.textContent = "Copy";
    }, 1600);
  }

  async function exportTranscript(): Promise<void> {
    if (exporting || turns.size === 0) return;
    const baseName = recordingName.replace(/\.[^.]+$/, "") || "transcript";
    const contents = { txt: transcriptText("txt"), md: transcriptText("md"), srt: transcriptText("srt") };
    exporting = true;
    renderExportActions();
    try {
      const path = await save({
        defaultPath: `${baseName}.txt`,
        filters: [
          { name: "Text", extensions: ["txt"] },
          { name: "Markdown", extensions: ["md"] },
          { name: "Subtitles", extensions: ["srt"] },
        ],
      });
      if (!path) return;
      const extension = path.split(".").pop()?.toLowerCase();
      const format: ExportFormat = extension === "md" || extension === "srt" ? extension : "txt";
      const result = await invoke<{ ok: boolean; message: string }>("save_text_file", { path, contents: contents[format] });
      log(result.message);
      setStatus(result.message);
    } catch (error) {
      log(`Export failed: ${String(error)}`, "error");
      setStatus(String(error), "error");
    } finally { exporting = false; renderExportActions(); }
  }

  async function exportRecordingAndTranscript(): Promise<void> {
    if (running || exporting || !completedJobId) return;
    const exportJobId = completedJobId;
    const contents = transcriptText("txt");
    const baseName = recordingName.replace(/\.[^.]+$/, "") || "recording";
    exporting = true;
    renderExportActions();
    try {
      const path = await save({ defaultPath: `${baseName}.zip`, filters: [{ name: "Recording and transcription", extensions: ["zip"] }] });
      if (!path) return;
      const result = await invoke<{ ok: boolean; message: string }>("export_recording_transcript", {
        jobId: exportJobId, path, contents, format: "txt",
      });
      log(result.message);
      setStatus(result.message);
    } catch (error) {
      log(`Recording export failed: ${String(error)}`, "error");
      setStatus(String(error), "error");
    } finally { exporting = false; renderExportActions(); }
  }

  setupDownloadBtn.addEventListener("click", () => void downloadModel());
  modelDownloadBtn.addEventListener("click", () => void downloadModel());
  chooseBtn.addEventListener("click", async () => {
    const picked = await open({
      multiple: false,
      filters: [{ name: "Audio or video", extensions: AUDIO_EXTENSIONS }],
    });
    if (typeof picked === "string") {
      await transcribe(picked);
    }
  });
  recordBtn.addEventListener("click", () => void record());
  window.addEventListener("focus", () => void refreshMicrophones());
  stopBtn.addEventListener("click", async () => {
    stopBtn.disabled = true;
    setStatus(source === "microphone" ? "Finishing the last words..." : "Stopping...");
    await invoke("cancel_transcription").catch((error) => log(`Stop failed: ${String(error)}`, "error"));
  });
  copyBtn.addEventListener("click", () => void copyTranscript());
  exportBtn.addEventListener("click", () => void exportTranscript());
  exportRecordingBtn.addEventListener("click", () => void exportRecordingAndTranscript());
  renderExportActions();

  setupCard.classList.remove("is-hidden");
  await listen<ModelDownloadProgressPayload>("voicereader:model-download", ({ payload }) => {
    if (payload.model !== ASR_MODEL_ID) {
      return;
    }
    if (payload.state === "progress") {
      renderDownloadProgress(payload);
    }
  });
  await listen<TranscriptEventPayload>("voicereader:transcript", ({ payload }) => handleTranscriptEvent(payload));
  await listen<string[]>("tauri://file-drop", ({ payload }) => {
    if (options.isPageActive() && modelStatus?.downloaded && payload.length > 0) {
      void transcribe(payload[0]);
    }
  });
  await refreshModelStatus();
  await refreshMicrophones();
}
