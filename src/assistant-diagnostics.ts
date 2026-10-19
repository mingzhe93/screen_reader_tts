// Settings > Assistant diagnostics: the llama.cpp backend's paths, the server's state and
// process details, its log, and `llama-server --list-devices`. The API key is never shown;
// the backend already hides it in the log.

import { invoke } from "@tauri-apps/api/tauri";
import { listen } from "@tauri-apps/api/event";
import { copyToClipboard } from "./assistant-format";
import type { AssistantDiagnostics } from "./types";

type Options = {
  log: (message: string, level?: "info" | "error") => void;
  isPageActive: () => boolean;
};

function el<K extends keyof HTMLElementTagNameMap>(tag: K, className = "", text = ""): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text) node.textContent = text;
  return node;
}

function formatUptime(seconds: number | null): string {
  if (seconds === null) return "-";
  const minutes = Math.floor(seconds / 60);
  return minutes > 0 ? `${minutes} min ${seconds % 60} s` : `${seconds} s`;
}

function summary(diagnostics: AssistantDiagnostics): string {
  const server = diagnostics.server;
  const lines: [string, string][] = [
    ["Backend", `llama.cpp ${diagnostics.backend_version}`],
    ["Package", diagnostics.platform_asset ?? "not available on this platform"],
    ["Server binary", diagnostics.server_binary ?? "not installed"],
    ["Assistant folder", diagnostics.assistant_root],
    ["State", server.state],
    ["Model", server.model_id ?? "-"],
    ["Context", server.ctx_size ? `${server.ctx_size} tokens` : "-"],
    ["Device", server.device_label ?? "-"],
    ["PID", diagnostics.pid !== null ? String(diagnostics.pid) : "-"],
    ["Port", diagnostics.port !== null ? `127.0.0.1:${diagnostics.port}` : "-"],
    ["Uptime", formatUptime(diagnostics.uptime_secs)],
  ];
  if (server.error) lines.push(["Error", server.error]);
  const width = Math.max(...lines.map(([label]) => label.length));
  return lines.map(([label, value]) => `${label.padEnd(width)}  ${value}`).join("\n");
}

export async function initAssistantDiagnostics(options: Options): Promise<void> {
  const slot = document.querySelector<HTMLDivElement>("#assistant-diagnostics-slot");
  if (!slot) return;

  const card = el("article", "card diagnostics assistant-diagnostics");
  const header = el("div", "card-header");
  const intro = el("div");
  intro.append(
    el("h2", "card-title", "Assistant diagnostics (llama.cpp)"),
    el("p", "hint", "Backend, server and device details for troubleshooting the Assistant. The server's API key is never shown."),
  );
  const refreshBtn = el("button", "btn btn-sm", "Refresh");
  const devicesBtn = el("button", "btn btn-sm", "Check devices");
  const copyBtn = el("button", "btn btn-sm", "Copy");
  for (const button of [refreshBtn, devicesBtn, copyBtn]) button.type = "button";
  const actions = el("div", "action-row");
  actions.append(refreshBtn, devicesBtn, copyBtn);
  header.append(intro, actions);
  const summaryBox = el("pre", "json-box", "Loading...");
  const devicesHead = el("h3", "subhead is-hidden", "Devices (llama-server --list-devices)");
  const devicesBox = el("pre", "json-box is-hidden");
  const logHead = el("h3", "subhead", "Server log");
  const logBox = el("pre", "json-box assistant-log-box", "No server has run yet.");
  card.append(header, summaryBox, devicesHead, devicesBox, logHead, logBox);
  slot.replaceChildren(card);

  let latest = 0;
  async function refresh(): Promise<void> {
    const asked = ++latest;
    try {
      const diagnostics = await invoke<AssistantDiagnostics>("assistant_diagnostics");
      if (asked !== latest) return; // a newer refresh has started; its result wins
      summaryBox.textContent = summary(diagnostics);
      logBox.textContent = diagnostics.log || "No server has run yet.";
      logBox.scrollTop = logBox.scrollHeight;
    } catch (error) {
      if (asked !== latest) return;
      summaryBox.textContent = `Could not read the Assistant diagnostics: ${String(error)}`;
    }
  }

  refreshBtn.addEventListener("click", () => void refresh());
  devicesBtn.addEventListener("click", async () => {
    devicesBtn.disabled = true;
    devicesHead.classList.remove("is-hidden");
    devicesBox.classList.remove("is-hidden");
    devicesBox.textContent = "Running llama-server --list-devices...";
    try {
      devicesBox.textContent = await invoke<string>("assistant_list_devices");
    } catch (error) {
      devicesBox.textContent = String(error);
    } finally {
      devicesBtn.disabled = false;
    }
  });
  copyBtn.addEventListener("click", async () => {
    const parts = [summaryBox.textContent ?? ""];
    if (!devicesBox.classList.contains("is-hidden")) parts.push(`Devices:\n${devicesBox.textContent ?? ""}`);
    parts.push(`Server log:\n${logBox.textContent ?? ""}`);
    try {
      await copyToClipboard(parts.join("\n\n"));
      copyBtn.textContent = "Copied";
      window.setTimeout(() => (copyBtn.textContent = "Copy"), 1500);
    } catch (error) {
      options.log(`Copy failed: ${String(error)}`, "error");
    }
  });

  // Keep the panel current while Settings is open.
  await listen("assistant:status", () => {
    if (options.isPageActive()) void refresh();
  });
  new MutationObserver(() => {
    if (options.isPageActive()) void refresh();
  }).observe(document.querySelector('.page[data-page="settings"]')!, { attributes: true, attributeFilter: ["class"] });
  await refresh();
}
