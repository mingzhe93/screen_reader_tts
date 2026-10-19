import { readStored, writeStored } from "../assistant-dom";
import { confirmAction } from "../confirm-dialog";
export const CONSENT_KEY = "voicereader.assistant.tools.consent";
export const TOOLS_CONSENT = "Enabled internet tools send queries to DuckDuckGo (or Brave), city names to Open-Meteo, and requests to the public pages you open. Python runs locally and downloads a 15 MB code runner only when you enable it. Continue?";
export async function consentToTools(): Promise<boolean> {
  if (readStored(CONSENT_KEY) === "true") return true;
  if (!(await confirmAction(TOOLS_CONSENT, "Continue"))) return false;
  writeStored(CONSENT_KEY, "true"); return true;
}
