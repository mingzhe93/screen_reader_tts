// The Assistant section of the Models page: installs the llama.cpp backend, downloads and
// deletes the Assistant models, and shows each model's memory estimate, its fit on this
// computer at 32K context and which one is recommended for this hardware.
// Downloads only start when the user presses a button, and the backend runs one at a time.

import { invoke } from "@tauri-apps/api/tauri";
import { listen } from "@tauri-apps/api/event";
import { button, errorText, h, megabytes } from "./assistant-dom";
import { fitDot, gigabytes, memoryBadge, memoryFit } from "./assistant-format";
import { ensureHardware, noteBackendInstalled, onHardware } from "./assistant-hardware";
import { makeConfirmButton } from "./confirm-button";
import type { AssistantDownloadProgressPayload, AssistantHardware, AssistantModelInfo, AssistantStatus } from "./types";

const TIER_LABELS = { small: "Small", mid: "Mid", "mid-high": "Mid-high", large: "Large" } as const;
/** The context the Models page judges memory fit at: the page's default. */
const FIT_CTX = 32768;

type Options = {
  log: (message: string, level?: "info" | "error") => void;
};

export async function initAssistantModels(options: Options): Promise<void> {
  const slot = document.querySelector<HTMLDivElement>("#assistant-models-slot");
  if (!slot) return;
  let status: AssistantStatus | null = null;
  let hardware: AssistantHardware | null = null;
  let progress: AssistantDownloadProgressPayload | null = null;
  // Events and replies can arrive out of order; a reply only applies if nothing newer has.
  let generation = 0;
  let cancelRequested = false;
  /** Errors per row ("backend" or a model id), kept until that row's next action. */
  const errors = new Map<string, string>();

  /** Progress elements of the download currently shown, updated in place by the progress event. */
  let progressBar: HTMLProgressElement | null = null;
  let progressText: HTMLElement | null = null;

  function progressBlock(): HTMLElement {
    const bar = h("progress", "download-progress");
    bar.max = 100;
    progressBar = bar;
    progressText = h("p", "caption mono");
    const block = h("div", "model-progress", bar, progressText);
    paintProgress();
    return block;
  }

  function paintProgress(): void {
    if (!progressBar || !progressText) return;
    const current = status?.download;
    if (!current || !progress || progress.kind !== current.kind || progress.id !== current.id) {
      progressBar.removeAttribute("value");
      progressText.textContent = "Starting download…";
      return;
    }
    if (progress.total > 0) {
      const percent = Math.min(100, (progress.downloaded / progress.total) * 100);
      progressBar.value = percent;
      progressText.textContent = `${progress.file}: ${megabytes(progress.downloaded)} of ${megabytes(progress.total)} MB (${Math.floor(percent)}%)`;
    } else {
      progressBar.removeAttribute("value");
      progressText.textContent = `${progress.file}: ${megabytes(progress.downloaded)} MB`;
    }
  }

  function statusLine(text: string, ok: boolean, rowKey: string): HTMLElement {
    const error = errors.get(rowKey);
    return h("p", `model-status${ok && !error ? " ok" : ""}`, error ?? text);
  }

  function backendRow(): HTMLElement {
    const current = status!;
    const installing = current.download?.kind === "backend";
    const title = h(
      "div",
      "model-title",
      h("h3", "model-name", "llama.cpp backend"),
      h("span", "caption", `(${current.backend_version}, ${megabytes(current.backend_download_bytes)} MB)`),
    );
    const text = current.backend_installed
      ? "Installed"
      : installing
        ? "Installing…"
        : "Not installed. The Assistant runs its models with llama.cpp; install it first.";
    const row = h(
      "div",
      "model-row assistant-model-entry assistant-backend-entry",
      h("div", "model-main", title, statusLine(text, current.backend_installed, "backend")),
    );
    if (!current.backend_installed) {
      const action = installing
        ? button("Cancel", "btn btn-quiet-danger", () => void cancelDownload())
        : button("Install", "btn btn-primary", () => void runDownload("backend", "assistant_install_backend"));
      if (!installing) action.disabled = !!current.download;
      row.append(h("div", "model-action", action));
      if (installing) row.append(progressBlock());
    }
    return row;
  }

  function modelRow(model: AssistantModelInfo): HTMLElement {
    const current = status!;
    const downloading = current.download?.kind === "model" && current.download.id === model.id;
    const loadedHere = current.server.model_id === model.id && current.server.state !== "stopped";
    const title = h(
      "div",
      "model-title",
      h("h3", "model-name", model.label),
      h("span", "badge", TIER_LABELS[model.tier] ?? model.tier),
      h("span", "badge", `${gigabytes(model.download_bytes)} GB`),
      memoryBadge(model),
    );
    if (hardware && model.memory) title.append(fitDot(memoryFit(model.memory, FIT_CTX, hardware), "At 32K context"));
    if (hardware && hardware.recommended_tier === model.tier) {
      const badge = h("span", "badge badge-accent badge-hint", "Recommended");
      badge.title = hardware.reason;
      badge.setAttribute("aria-label", `Recommended: ${hardware.reason}`);
      badge.tabIndex = 0;
      title.append(badge);
    }
    const text = downloading
      ? "Downloading…"
      : model.installed
        ? loadedHere
          ? "Downloaded. Loaded now; deleting unloads it first."
          : "Downloaded."
        : current.backend_installed
          ? "Not downloaded."
          : "Not downloaded. Install the llama.cpp backend first.";
    const row = h(
      "div",
      "model-row assistant-model-entry",
      h("div", "model-main", title, statusLine(text, model.installed, model.id)),
    );
    let action: HTMLButtonElement;
    if (model.installed) {
      action = h("button", "btn btn-quiet-danger");
      action.type = "button";
      makeConfirmButton(action, {
        label: "Delete",
        confirmLabel: `Delete ${gigabytes(model.download_bytes)} GB?`,
        busyLabel: "Deleting...",
        onConfirm: () => deleteModel(model),
      });
      action.disabled = downloading;
    } else if (downloading) {
      action = button("Cancel", "btn btn-quiet-danger", () => void cancelDownload());
    } else {
      action = button("Download", "btn", () => void runDownload(model.id, "assistant_download_model", { modelId: model.id }));
      action.disabled = !current.backend_installed || !!current.download;
      action.title = !current.backend_installed
        ? "Install the llama.cpp backend first"
        : current.download
          ? "Another download is running"
          : `Download ${gigabytes(model.download_bytes)} GB`;
    }
    row.append(h("div", "model-action", action));
    if (downloading) row.append(progressBlock());
    return row;
  }

  function render(): void {
    progressBar = null;
    progressText = null;
    const show = !!status?.platform_supported;
    slot!.classList.toggle("is-hidden", !show);
    if (!show || !status) return;
    // The page's "Assistant" group heading (main.ts) titles this card, so it has none of its own.
    const header = h(
      "div",
      "model-main",
      h(
        "p",
        "model-desc",
        "Language models for the Assistant page, run on this computer with llama.cpp. Downloads only start when you press a button. Hover over Memory for the estimated memory, and over the dot for how well it fits this computer at 32K context.",
      ),
    );
    const planned = h(
      "p",
      "caption assistant-models-planned",
      "Custom GGUF models from Hugging Face and API endpoints are planned for a later version.",
    );
    slot!.replaceChildren(
      h("article", "card assistant-models-card", header, backendRow(), ...status.models.map(modelRow), planned),
    );
  }

  function apply(next: AssistantStatus): void {
    status = next;
    if (!next.download) progress = null;
    noteBackendInstalled(next.backend_installed);
    render();
  }

  async function refresh(): Promise<void> {
    const asked = ++generation;
    try {
      const next = await invoke<AssistantStatus>("assistant_status");
      if (asked === generation) apply(next);
    } catch (error) {
      options.log(`Assistant model status failed: ${errorText(error)}`, "error");
    }
  }

  /** Runs an install or download to its end. Status events keep the rows current meanwhile. */
  async function runDownload(rowKey: string, command: string, args?: Record<string, unknown>): Promise<void> {
    errors.delete(rowKey);
    cancelRequested = false;
    progress = null;
    try {
      const asked = ++generation;
      const next = await invoke<AssistantStatus>(command, args);
      if (asked === generation) apply(next);
      else await refresh();
    } catch (error) {
      if (!cancelRequested) {
        const message = `Download failed: ${errorText(error)}`;
        errors.set(rowKey, message);
        options.log(message, "error");
      }
      await refresh();
    }
  }

  async function cancelDownload(): Promise<void> {
    cancelRequested = true;
    try {
      await invoke("assistant_cancel_download");
    } catch (error) {
      options.log(`Cancel failed: ${errorText(error)}`, "error");
    }
  }

  async function deleteModel(model: AssistantModelInfo): Promise<void> {
    try {
      errors.delete(model.id);
      const asked = ++generation;
      const next = await invoke<AssistantStatus>("assistant_delete_model", { modelId: model.id });
      if (asked === generation) apply(next);
      else await refresh();
      options.log(`Deleted the Assistant model ${model.label}`);
    } catch (error) {
      errors.set(model.id, errorText(error));
      options.log(`Deleting ${model.label} failed: ${errorText(error)}`, "error");
      render();
    }
  }

  onHardware((next) => {
    hardware = next;
    render();
  });
  await listen<AssistantStatus>("assistant:status", ({ payload }) => {
    generation += 1;
    apply(payload);
  });
  await listen<AssistantDownloadProgressPayload>("assistant:download-progress", ({ payload }) => {
    progress = payload;
    if (status && !status.download) {
      // The progress event can beat the status event that announces the download.
      status = { ...status, download: { kind: payload.kind, id: payload.id } };
      render();
      return;
    }
    paintProgress();
  });
  ensureHardware(options.log);
  await refresh();
}
