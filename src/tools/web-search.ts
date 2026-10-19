import { invoke } from "@tauri-apps/api/tauri";
import { button, h } from "../assistant-dom";
import { confirmAction } from "../confirm-dialog";
import type { AssistantToolsSettings } from "../types";
import { timeoutSetting } from "./settings";
import type { ToolUiDefinition } from "./types";

const tool: ToolUiDefinition = {
  name: "web_search", order: 10, title: "Web search", description: "Search the web with DuckDuckGo or Brave.",
  createSettings(context) {
    let saved: AssistantToolsSettings | null = null;
    const provider = h("select"); provider.setAttribute("aria-label", "Search provider");
    for (const [value, title] of [["duckduckgo", "DuckDuckGo (no key)"], ["brave", "Brave Search (saved key)"]]) {
      const option = h("option", "", title); option.value = value; provider.appendChild(option);
    }
    const key = h("input"); key.type = "password"; key.autocomplete = "off"; key.placeholder = "Brave Search API key"; key.setAttribute("aria-label", "Brave Search API key");
    const save = button("Save key", "btn btn-sm", () => void context.run(async () => {
      if (!key.value.trim()) throw new Error("Enter a Brave Search API key.");
      await invoke("assistant_set_search_key", { provider: "brave", key: key.value.trim() }); key.value = ""; return "Brave key saved.";
    }));
    const remove = button("Remove key", "btn btn-sm btn-quiet-danger", () => void context.run(async () => {
      await invoke("assistant_set_search_key", { provider: "brave", key: null }); key.value = ""; return "Brave key removed. Search uses DuckDuckGo.";
    }));
    const test = button("Test key", "btn btn-sm", () => void context.run(async () => { await invoke("assistant_test_search_key"); return "Brave key works."; }));
    for (const control of [provider, key, save]) context.register(control);
    for (const control of [remove, test]) context.register(control, () => !!saved?.brave_key_set);
    context.onRefresh((settings) => { saved = settings; provider.value = settings.provider; });
    provider.addEventListener("change", () => {
      if (provider.value === "duckduckgo" && saved?.brave_key_set) void (async () => {
        // Switching provider deletes the key, which cannot be recovered from the app: ask first.
        if (!(await confirmAction("Switch to DuckDuckGo? This deletes your saved Brave Search API key.", "Delete key"))) { provider.value = "brave"; return; }
        await context.run(async () => { await invoke("assistant_set_search_key", { provider: "brave", key: null }); return "Search uses DuckDuckGo."; });
      })();
      else if (provider.value === "brave" && !saved?.brave_key_set) { context.notice("Save a Brave key below to use Brave Search."); key.focus(); }
    });
    return h("div", "assistant-tool-settings",
      h("label", "assistant-field-label", "Search provider", provider),
      h("p", "caption", "Brave is used when a key is saved. Choosing DuckDuckGo removes the saved key."), key,
      h("div", "assistant-tools-key", save, remove, test),
      h("p", "caption", "The Brave key is stored in plain text in the app’s settings file. It is never included in model messages."),
      timeoutSetting({ name: "web_search", label: "Search", defaultSeconds: 8, maxSeconds: 300, legacyKey: "search_timeout_secs" }, context));
  },
};
export default tool;
