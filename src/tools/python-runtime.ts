import { invoke } from "@tauri-apps/api/tauri";
import { button, errorText, h, megabytes } from "../assistant-dom";
import type { AssistantSandboxStatus } from "../types";
import { consentToTools } from "./consent";
import { confirmAction } from "../confirm-dialog";

export function createSandboxRow() {
  const element = h("div", "model-row assistant-model-entry");
  let current: AssistantSandboxStatus | null = null;
  let busy = false;
  let installing = false;
  let failure = "";
  function paint(): void {
    const state = busy ? "Working…" : current?.state === "ready" ? "Installed" : current?.state === "installing" ? "Installing…" : "Not installed";
    const description = current ? `${current.version} · ${megabytes(current.download_size_bytes)} MB download${current.installed ? ` · ${megabytes(current.installed_size_bytes)} MB installed` : ""}` : "15 MB download";
    const downloading = installing || current?.state === "installing";
    const action = downloading
      ? button("Cancel download", "btn", () => void invoke("assistant_tools_cancel_install").catch(() => undefined))
      : button(current?.installed ? "Delete" : "Download", current?.installed ? "btn btn-quiet-danger" : "btn", () => void run());
    action.disabled = busy && !downloading;
    element.replaceChildren(h("div", "model-main", h("div", "model-title", h("h3", "model-name", "Isolated Python code runner"), h("span", "caption", description)),
      h("p", "model-status", failure || current?.error || state), h("p", "caption", "Python standard library in WebAssembly, with no network or writable files. Used by Assistant tools.")), h("div", "model-action", action));
  }
  async function refresh(): Promise<void> {
    try { current = await invoke<AssistantSandboxStatus>("assistant_tools_status"); } catch (error) { failure = errorText(error); }
    paint();
  }
  async function run(): Promise<void> {
    if (busy) return;
    if (current?.installed) { if (!(await confirmAction("Delete the isolated Python code runner? It can be downloaded again.", "Delete"))) return; }
    else if (!(await consentToTools())) return;
    installing = !current?.installed;
    busy = true; failure = ""; paint();
    try { await invoke(current?.installed ? "assistant_tools_delete" : "assistant_tools_install"); }
    catch (error) { failure = errorText(error); }
    finally { busy = false; installing = false; await refresh(); }
  }
  paint();
  return { element, refresh };
}
