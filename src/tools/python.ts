import { invoke } from "@tauri-apps/api/tauri";
import { h } from "../assistant-dom";
import type { AssistantSandboxStatus } from "../types";
import { createSandboxRow } from "./python-runtime";
import { timeoutSetting } from "./settings";
import type { ToolUiDefinition } from "./types";
export default {
  name: "run_python", order: 40, title: "Python", description: "Local calculations with an isolated Python interpreter.",
  async onEnable() {
    const current = await invoke<AssistantSandboxStatus>("assistant_tools_status");
    if (!current.installed || current.state !== "ready") await invoke("assistant_tools_install");
    const ready = await invoke<AssistantSandboxStatus>("assistant_tools_status");
    if (!ready.installed || ready.state !== "ready") throw new Error(ready.error || "The Python code runner is not ready. Try again from Python settings.");
  },
  createSettings(context) {
    const runtime = createSandboxRow();
    // Load status when the menu refreshes, not on application startup.
    context.onRefresh(() => { void runtime.refresh(); });
    return h("div", "assistant-tool-settings", timeoutSetting({ name: "run_python", label: "Python", defaultSeconds: 60, maxSeconds: 3600, legacyKey: "code_timeout_secs" }, context),
      runtime.element, h("p", "caption", "About 15 MB to download. No network or writable files; Python standard library only."));
  },
} satisfies ToolUiDefinition;
