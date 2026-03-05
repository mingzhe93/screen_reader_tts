import { emit, listen } from "@tauri-apps/api/event";
import { invoke } from "@tauri-apps/api/tauri";
import { appWindow, currentMonitor, LogicalPosition, PhysicalPosition } from "@tauri-apps/api/window";
import "./toolbar.css";

type ToolbarActionPayload = {
  action: "pause-toggle" | "skip-back" | "skip-forward" | "stop";
};

type ToolbarShowPayload = {
  job_id: string;
  source_window: string;
  rate: number;
};

type ToolbarPausePayload = {
  paused: boolean;
};

type SpeakRateResult = {
  ok: boolean;
  message: string;
  rate: number;
};

type ToolbarPosition = {
  x: number;
  y: number;
};

const TOOLBAR_POS_STORAGE_KEY = "voicereader.toolbar.position.v1";
const TOOLBAR_DEFAULT_MARGIN_PX = 20;

const app = document.querySelector<HTMLDivElement>("#toolbar-app");
if (!app) {
  throw new Error("Missing toolbar app root");
}

app.innerHTML = `
  <div id="toolbar-shell" class="toolbar-shell">
    <div id="toolbar-source" class="toolbar-source">Reading aloud...</div>
    <div class="toolbar-controls">
      <button id="toolbar-rate" class="toolbar-rate" type="button" title="Cycle rate">1x</button>
      <button id="toolbar-skip-back" class="toolbar-btn" type="button" aria-label="Skip back" title="Skip back">
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6h2v12H6zm3.5 6 8.5 6V6z"/></svg>
      </button>
      <button id="toolbar-pause" class="toolbar-btn toolbar-btn-primary" type="button" aria-label="Pause playback" title="Pause playback">
        <svg id="icon-pause" viewBox="0 0 24 24" aria-hidden="true"><path d="M6 19h4V5H6zm8-14v14h4V5z"/></svg>
        <svg id="icon-play" class="hidden" viewBox="0 0 24 24" aria-hidden="true"><path d="M8 5v14l11-7z"/></svg>
      </button>
      <button id="toolbar-stop" class="toolbar-btn toolbar-btn-stop" type="button" aria-label="Stop playback" title="Stop playback">
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M6 6h12v12H6z"/></svg>
      </button>
      <button id="toolbar-skip-fwd" class="toolbar-btn" type="button" aria-label="Skip forward" title="Skip forward">
        <svg viewBox="0 0 24 24" aria-hidden="true"><path d="M16 18h2V6h-2zm-10.5 0L14 12 5.5 6z"/></svg>
      </button>
    </div>
  </div>
`;

const shell = document.querySelector<HTMLDivElement>("#toolbar-shell")!;
const sourceLabel = document.querySelector<HTMLDivElement>("#toolbar-source")!;
const rateBtn = document.querySelector<HTMLButtonElement>("#toolbar-rate")!;
const skipBackBtn = document.querySelector<HTMLButtonElement>("#toolbar-skip-back")!;
const pauseBtn = document.querySelector<HTMLButtonElement>("#toolbar-pause")!;
const stopBtn = document.querySelector<HTMLButtonElement>("#toolbar-stop")!;
const skipForwardBtn = document.querySelector<HTMLButtonElement>("#toolbar-skip-fwd")!;
const pauseIcon = document.querySelector<SVGElement>("#icon-pause")!;
const playIcon = document.querySelector<SVGElement>("#icon-play")!;

let toolbarPaused = false;

function formatRate(rate: number): string {
  if (!Number.isFinite(rate)) {
    return "1x";
  }
  return `${rate.toFixed(2).replace(/\.?0+$/, "")}x`;
}

function setPauseVisual(paused: boolean): void {
  pauseIcon.classList.toggle("hidden", paused);
  playIcon.classList.toggle("hidden", !paused);
  const label = paused ? "Resume playback" : "Pause playback";
  pauseBtn.setAttribute("aria-label", label);
  pauseBtn.setAttribute("title", label);
}

function flashSkipBackNoop(): void {
  shell.classList.remove("skip-back-noop");
  void shell.offsetWidth;
  shell.classList.add("skip-back-noop");
  window.setTimeout(() => {
    shell.classList.remove("skip-back-noop");
  }, 220);
}

function dispatchAction(action: ToolbarActionPayload["action"]): void {
  void emit("voicereader:toolbar-action", { action } satisfies ToolbarActionPayload);
}

function readSavedToolbarPosition(): ToolbarPosition | null {
  try {
    const raw = window.localStorage.getItem(TOOLBAR_POS_STORAGE_KEY);
    if (!raw) {
      return null;
    }
    const parsed = JSON.parse(raw) as Record<string, unknown>;
    const x = Number(parsed.x);
    const y = Number(parsed.y);
    if (!Number.isFinite(x) || !Number.isFinite(y)) {
      return null;
    }
    return { x, y };
  } catch {
    return null;
  }
}

function saveToolbarPosition(position: ToolbarPosition): void {
  try {
    window.localStorage.setItem(TOOLBAR_POS_STORAGE_KEY, JSON.stringify(position));
  } catch {
    // Ignore storage write errors.
  }
}

async function setDefaultToolbarPosition(): Promise<void> {
  const monitor = await currentMonitor();
  const windowSize = await appWindow.outerSize();
  if (!monitor) {
    await appWindow.setPosition(
      new LogicalPosition(TOOLBAR_DEFAULT_MARGIN_PX, TOOLBAR_DEFAULT_MARGIN_PX),
    );
    return;
  }
  const x = Math.round(monitor.position.x + TOOLBAR_DEFAULT_MARGIN_PX);
  const y = Math.round(
    monitor.position.y + monitor.size.height - windowSize.height - TOOLBAR_DEFAULT_MARGIN_PX,
  );
  await appWindow.setPosition(new PhysicalPosition(x, y));
}

async function restoreToolbarPosition(): Promise<void> {
  const saved = readSavedToolbarPosition();
  if (saved) {
    await appWindow.setPosition(new LogicalPosition(saved.x, saved.y));
    return;
  }
  await setDefaultToolbarPosition();
}

async function cycleRate(): Promise<void> {
  const result = await invoke<SpeakRateResult>("cycle_speak_rate");
  rateBtn.textContent = formatRate(result.rate);
}

shell.addEventListener("mousedown", (event) => {
  const target = event.target as HTMLElement | null;
  if (target?.closest("button")) {
    return;
  }
  void appWindow.startDragging();
});

rateBtn.addEventListener("click", async () => {
  try {
    await cycleRate();
  } catch (error) {
    console.error("Failed to cycle rate", error);
  }
});

pauseBtn.addEventListener("click", () => {
  dispatchAction("pause-toggle");
});

stopBtn.addEventListener("click", () => {
  dispatchAction("stop");
});

skipForwardBtn.addEventListener("click", () => {
  dispatchAction("skip-forward");
});

skipBackBtn.addEventListener("click", () => {
  dispatchAction("skip-back");
  flashSkipBackNoop();
});

void listen<ToolbarShowPayload>("voicereader:toolbar-show", async ({ payload }) => {
  sourceLabel.textContent = payload.source_window || "Reading aloud...";
  rateBtn.textContent = formatRate(payload.rate);
  toolbarPaused = false;
  setPauseVisual(false);
  await appWindow.show();
});

void listen("voicereader:toolbar-hide", async () => {
  toolbarPaused = false;
  setPauseVisual(false);
  await appWindow.hide();
});

void listen<ToolbarPausePayload>("voicereader:toolbar-paused", ({ payload }) => {
  toolbarPaused = Boolean(payload.paused);
  setPauseVisual(toolbarPaused);
});

void listen<Record<string, unknown>>("voicereader:rate-updated", ({ payload }) => {
  const parsed = Number(payload.rate ?? NaN);
  if (!Number.isFinite(parsed)) {
    return;
  }
  rateBtn.textContent = formatRate(parsed);
});

void listen("voicereader:toolbar-skip-back-noop", () => {
  flashSkipBackNoop();
});

void appWindow.onMoved(({ payload }) => {
  saveToolbarPosition({ x: payload.x, y: payload.y });
});

async function initializeToolbarWindow(): Promise<void> {
  await restoreToolbarPosition();
  await appWindow.hide();
}

void initializeToolbarWindow();
