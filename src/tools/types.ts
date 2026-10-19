import type { AssistantToolLimits, AssistantToolsSettings } from "../types";

export type SettingsControl = HTMLInputElement | HTMLSelectElement | HTMLButtonElement;
/** The menu supplies lifecycle/state; a tool owns its fields and settings commands. */
export type ToolSettingsContext = {
  run(action: () => Promise<string>): Promise<void>;
  register(control: SettingsControl, enabled?: () => boolean): void;
  onRefresh(listener: (settings: AssistantToolsSettings) => void): void;
  notice(text: string): void;
  limits(): AssistantToolLimits;
  setLimits(limits: AssistantToolLimits): void;
};
export type ToolUiDefinition = {
  name: string;
  order: number;
  title: string;
  description: string;
  createSettings(context: ToolSettingsContext): HTMLElement;
  onEnable?: () => Promise<void>;
};
export type TimeoutDefinition = {
  name: string;
  label: string;
  defaultSeconds: number;
  maxSeconds: number;
  // Compatibility for settings saved before per-tool modules existed.
  legacyKey?: Exclude<keyof AssistantToolLimits, "max_rounds" | "timeouts">;
};
