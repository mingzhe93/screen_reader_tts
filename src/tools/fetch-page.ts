import { timeoutSetting } from "./settings";
import type { ToolUiDefinition } from "./types";
export default {
  name: "fetch_page", order: 20, title: "Read web pages", description: "Read links you share or that a search returns.",
  createSettings: (context) => timeoutSetting({ name: "fetch_page", label: "Page", defaultSeconds: 10, maxSeconds: 300, legacyKey: "fetch_timeout_secs" }, context),
} satisfies ToolUiDefinition;
