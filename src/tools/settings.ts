import { invoke } from "@tauri-apps/api/tauri";
import { button, h } from "../assistant-dom";
import type { ToolSettingsContext, TimeoutDefinition } from "./types";

/** New tools use timeouts[name]; existing flat settings continue to round-trip. */
export function timeoutSetting(definition: TimeoutDefinition, context: ToolSettingsContext): HTMLElement {
  const current = () => context.limits().timeouts?.[definition.name]
    ?? (definition.legacyKey ? context.limits()[definition.legacyKey] : undefined) ?? definition.defaultSeconds;
  const input = h("input"); input.type = "number"; input.min = "1"; input.max = String(definition.maxSeconds); input.step = "1"; input.value = String(current());
  input.setAttribute("aria-label", `${definition.label} timeout (seconds)`);
  const save = button("Save", "btn btn-sm", () => void context.run(async () => {
    const seconds = Number(input.value);
    if (!/^\d+$/.test(input.value) || !Number.isSafeInteger(seconds) || seconds < 1 || seconds > definition.maxSeconds) {
      input.setCustomValidity(`Enter a whole number between 1 and ${definition.maxSeconds}.`); input.reportValidity();
      throw new Error(`Enter a valid ${definition.label.toLowerCase()} timeout.`);
    }
    input.setCustomValidity("");
    const next = { ...context.limits(), timeouts: { ...context.limits().timeouts, [definition.name]: seconds } };
    if (definition.legacyKey) next[definition.legacyKey] = seconds;
    await invoke("assistant_set_tool_limits", { limits: next }); context.setLimits(next);
    return `${definition.label} timeout saved.`;
  }));
  input.addEventListener("input", () => input.setCustomValidity(""));
  context.register(input); context.register(save);
  context.onRefresh(() => { input.value = String(current()); });
  return h("div", "assistant-tool-limit", h("label", "assistant-field-label", `${definition.label} timeout (seconds)`, input), save);
}
