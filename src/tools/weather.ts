import { timeoutSetting } from "./settings";
import type { ToolUiDefinition } from "./types";
export default {
  name: "get_weather", order: 30, title: "Weather", description: "Current conditions and today's forecast for a city.",
  createSettings: (context) => timeoutSetting({ name: "get_weather", label: "Weather", defaultSeconds: 5, maxSeconds: 300, legacyKey: "weather_timeout_secs" }, context),
} satisfies ToolUiDefinition;
